package wtf.fob.cs.workspace

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*

/** Server-side title/project search and cursor paging across all local Codex stores. */
@Composable
fun HostHistoryOverview(
    repo: RemoteRepository,
    server: String,
    home: JSONObject?,
    search: String,
    connected: Boolean,
    open: (RecentChat) -> Unit,
    chooseProject: () -> Unit,
    newWork: () -> Unit,
    cwd: String = "",
    showActions: Boolean = true,
) {
    val servers by repo.servers.collectAsState()
    var term by remember(server, cwd) { mutableStateOf("") }
    var archived by rememberSaveable(server, cwd) { mutableStateOf(false) }
    LaunchedEffect(search) {
        delay(300)
        term = search.trim()
    }
    val params = obj("searchTerm" to term, "cwd" to cwd, "archived" to archived, "limit" to 50)
    val (query, state) = rememberRemoteData(repo, server, "", "history/list", params, enabled = connected)
    var pages by remember(query) { mutableStateOf<List<JSONObject>>(emptyList()) }
    var next by remember(query) { mutableStateOf<String?>(null) }
    var busy by remember(query) { mutableStateOf(false) }
    var failure by remember(query) { mutableStateOf("") }
    val scope = rememberCoroutineScope()
    LaunchedEffect(state.value) {
        pages = emptyList()
        next = null
    }
    val cursor = next ?: state.value?.text("nextCursor").orEmpty()
    val saved =
        state.value
            ?.optJSONArray("data")
            ?.objects()
            .orEmpty() + pages
    val live =
        if (archived) {
            emptyList()
        } else {
            home?.optJSONArray("projectChats")?.objects().orEmpty().filter {
                val session = it.getJSONObject("session")
                session.text("kind") != "history" && (cwd.isEmpty() || session.text("cwd") == cwd)
            }
        }
    val listing =
        obj(
            "projectChats" to JSONArray((live + saved).distinctBy { chatIdentity(it.getJSONObject("session"), it.getJSONObject("chat")) }),
        )
    Column(Modifier.fillMaxSize()) {
        Row {
            FilterChip(archived, { archived = !archived }, label = { Text("Archived") }, modifier = Modifier.testTag("history-archived"))
            Spacer(Modifier.weight(1f))
            TextButton(onClick = { repo.data.refresh(query, true) }, enabled = connected && !state.refreshing) { Text("Refresh") }
        }
        DataStatus(state) { repo.data.refresh(query, true) }
        state.value?.optJSONArray("warnings")?.let { warnings ->
            if (warnings.length() > 0) {
                var show by remember { mutableStateOf(false) }
                TextButton(onClick = { show = !show }) { Text("Saved history: ${warnings.length()} notice(s)") }
                if (show) {
                    Text(
                        (0 until warnings.length()).joinToString("\n") { warnings.optString(it) },
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            }
        }
        if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
        WorkOverview(
            listing,
            search,
            state.refreshing || busy,
            connected,
            open,
            chooseProject,
            workSupported = servers.any { it.id == server && "workProjects" in it.capabilities },
            newWork = newWork,
            historyComplete = true,
            showActions = showActions,
            hasMore = cursor.isNotEmpty(),
            loadMore = {
                scope.launch {
                    busy = true
                    failure = ""
                    try {
                        val result = repo.request(server, "history/list", JSONObject(params.toString()).put("cursor", cursor)) as JSONObject
                        pages =
                            (
                                pages +
                                    result
                                        .getJSONArray(
                                            "data",
                                        ).objects()
                            ).distinctBy { chatIdentity(it.getJSONObject("session"), it.getJSONObject("chat")) }
                        next = result.text("nextCursor")
                    } catch (cancelled: kotlinx.coroutines.CancellationException) {
                        throw cancelled
                    } catch (error: Exception) {
                        failure = error.message ?: "Cannot load older chats"
                    } finally {
                        busy = false
                    }
                }
            },
        )
    }
}
