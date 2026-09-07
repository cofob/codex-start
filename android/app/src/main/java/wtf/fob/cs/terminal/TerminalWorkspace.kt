package wtf.fob.cs.terminal

import android.util.Base64
import androidx.compose.runtime.*
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import org.json.JSONObject
import wtf.fob.cs.data.*
import java.util.UUID

/** The host owns the PTYs. Closing this model never closes a remote shell. */
internal class TerminalWorkspace(
    private val repo: RemoteRepository,
    private val server: String,
) : ViewModel() {
    var terminals by mutableStateOf<List<JSONObject>>(emptyList())
        private set
    var selected by mutableStateOf("")
    var busy by mutableStateOf(false)
        private set
    var failure by mutableStateOf("")
    private val writers = mutableMapOf<String, TerminalInput>()

    suspend fun call(
        method: String,
        params: JSONObject = JSONObject(),
    ): JSONObject = repo.request(server, "terminal/$method", params) as JSONObject

    fun refresh() =
        action {
            val data = call("list").getJSONArray("data")
            terminals = (0 until data.length()).map { data.getJSONObject(it) }
            if (terminals.none { it.text("id") == selected }) selected = terminals.firstOrNull()?.text("id").orEmpty()
            writers.keys
                .toList()
                .filter { id -> terminals.none { it.text("id") == id } }
                .forEach { writers.remove(it)?.close() }
        }

    fun open(
        target: String,
        session: String,
        cwd: String,
        command: String,
    ) = action {
        val params = obj("terminalId" to UUID.randomUUID().toString(), "target" to target, "sessionId" to session, "command" to command)
        if (cwd.isNotBlank()) params.put("cwd", cwd)
        val terminal = call("open", params)
        terminals = terminals + terminal
        selected = terminal.text("id")
    }

    fun rename(
        id: String,
        name: String,
    ) = action {
        val updated = call("rename", obj("terminalId" to id, "name" to name))
        terminals = terminals.map { if (it.text("id") == id) updated else it }
    }

    fun close(id: String) =
        action {
            call("close", obj("terminalId" to id))
            writers.remove(id)?.close()
            terminals = terminals.filterNot { it.text("id") == id }
            if (selected == id) selected = terminals.lastOrNull()?.text("id").orEmpty()
        }

    fun status(
        id: String,
        result: JSONObject,
    ) {
        val existing = terminals.firstOrNull { it.text("id") == id } ?: return
        if (existing.text("status") == result.text("status") && existing.text("error") == result.text("error")) return
        val changed =
            JSONObject(existing.toString())
                .put("status", result.text("status"))
                .put("exitCode", result.opt("exitCode"))
                .put("error", result.text("error"))
        terminals = terminals.map { if (it.text("id") == id) changed else it }
    }

    fun writer(id: String): TerminalInput = writers.getOrPut(id) { TerminalInput(id, viewModelScope, ::call) }

    private fun action(block: suspend () -> Unit) =
        viewModelScope.launch {
            if (busy) return@launch
            busy = true
            failure = ""
            try {
                block()
            } catch (
                error: CancellationException,
            ) {
                throw error
            } catch (error: Exception) {
                failure = error.message ?: "Terminal request failed"
            } finally {
                busy = false
            }
        }

    override fun onCleared() {
        writers.values.forEach { it.close() }
    }
}

/** One bounded FIFO per shell. Never replay keystrokes after an unknown delivery result. */
internal class TerminalInput(
    private val id: String,
    private val scope: CoroutineScope,
    private val request: suspend (String, JSONObject) -> JSONObject,
) {
    var ready by mutableStateOf(false)
        private set
    var failure by mutableStateOf("")
        private set
    private var generation = 0
    private var offset = 0L
    private val delivery = Mutex()
    private val pending = Channel<Pair<Int, ByteArray>>(64)
    private val job =
        scope.launch {
            for ((version, bytes) in pending) {
                if (!ready || generation != version) continue
                try {
                    var start = 0
                    while (start < bytes.size && ready && generation == version) {
                        val end = minOf(start + 2048, bytes.size)
                        val chunk = Base64.encodeToString(bytes.copyOfRange(start, end), Base64.NO_WRAP)
                        delivery.withLock {
                            if (ready && generation == version) {
                                offset =
                                    request("write", obj("terminalId" to id, "offset" to offset, "dataBase64" to chunk)).getLong("offset")
                            }
                        }
                        start = end
                    }
                } catch (error: CancellationException) {
                    throw error
                } catch (error: Exception) {
                    pause("Input stopped. Delivery is unknown. Check the terminal, then reconnect input.")
                }
            }
        }

    init {
        connect()
    }

    fun connect() =
        scope.launch {
            ready = false
            generation++
            try {
                delivery.withLock {
                    offset = request("status", obj("terminalId" to id)).getLong("inputOffset")
                    failure = ""
                    ready = true
                }
            } catch (
                error: CancellationException,
            ) {
                throw error
            } catch (error: Exception) {
                failure = error.message ?: "Input connection failed"
            }
        }

    fun send(base64: String) {
        if (!ready) return
        val bytes = runCatching { Base64.decode(base64, Base64.DEFAULT) }.getOrNull() ?: return
        if (bytes.size > 64 * 1024 || pending.trySend(generation to bytes).isFailure) {
            pause("Input buffer is full. Some input was not sent. Check the terminal before you reconnect input.")
        }
    }

    private fun pause(message: String) {
        ready = false
        generation++
        while (pending.tryReceive().isSuccess) { /* Drop unsent input; never send it later. */ }
        failure = message
    }

    fun close() {
        ready = false
        pending.close()
        job.cancel()
    }
}
