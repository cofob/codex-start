package wtf.fob.cs.data

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import org.json.JSONArray
import org.json.JSONObject

fun chatTitle(chat: JSONObject): String =
    chat
        .text("name")
        .ifBlank {
            chat.text("preview")
        }.ifBlank { "New chat" }
        .lineSequence()
        .first()
        .take(160)

// An omitted or empty sourceKinds list means only CLI and VS Code, not all sources.
fun chatSources() =
    JSONArray(
        listOf(
            "cli",
            "vscode",
            "exec",
            "appServer",
            "subAgent",
            "subAgentReview",
            "subAgentCompact",
            "subAgentThreadSpawn",
            "subAgentOther",
            "unknown",
        ),
    )

fun chatIdentity(
    session: JSONObject,
    chat: JSONObject,
): String = JSONArray(listOf(chat.text("historyStore"), session.text("profile"), chat.text("id"))).toString()

fun chatOrigin(chat: JSONObject): String {
    val source = chat.opt("source")
    return when {
        source is JSONObject -> "Agent thread"
        source == "vscode" -> "VS Code / desktop"
        source == "cli" -> "Codex CLI"
        source == "appServer" -> "App server"
        source == "exec" -> "Codex exec"
        source is String && source.startsWith("subAgent") -> "Agent thread"
        else -> ""
    }
}

fun projectTitle(path: String): String = path.trimEnd('/').substringAfterLast('/').ifBlank { path }

fun chatWorkingDirectories(session: JSONObject): Any {
    val paths = listOf(session.text("executionCwd"), session.text("cwd")).filter { it.isNotEmpty() }.distinct()
    return if (paths.size == 1) paths.single() else JSONArray(paths)
}

fun matchesChatDirectory(
    chat: JSONObject,
    params: JSONObject,
): Boolean =
    when (val cwd = params.opt("cwd")) {
        is JSONArray -> (0 until cwd.length()).any { cwd.optString(it) == chat.text("cwd") }
        is String -> cwd == chat.text("cwd")
        else -> true
    }

fun JSONObject.supportsTasks(): Boolean =
    text("kind") != "job" &&
        optJSONArray("capabilities")?.let { values ->
            (0 until values.length()).any { values.optString(it) == "codexRpc" }
        } == true

/** Codex keeps a new, empty conversation in memory before it has a durable history. */
suspend fun RemoteRepository.listProjectChats(
    server: String,
    session: JSONObject,
    params: JSONObject,
): JSONObject {
    val response = rpc(server, session.getString("id"), "thread/list", params)
    if (params.optBoolean("archived") || params.text("cursor").isNotEmpty()) return response
    val ids =
        try {
            val loaded = JSONArray()
            var cursor = ""
            val seen = mutableSetOf<String>()
            do {
                val page =
                    rpc(
                        server,
                        session.getString("id"),
                        "thread/loaded/list",
                        obj(
                            "limit" to 100,
                            "cursor" to cursor.ifBlank { null },
                        ),
                    )
                val data = page.getJSONArray("data")
                (0 until data.length()).forEach { loaded.put(data.getString(it)) }
                cursor = page.text("nextCursor")
                check(cursor.isEmpty() || seen.add(cursor)) { "The host repeated a loaded-chat page" }
            } while (cursor.isNotEmpty())
            loaded
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            return response
        }
    val listed =
        response
            .getJSONArray("data")
            .objects()
            .map { it.text("id") }
            .toSet()
    val limit = Semaphore(4)
    val live =
        coroutineScope {
            (0 until ids.length())
                .filter { ids.getString(it) !in listed }
                .map { index ->
                    async {
                        limit.withPermit {
                            try {
                                rpc(
                                    server,
                                    session.getString("id"),
                                    "thread/read",
                                    obj("threadId" to ids.getString(index), "includeTurns" to false),
                                ).getJSONObject("thread")
                            } catch (cancelled: CancellationException) {
                                throw cancelled
                            } catch (_: Exception) {
                                null
                            }
                        }
                    }
                }.awaitAll()
                .filterNotNull()
        }.filter {
            matchesChatDirectory(it, params) &&
                chatTitle(it).contains(params.text("searchTerm"), true)
        }
    val chats =
        (live + response.getJSONArray("data").objects())
            .distinctBy { it.text("id") }
            .sortedByDescending { it.optLong("updatedAt") }
    return response.put("data", JSONArray(chats))
}
