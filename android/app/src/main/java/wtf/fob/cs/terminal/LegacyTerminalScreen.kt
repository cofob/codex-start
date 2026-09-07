package wtf.fob.cs.terminal

import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import java.util.UUID
import kotlin.time.Duration.Companion.milliseconds

@Composable internal fun LegacyTerminalScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    var command by remember { mutableStateOf("") }
    var process by remember { mutableStateOf("") }
    var outputProcess by remember { mutableStateOf("") }
    var ready by remember { mutableStateOf(false) }
    var cols by remember { mutableIntStateOf(80) }
    var rows by remember { mutableIntStateOf(24) }
    var status by remember { mutableStateOf("Start a shell or enter a command.") }
    val sessionId = session.getString("id")
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    // One consumer keeps keystrokes and paste chunks in order on the wire.
    val input = remember { Channel<Pair<String, String>>(Channel.UNLIMITED) }
    val exitFocus = remember { FocusRequester() }
    val inputMode = androidx.compose.ui.platform.LocalInputModeManager.current
    val terminal =
        remember {
            TerminalView(
                context,
                onReady = { c, r ->
                    cols = c
                    rows = r
                    ready = true
                },
                onInput = { bytes -> if (process.isNotEmpty()) input.trySend(process to bytes) },
                onResize = { c, r ->
                    cols = c
                    rows = r
                },
                onExitKeyboardFocus = {
                    inputMode.requestInputMode(androidx.compose.ui.input.InputMode.Keyboard)
                    exitFocus.requestFocus()
                },
            )
        }
    DisposableEffect(terminal) {
        onDispose {
            input.close()
            terminal.close()
        }
    }
    LaunchedEffect(input) {
        for ((id, bytes) in input) {
            if (id == process) {
                runCatching {
                    repo.rpc(server, sessionId, "command/exec/write", obj("processId" to id, "deltaBase64" to bytes))
                }.onFailure(repo::report)
            }
        }
    }
    LaunchedEffect(process, cols, rows) {
        if (process.isNotEmpty()) {
            delay(150.milliseconds) // Coalesce keyboard and window resize events.
            runCatching {
                repo.rpc(
                    server,
                    sessionId,
                    "command/exec/resize",
                    obj("processId" to process, "size" to obj("cols" to cols, "rows" to rows)),
                )
            }.onFailure(repo::report)
        }
    }
    LaunchedEffect(server, sessionId) {
        repo.events.collect { event ->
            val params = event.message.optJSONObject("params")
            if (outputProcess.isNotEmpty() &&
                event.server == server &&
                event.session == sessionId &&
                event.message.text("method") == "command/exec/outputDelta" &&
                params?.text("processId") == outputProcess
            ) {
                // Keep raw bytes across chunks; xterm.js handles split UTF-8 and escape codes.
                terminal.writeBase64(params.text("deltaBase64"))
            }
        }
    }

    fun start() {
        val id = UUID.randomUUID().toString()
        process = id
        outputProcess = id
        status = "Running"
        terminal.resetTerminal()
        terminal.setActive(true)
        terminal.requestFocus()
        val argv = if (command.isBlank()) listOf("sh", "-l") else listOf("sh", "-lc", command)
        scope.launch {
            try {
                val result =
                    repo.rpc(
                        server,
                        sessionId,
                        "command/exec",
                        obj(
                            "command" to JSONArray(argv),
                            "cwd" to session.getString("executionCwd"),
                            "processId" to id,
                            "tty" to true,
                            "streamStdin" to true,
                            "streamStdoutStderr" to true,
                            "disableOutputCap" to true,
                            "disableTimeout" to true,
                            "env" to obj("TERM" to "xterm-256color", "COLORTERM" to "truecolor"),
                            "size" to obj("cols" to cols, "rows" to rows),
                        ),
                    )
                status = "Process exited: ${result.optInt("exitCode")}"
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                status = error.message ?: "Terminal connection failed"
                repo.report(error)
            } finally {
                // A command must not remain hidden after the terminal view is closed.
                // Also stop it if a request failed with an unknown delivery result.
                withContext(NonCancellable) {
                    withTimeoutOrNull(5000.milliseconds) {
                        runCatching { repo.rpc(server, sessionId, "command/exec/terminate", obj("processId" to id)) }
                    }
                }
                if (currentCoroutineContext().isActive) {
                    terminal.setActive(false)
                    process = ""
                }
            }
        }
    }
    Column(Modifier.fillMaxSize()) {
        OutlinedTextField(
            command,
            { command = it },
            label = { Text("Command (optional)") },
            singleLine = true,
            enabled = process.isEmpty(),
            modifier = Modifier.fillMaxWidth(),
        )
        Row {
            Button(enabled = ready && process.isEmpty(), onClick = ::start) { Text(if (command.isBlank()) "Start shell" else "Run") }
            TextButton(enabled = process.isNotEmpty(), onClick = {
                scope.launch {
                    runCatching {
                        repo.rpc(
                            server,
                            sessionId,
                            "command/exec/terminate",
                            obj("processId" to process),
                        )
                    }.onFailure(repo::report)
                }
            }) { Text("Terminate") }
            TextButton(modifier = Modifier.focusRequester(exitFocus), enabled = ready, onClick = {
                terminal.requestFocus()
                terminal.focusTerminal()
            }) { Text("Keyboard") }
        }
        Text(if (ready) status else "Loading terminal…", style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(4.dp))
        AndroidView(factory = { terminal }, modifier = Modifier.fillMaxWidth().weight(1f))
        Row(Modifier.fillMaxWidth().horizontalScroll(rememberScrollState())) {
            listOf(
                "Esc" to "\u001b",
                "Tab" to "\t",
                "Ctrl-C" to "\u0003",
                "Ctrl-D" to "\u0004",
                "↑" to "\u001b[A",
                "↓" to "\u001b[B",
                "←" to "\u001b[D",
                "→" to "\u001b[C",
            ).forEach { (label, value) ->
                TextButton(enabled = process.isNotEmpty(), onClick = { terminal.sendKey(value) }) { Text(label) }
            }
        }
    }
}
