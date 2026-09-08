package wtf.fob.cs.data

import androidx.compose.runtime.*
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import org.json.JSONArray
import org.json.JSONObject

/** A query is scoped to both the server and the Codex session. */
data class RemoteQuery(
    val server: String,
    val session: String,
    val method: String,
    val parameters: String = "{}",
) {
    companion object {
        fun of(
            server: String,
            session: String,
            method: String,
            params: JSONObject = JSONObject(),
        ) = RemoteQuery(server, session, method, canonicalJson(params))
    }
}

fun canonicalJson(value: Any?): String =
    when (value) {
        is JSONObject ->
            value.keys().asSequence().sorted().joinToString(",", "{", "}") {
                JSONObject.quote(it) + ":" +
                    canonicalJson(value.opt(it))
            }
        is JSONArray -> (0 until value.length()).joinToString(",", "[", "]") { canonicalJson(value.opt(it)) }
        is String -> JSONObject.quote(value)
        null, JSONObject.NULL -> "null"
        else -> value.toString()
    }

data class RemoteDataState(
    val value: JSONObject? = null,
    val refreshing: Boolean = false,
    val error: String? = null,
    val updatedAt: Long = 0,
) {
    val loading get() = value == null && refreshing
}

/** In-memory, stale-while-revalidate data. No conversation or account data is written to disk.
 * Invalidations during a read cause one more read, so a late response cannot hide an event.
 */
class RemoteDataCache(
    private val scope: CoroutineScope,
    private val now: () -> Long = { System.nanoTime() / 1_000_000 },
    private val load: suspend (RemoteQuery) -> JSONObject,
) {
    private class Entry {
        val state = MutableStateFlow(RemoteDataState())
        var revision = 0L
        var stale = true
        var size = 0
        var job: Deferred<JSONObject>? = null
    }

    private val lock = Any()
    private val entries = LinkedHashMap<RemoteQuery, Entry>(16, .75f, true)

    private fun entry(query: RemoteQuery): Entry = synchronized(lock) { entries.getOrPut(query) { Entry() } }

    fun observe(query: RemoteQuery): StateFlow<RemoteDataState> = entry(query).state.asStateFlow()

    fun peek(query: RemoteQuery): JSONObject? = synchronized(lock) { entries[query]?.state?.value?.value }

    /** Show the catalogue while its bounded recent-chat reads are still in progress. */
    fun publishPartial(
        query: RemoteQuery,
        value: JSONObject,
    ) = synchronized(lock) {
        val entry = entry(query)
        entry.size = value.toString().length
        entry.state.update { it.copy(value = value) }
    }

    /** Starts a shared refresh. Call [read] when the result must be awaited. */
    @Suppress("DeferredResultUnused")
    fun refresh(
        query: RemoteQuery,
        force: Boolean = false,
    ) {
        refreshAsync(query, force)
    }

    internal fun refreshAsync(
        query: RemoteQuery,
        force: Boolean,
    ): Deferred<JSONObject> =
        synchronized(lock) {
            val entry = entry(query)
            entry.job?.takeIf { it.isActive }?.let { return@synchronized it }
            val current = entry.state.value
            if (!force && !entry.stale && current.value != null && now() - current.updatedAt < 30_000) {
                return@synchronized CompletableDeferred(current.value)
            }
            entry.state.value = current.copy(refreshing = true, error = null)
            scope
                .async(start = CoroutineStart.LAZY) {
                    try {
                        var result: JSONObject
                        do {
                            val revision = synchronized(lock) { entry.revision }
                            result = load(query)
                            val size = result.toString().length
                            synchronized(lock) {
                                entry.size = size
                                entry.stale = revision != entry.revision
                                entry.state.value = RemoteDataState(result, refreshing = entry.stale, updatedAt = now())
                            }
                        } while (synchronized(lock) { entry.stale })
                        result
                    } catch (cancelled: CancellationException) {
                        throw cancelled
                    } catch (failure: Exception) {
                        synchronized(lock) {
                            entry.stale = true
                            entry.state.update { it.copy(error = failure.message ?: "Cannot refresh this page") }
                        }
                        throw failure
                    } finally {
                        synchronized(lock) {
                            entry.state.update { it.copy(refreshing = false) }
                            entry.job = null
                            trim()
                        }
                    }
                }.also {
                    entry.job = it
                    it.start()
                }
        }

    suspend fun read(
        query: RemoteQuery,
        force: Boolean = false,
    ) = refreshAsync(query, force).await()

    fun invalidate(
        server: String,
        predicate: (RemoteQuery) -> Boolean = { true },
    ) = synchronized(lock) {
        entries.filterKeys { it.server == server && predicate(it) }.forEach { (_, value) ->
            value.stale = true
            value.revision++
        }
    }

    fun refreshObserved(
        server: String? = null,
        force: Boolean = false,
    ) {
        val keys =
            synchronized(lock) {
                entries
                    .filter { (key, value) ->
                        (server == null || key.server == server) &&
                            value.state.subscriptionCount.value > 0
                    }.keys
                    .toList()
            }
        keys.forEach { refresh(it, force) }
    }

    fun remove(server: String) =
        synchronized(lock) {
            entries.filterKeys { it.server == server }.values.forEach { it.job?.cancel() }
            entries.keys.removeAll { it.server == server }
        }

    private fun trim() {
        var size = entries.values.sumOf { it.size }
        val iterator = entries.entries.iterator()
        while (iterator.hasNext() && (entries.size > 96 || size > 12 * 1024 * 1024)) {
            val entry = iterator.next().value
            if (entry.job == null && entry.state.subscriptionCount.value == 0) {
                size -= entry.size
                iterator.remove()
            }
        }
    }
}

/** Streaming text is rendered directly. It must not trigger a network read per token. */
fun queryAffectedByEvent(
    query: RemoteQuery,
    method: String,
    session: String,
    thread: String,
): Boolean {
    if (method == "connection/reset") return true
    if (method.endsWith("Delta") || method.endsWith("/delta") || method.startsWith("thread/realtime/")) return false
    val queryThread = JSONObject(query.parameters).text("threadId")
    // The gateway can route a task to another session that already owns its writer.
    val sameTask = thread.isNotEmpty() && queryThread == thread && query.method.startsWith("thread/")
    if (!sameTask && query.session.isNotEmpty() && session.isNotEmpty() && query.session != session) return false
    if (queryThread.isNotEmpty() && thread.isNotEmpty() && queryThread != thread) return false
    val group = method.substringBefore('/')
    return when {
        query.method == "ui/home" -> group in setOf("project", "session", "thread", "turn", "serverRequest", "launcher")
        query.method == "history/list" -> group in setOf("project", "session", "thread", "turn", "launcher")
        query.method == "ui/chats" -> group in setOf("thread", "turn", "serverRequest", "session")
        query.method == "workspace/diff" || query.method.startsWith("fs/") -> group in setOf("fs", "item", "turn")
        query.method.startsWith("thread/") -> group in setOf("thread", "turn", "item", "serverRequest")
        query.method.startsWith("plugin/") || query.method.startsWith("app/") || query.method.startsWith("skills/") ->
            group in
                setOf("plugin", "app", "skills", "marketplace", "config")
        else -> query.method.substringBefore('/') == group || group == "config"
    }
}

@Composable fun rememberRemoteData(
    repo: RemoteRepository,
    server: String,
    session: String,
    method: String,
    params: JSONObject = JSONObject(),
    enabled: Boolean = true,
): Pair<RemoteQuery, RemoteDataState> {
    val parameters = canonicalJson(params)
    val query = remember(server, session, method, parameters) { RemoteQuery(server, session, method, parameters) }
    val flow =
        remember(repo, query, enabled) {
            if (enabled &&
                server.isNotEmpty()
            ) {
                repo.data.observe(query)
            } else {
                MutableStateFlow(RemoteDataState(value = repo.data.peek(query)))
            }
        }
    val state by flow.collectAsStateWithLifecycle()
    LaunchedEffect(repo, query, enabled) { if (server.isNotEmpty() && enabled) repo.data.refresh(query) }
    return query to state
}
