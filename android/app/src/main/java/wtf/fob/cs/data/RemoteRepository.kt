package wtf.fob.cs.data

import android.app.Application
import androidx.core.content.edit
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import org.json.JSONArray
import org.json.JSONObject
import org.json.JSONTokener
import wtf.fob.cs.nativeclient.NativeClient
import wtf.fob.cs.nativeclient.discoverHost
import java.util.concurrent.ConcurrentHashMap
import kotlin.time.Duration.Companion.milliseconds

fun JSONObject.text(
    key: String,
    fallback: String = "",
): String = if (isNull(key)) fallback else optString(key, fallback)

fun JSONArray.objects(): List<JSONObject> = (0 until length()).mapNotNull { optJSONObject(it) }

fun obj(vararg entries: Pair<String, Any?>): JSONObject = JSONObject().apply { entries.forEach { (k, v) -> put(k, v ?: JSONObject.NULL) } }

fun requestIdKey(value: Any?): String = if (value is String) JSONObject.quote(value) else value.toString()

fun inputNotificationTag(
    server: String,
    session: String,
    requestId: Any?,
): String = "$server/$session/${requestIdKey(requestId)}"

data class RemoteServer(
    val id: String,
    val name: String,
    val status: String,
    val fingerprint: String,
    val capabilities: Set<String> = emptySet(),
)

data class RemoteEvent(
    val server: String,
    val session: String,
    val sequence: Long,
    val message: JSONObject,
)

fun connectionStatus(message: String?): String {
    val detail = message.orEmpty().removePrefix("reason=")
    return when {
        detail.contains("timed out", true) -> "Connection timed out"
        detail.contains("identity", true) || detail.contains("fingerprint", true) -> "Host identity changed. Pair again"
        detail.contains("authenticated", true) || detail.contains("revoked", true) -> "Device access was rejected"
        detail.contains("closed", true) || detail.contains("connection lost", true) -> "Host closed the connection"
        detail.contains("IO error", true) || detail.contains("refused", true) -> "Host is unavailable"
        else ->
            detail
                .lineSequence()
                .first()
                .take(160)
                .ifBlank { "Connection lost" }
    }
}

class CodexApplication : Application() {
    val repository by lazy { RemoteRepository(this) }

    override fun onCreate() {
        super.onCreate()
        repository.start()
    }
}

class RemoteRepository(
    private val app: Application,
) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val store = ConnectionStore(app)
    private val labels = app.getSharedPreferences("server-labels", android.content.Context.MODE_PRIVATE)
    private val cache =
        app.filesDir
            .resolve("remote-cache")
            .apply { mkdirs() }
            .absolutePath
    private val clients = ConcurrentHashMap<String, NativeClient>()
    private val jobs = ConcurrentHashMap<String, Job>()
    private val refreshJobs = ConcurrentHashMap<String, Job>()
    private val requestSlots = ConcurrentHashMap<String, Semaphore>()
    private val reconnectSignals = ConcurrentHashMap<String, Channel<Unit>>()
    private var foreground: Job? = null
    val data = RemoteDataCache(scope, load = ::loadQuery)
    val catalog by lazy { ProtocolCatalog(app) }
    private val drafts = LinkedHashMap<RemoteQuery, String>()

    fun draft(
        server: String,
        session: String,
        thread: String,
    ): String =
        synchronized(drafts) {
            drafts[RemoteQuery(server, session, thread)].orEmpty()
        }

    fun keepDraft(
        server: String,
        session: String,
        thread: String,
        text: String,
    ) = synchronized(drafts) {
        val key = RemoteQuery(server, session, thread)
        if (text.isEmpty()) drafts.remove(key) else drafts[key] = text.take(128 * 1024)
        while (drafts.size > 24) drafts.remove(drafts.keys.first())
    }

    private val network =
        AndroidNetwork(app) {
            clients.values.forEach { client -> scope.launch { runCatching { client.networkChanged() } } }
            reconnectSignals.values.forEach { it.trySend(Unit) }
        }
    private val mutableServers = MutableStateFlow<List<RemoteServer>>(emptyList())
    val servers = mutableServers.asStateFlow()
    private val mutableEvents = MutableSharedFlow<RemoteEvent>(extraBufferCapacity = 8)
    val events = mutableEvents.asSharedFlow()
    private val mutableError = MutableStateFlow<String?>(null)
    val error = mutableError.asStateFlow()
    private val pending = ConcurrentHashMap<String, RemoteEvent>()

    fun pendingApprovals(
        server: String,
        session: String,
    ): List<RemoteEvent> =
        pending.values.filter {
            it.server == server &&
                it.session == session
        }

    fun clearError() {
        mutableError.value = null
    }

    /** Wake the existing reconnect loop. Never replace a live client or replay an RPC. */
    fun retryConnection(server: String) {
        if (servers.value.any { it.id == server && it.status != "Connected" }) {
            reconnectSignals[server]?.trySend(Unit)
        }
    }

    fun report(error: Throwable) {
        if (error is CancellationException) throw error
        mutableError.value = error.message ?: "Operation failed"
    }

    fun setForeground(active: Boolean) {
        foreground?.cancel()
        foreground =
            if (active) {
                scope.launch {
                    while (isActive) {
                        servers.value.filter { it.status == "Connected" }.forEach { data.refreshObserved(it.id) }
                        delay(30_000.milliseconds)
                    }
                }
            } else {
                null
            }
    }

    fun homeQuery(server: String) = RemoteQuery(server, "", "ui/home")

    fun supportsHistory(server: String) = servers.value.any { it.id == server && "hostHistory" in it.capabilities }

    fun prefetchSession(
        server: String,
        session: JSONObject,
    ) {
        val id = session.text("id")
        data.refresh(chatsQuery(server, session))
        listOf(
            "model/list",
            "account/read",
            "account/rateLimits/read",
            "plugin/installed",
        ).forEach { data.refresh(RemoteQuery(server, id, it)) }
    }

    fun chatsQuery(
        server: String,
        session: JSONObject,
        params: JSONObject = JSONObject(),
    ) = RemoteQuery.of(
        server,
        session.getString("id"),
        "ui/chats",
        JSONObject(params.toString()).apply {
            put("cwd", chatWorkingDirectories(session))
            if (!has("limit")) put("limit", 50)
            if (!has("sourceKinds")) put("sourceKinds", chatSources())
            if (!has("sortKey")) put("sortKey", "updated_at")
        },
    )

    private suspend fun loadQuery(query: RemoteQuery): JSONObject {
        val params = JSONObject(query.parameters)
        return when (query.method) {
            "ui/chats" -> listProjectChats(query.server, obj("id" to query.session), params)
            "ui/home" ->
                coroutineScope {
                    val history =
                        async {
                            if (supportsHistory(query.server)) data.read(RemoteQuery(query.server, "", "history/list")) else null
                        }
                    val projects = async { data.read(RemoteQuery(query.server, "", "project/list")) }
                    val sessions = data.read(RemoteQuery(query.server, "", "session/list")).getJSONArray("data").objects()
                    val catalog = projects.await().getJSONArray("data")
                    val previous = data.peek(query)
                    data.publishPartial(
                        query,
                        obj(
                            "projects" to catalog,
                            "sessions" to JSONArray(sessions),
                            "recent" to (previous?.optJSONArray("recent") ?: JSONArray()),
                            "projectChats" to (previous?.optJSONArray("projectChats") ?: JSONArray()),
                        ),
                    )
                    val chatFailure =
                        java.util.concurrent.atomic
                            .AtomicReference<Exception?>()
                    val sessionChats =
                        sessions
                            .filter { it.supportsTasks() }
                            .map { session ->
                                async {
                                    try {
                                        data
                                            .read(chatsQuery(query.server, session))
                                            .getJSONArray("data")
                                            .objects()
                                            .map { obj("session" to session, "chat" to it) }
                                    } catch (cancelled: CancellationException) {
                                        throw cancelled
                                    } catch (failure: Exception) {
                                        chatFailure.compareAndSet(null, failure)
                                        data
                                            .peek(chatsQuery(query.server, session))
                                            ?.optJSONArray("data")
                                            ?.objects()
                                            .orEmpty()
                                            .map { obj("session" to session, "chat" to it) }
                                    }
                                }
                            }.awaitAll()
                            .flatten()
                            .distinctBy {
                                it.getJSONObject("session").text("profile") to it.getJSONObject("chat").text("id")
                            }.sortedByDescending { it.getJSONObject("chat").optLong("updatedAt") }
                    val saved = history.await()
                    val projectChats =
                        (sessionChats + saved?.optJSONArray("data")?.objects().orEmpty())
                            .distinctBy { chatIdentity(it.getJSONObject("session"), it.getJSONObject("chat")) }
                            .sortedByDescending { it.getJSONObject("chat").optLong("updatedAt") }
                    val allProjects =
                        (catalog.objects() + saved?.optJSONArray("projects")?.objects().orEmpty())
                            .distinctBy { it.text("path") }
                    val recent = projectChats.take(24)
                    recent.take(3).forEach { item ->
                        data.refresh(
                            RemoteQuery.of(
                                query.server,
                                item.getJSONObject("session").text("id"),
                                "thread/snapshot",
                                obj(
                                    "threadId" to item.getJSONObject("chat").text("id"),
                                ),
                            ),
                        )
                    }
                    val result =
                        obj(
                            "projects" to JSONArray(allProjects),
                            "sessions" to JSONArray(sessions),
                            "recent" to JSONArray(recent),
                            "projectChats" to JSONArray(projectChats),
                        )
                    chatFailure.get()?.let {
                        data.publishPartial(query, result)
                        throw it
                    }
                    result
                }
            else ->
                if (query.session.isEmpty()) {
                    request(
                        query.server,
                        query.method,
                        params,
                    ) as JSONObject
                } else {
                    rpc(query.server, query.session, query.method, params)
                }
        }
    }

    private fun changed(event: RemoteEvent) {
        val method = event.message.text("method")
        val thread =
            event.message
                .optJSONObject("params")
                ?.text("threadId")
                .orEmpty()
        data.invalidate(event.server) { queryAffectedByEvent(it, method, event.session, thread) }
        if (method.endsWith("Delta") || method.endsWith("/delta") || method.startsWith("thread/realtime/")) return
        synchronized(refreshJobs) {
            if (refreshJobs[event.server]?.isActive == true) return
            refreshJobs[event.server] =
                scope.launch {
                    delay(350.milliseconds)
                    data.refreshObserved(event.server)
                    data.refresh(homeQuery(event.server))
                }
        }
    }

    fun start() {
        network.start()
        scope.launch { catalog }
        scope.launch { runCatching { store.read().forEach(::connectSaved) }.onFailure(::report) }
    }

    private fun update(
        id: String,
        name: String,
        status: String,
        fingerprint: String,
        capabilities: Set<String> = emptySet(),
    ) {
        val server = RemoteServer(id, labels.getString(id, null)?.takeIf { it.isNotBlank() } ?: name, status, fingerprint, capabilities)
        mutableServers.update { list -> if (list.none { it.id == id }) list + server else list.map { if (it.id == id) server else it } }
    }

    private fun connectSaved(
        saved: String,
        authenticated: NativeClient? = null,
    ) {
        network.setEnabled(store.read().any { JSONObject(it).getJSONObject("connection").text("type") == "yggdrasil" })
        val discovery = JSONObject(saved).getJSONObject("discovery")
        val id = discovery.getString("daemonId")
        val previous = jobs.remove(id)
        previous?.cancel()
        val ready =
            java.util.concurrent.atomic
                .AtomicReference(authenticated)
        val retry = reconnectSignals.getOrPut(id) { Channel(Channel.CONFLATED) }
        var capabilities =
            discovery
                .optJSONArray("capabilities")
                ?.let { values ->
                    (0 until values.length()).map { values.getString(it) }.toSet()
                }.orEmpty()
        jobs[id] =
            scope
                .launch {
                    previous?.join()
                    var delayMs = 1000L
                    while (isActive) {
                        update(id, discovery.getString("name"), "Connecting", discovery.getString("fingerprint"), capabilities)
                        var client: NativeClient? = null
                        try {
                            client = ready.getAndSet(null) ?: NativeClient.restore(saved, cache)
                            ensureActive()
                            clients[id] = client
                            val current = JSONObject(client.savedConnection()).getJSONObject("discovery")
                            val advertised = current.getJSONArray("capabilities")
                            capabilities = (0 until advertised.length()).map { advertised.getString(it) }.toSet()
                            discovery.put("name", current.getString("name"))
                            update(id, current.getString("name"), "Connected", current.getString("fingerprint"), capabilities)
                            data.invalidate(id)
                            data.refresh(homeQuery(id))
                            if ("launcherSettings" in capabilities) data.refresh(RemoteQuery(id, "", "launcher/list"))
                            data.refreshObserved(id)
                            mutableEvents.emit(RemoteEvent(id, "", 0, obj("method" to "connection/reset")))
                            delayMs = 1000L
                            while (isActive) {
                                val event = client.nextEvent() ?: continue
                                when (event.kind.name) {
                                    "DISCONNECTED" -> error(event.message)
                                    "RECONCILE" -> {
                                        val reset = RemoteEvent(id, "", 0, obj("method" to "connection/reset"))
                                        changed(reset)
                                        mutableEvents.emit(reset)
                                    }
                                    "MESSAGE" -> {
                                        val message = JSONObject(event.message)
                                        val remote = RemoteEvent(id, event.sessionId, event.sequence.toLong(), message)
                                        if (message.has("id") &&
                                            message.has("method")
                                        ) {
                                            pending[inputNotificationTag(id, remote.session, message.get("id"))] = remote
                                        }
                                        if (message.text("method") == "serverRequest/resolved") {
                                            val requestId = message.optJSONObject("params")?.opt("requestId")
                                            pending.remove(inputNotificationTag(id, remote.session, requestId))
                                        }
                                        changed(remote)
                                        mutableEvents.emit(remote)
                                    }
                                }
                            }
                        } catch (
                            e: CancellationException,
                        ) {
                            throw e
                        } catch (
                            e: Exception,
                        ) {
                            update(
                                id,
                                discovery.getString("name"),
                                connectionStatus(e.message),
                                discovery.getString("fingerprint"),
                                capabilities,
                            )
                        } finally {
                            if (client != null) {
                                clients.remove(id, client)
                                client.destroy()
                            }
                        }
                        withTimeoutOrNull(delayMs) { retry.receive() }
                        delayMs = (delayMs * 2).coerceAtMost(30_000)
                    }
                }.also { job -> job.invokeOnCompletion { ready.getAndSet(null)?.destroy() } }
    }

    suspend fun scanHost(
        host: String,
        port: Int?,
    ): List<JSONObject> =
        withContext(Dispatchers.IO) {
            require(host.isNotBlank()) { "Enter a host name or IP address" }
            require(port == null || port in 1..65535) { "Invalid port" }
            discoverHost(host.trim().removeSurrounding("[", "]"), port?.toUShort(), cache).map(::JSONObject)
        }

    suspend fun pairInvitation(uri: String): Pair<NativeClient, JSONObject> =
        withContext(Dispatchers.IO) {
            network.setEnabled(true)
            try {
                val client = NativeClient.fromInvitation(uri.trim(), cache)
                try {
                    client to JSONObject(client.enroll(android.os.Build.MODEL))
                } catch (e: Exception) {
                    client.destroy()
                    throw e
                }
            } finally {
                network.setEnabled(store.read().any { JSONObject(it).getJSONObject("connection").text("type") == "yggdrasil" })
            }
        }

    suspend fun pairDirect(
        host: String,
        port: Int,
    ): Pair<NativeClient, JSONObject> =
        withContext(Dispatchers.IO) {
            val client = NativeClient.direct(host, port.toUShort(), cache)
            try {
                client to JSONObject(client.enroll(android.os.Build.MODEL))
            } catch (e: Exception) {
                client.destroy()
                throw e
            }
        }

    suspend fun finishPairing(client: NativeClient) =
        withContext(Dispatchers.IO) {
            val saved = client.savedConnection()
            require(!JSONObject(saved).isNull("token")) { "Host approval is required" }
            store.save(saved)
            try {
                connectSaved(saved, client.activate(cache))
            } catch (failure: Exception) {
                connectSaved(saved)
                throw failure
            } finally {
                client.destroy()
            }
        }

    suspend fun pendingAlerts(): List<RemoteEvent> =
        withContext(Dispatchers.IO) {
            clients.entries.flatMap { (id, client) ->
                client.pendingAlerts().map { RemoteEvent(id, it.sessionId, it.sequence.toLong(), JSONObject(it.message)) }
            }
        }

    suspend fun transportPeers(server: String) = withContext(Dispatchers.IO) { clients[server]?.transportPeers().orEmpty() }

    suspend fun acknowledgeAlert(event: RemoteEvent) =
        withContext(Dispatchers.IO) {
            clients[event.server]?.acknowledgeAlert(event.sequence.toULong())
        }

    suspend fun forget(id: String) =
        withContext(Dispatchers.IO) {
            jobs.remove(id)?.cancelAndJoin()
            store.remove(id)
            mutableServers.update { list -> list.filterNot { it.id == id } }
            pending.entries.removeAll { it.value.server == id }
            refreshJobs.remove(id)?.cancel()
            data.remove(id)
            reconnectSignals.remove(id)?.close()
            requestSlots.remove(id)
            synchronized(drafts) { drafts.keys.removeAll { it.server == id } }
            labels.edit { remove(id) }
            network.setEnabled(store.read().any { JSONObject(it).getJSONObject("connection").text("type") == "yggdrasil" })
        }

    fun rename(
        id: String,
        name: String,
    ) {
        require(name.trim().isNotEmpty()) { "Enter a server name" }
        labels.edit { putString(id, name.trim().take(120)) }
        mutableServers.update { list -> list.map { if (it.id == id) it.copy(name = name.trim().take(120)) else it } }
    }

    suspend fun upload(
        server: String,
        session: String,
        source: java.io.File,
        destination: String,
    ) = withContext(Dispatchers.IO) {
        (clients[server] ?: error("The server is disconnected")).uploadFile(session, source.absolutePath, destination)
    }

    suspend fun download(
        server: String,
        session: String,
        source: String,
        destination: java.io.File,
        maximumBytes: Long =
            1024L * 1024 *
                1024,
    ) = withContext(Dispatchers.IO) {
        require(maximumBytes >= 0) { "Invalid download limit" }
        (
            clients[server] ?: error(
                "The server is disconnected",
            )
        ).downloadFileLimited(session, source, destination.absolutePath, maximumBytes.toULong())
    }

    suspend fun request(
        server: String,
        method: String,
        params: JSONObject = JSONObject(),
    ): Any =
        withContext(Dispatchers.IO) {
            val client = clients[server] ?: error("The server is disconnected")
            val result =
                requestSlots
                    .getOrPut(server) {
                        Semaphore(8)
                    }.withPermit { JSONTokener(client.request(method, params.toString())).nextValue() }
            if (method in
                setOf(
                    "project/add",
                    "project/createWork",
                    "project/open",
                    "session/create",
                    "session/stop",
                    "session/restart",
                    "device/revoke",
                    "connectionPassword/rotate",
                    "launcher/write",
                )
            ) {
                data.invalidate(server)
                changed(RemoteEvent(server, "", 0, obj("method" to method)))
            }
            result
        }

    suspend fun rpc(
        server: String,
        session: String,
        method: String,
        params: JSONObject = JSONObject(),
    ): JSONObject {
        val result = request(server, "codex/rpc", obj("sessionId" to session, "method" to method, "params" to params)) as JSONObject
        if (method.substringAfterLast('/') in
            setOf(
                "set",
                "update",
                "start",
                "create",
                "delete",
                "archive",
                "unarchive",
                "fork",
                "write",
                "batchWrite",
                "install",
                "uninstall",
                "add",
                "remove",
                "reorder",
                "clear",
                "move",
                "reset",
                "reload",
                "writeFile",
                "createDirectory",
                "copy",
                "upgrade",
                "reconcile",
                "terminate",
                "clean",
                "consume",
                "setup",
                "logout",
                "import",
            )
        ) {
            changed(RemoteEvent(server, session, 0, obj("method" to method, "params" to params)))
        }
        return result
    }

    suspend fun reply(
        event: RemoteEvent,
        result: JSONObject,
    ) {
        request(event.server, "codex/reply", obj("sessionId" to event.session, "id" to event.message.get("id"), "result" to result))
        pending.remove(inputNotificationTag(event.server, event.session, event.message.get("id")))
    }
}
