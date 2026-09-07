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

fun chatSources() = JSONArray(listOf("cli", "vscode", "appServer"))

fun projectTitle(path: String): String = path.trimEnd('/').substringAfterLast('/').ifBlank { path }

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
            rpc(server, session.getString("id"), "thread/loaded/list", obj("limit" to 100)).getJSONArray("data")
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
                .take(100)
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
            it.text("cwd") == session.text("executionCwd") &&
                chatTitle(it).contains(params.text("searchTerm"), true)
        }
    val chats =
        (live + response.getJSONArray("data").objects())
            .distinctBy { it.text("id") }
            .sortedByDescending { it.optLong("updatedAt") }
    return response.put("data", JSONArray(chats))
}
