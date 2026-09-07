package wtf.fob.cs.conversation

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.*
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import kotlin.time.Duration.Companion.milliseconds

/** Search and history use separate cursors. Neither operation resumes a chat. */
@Composable fun ChatHistorySheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    searchMode: Boolean,
    close: () -> Unit,
) {
    var search by rememberSaveable { mutableStateOf("") }
    var term by remember { mutableStateOf("") }
    LaunchedEffect(search) {
        delay(300.milliseconds)
        term = search.trim()
    }
    val method = if (searchMode) "thread/searchOccurrences" else "thread/timeline/list"
    val params = obj("threadId" to thread, "limit" to 50).apply { if (searchMode) put("searchTerm", term) }
    val (query, state) = rememberRemoteData(repo, server, session, method, params, enabled = !searchMode || term.isNotEmpty())
    var extra by remember(query) { mutableStateOf<List<JSONObject>>(emptyList()) }
    var cursor by remember(query, state.value) { mutableStateOf(state.value?.text("nextCursor").orEmpty()) }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    NativeSheet(if (searchMode) "Find in chat" else "Chat history", close) {
        if (searchMode) SearchField(search, { search = it }, "Find a word or phrase", Modifier.padding(bottom = 12.dp))
        if (!searchMode || term.isNotEmpty()) DataStatus(state) { repo.data.refresh(query, true) }
        LazyColumn(Modifier.weight(1f, fill = false)) {
            val entries =
                if (searchMode && term.isEmpty()) {
                    emptyList()
                } else {
                    state.value
                        ?.optJSONArray("data")
                        ?.objects()
                        .orEmpty() + extra
                }
            if (entries.isEmpty() &&
                !state.loading
            ) {
                item {
                    EmptyState(
                        if (searchMode) "Find a message" else "No history yet",
                        if (searchMode) "Search messages in this chat." else "Messages and voice activity will appear here.",
                        Icons.Default.History,
                    )
                }
            }
            items(entries) { entry ->
                SettingsGroup {
                    if (searchMode) {
                        Text(entry.text("snippet"), Modifier.padding(20.dp))
                    } else {
                        when (entry.text("type")) {
                            "item" ->
                                entry.optJSONObject("item")?.let { raw ->
                                    val item = conversationItem(raw)
                                    Column(Modifier.padding(20.dp)) {
                                        Text(humanize(item.type), style = MaterialTheme.typography.labelMedium)
                                        Markdown(item.text)
                                    }
                                }
                            "realtime" ->
                                entry.optJSONObject("item")?.let { raw ->
                                    SettingsRow(
                                        "Voice · ${humanize(raw.text("type"))}",
                                        raw.text("text", raw.text("transcript")),
                                        Icons.Default.GraphicEq,
                                    )
                                }
                            "turnStarted" -> SettingsRow("Turn started", icon = Icons.Default.PlayArrow)
                            "turnCompleted" ->
                                SettingsRow(
                                    "Turn ${humanize(entry.text("status")).lowercase()}",
                                    entry.optJSONObject("error")?.text("message").orEmpty(),
                                    Icons.Default.CheckCircleOutline,
                                )
                        }
                    }
                }
            }
            if (cursor.isNotEmpty() && (!searchMode || term.isNotEmpty())) {
                item {
                    TextButton(enabled = !busy, onClick = {
                        scope.launch {
                            busy = true
                            try {
                                val next =
                                    repo.data.read(
                                        RemoteQuery.of(server, session, method, JSONObject(params.toString()).put("cursor", cursor)),
                                    )
                                extra = extra + next.optJSONArray("data")?.objects().orEmpty()
                                cursor = next.text("nextCursor")
                            } catch (error: Exception) {
                                repo.report(error)
                            } finally {
                                busy = false
                            }
                        }
                    }) { Text(if (busy) "Loading…" else "Load more") }
                }
            }
        }
    }
}

@Composable fun BackgroundTerminalsSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    close: () -> Unit,
) {
    val (query, state) = rememberRemoteData(repo, server, session, "thread/backgroundTerminals/list", obj("threadId" to thread))
    var terminate by remember { mutableStateOf<JSONObject?>(null) }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    NativeSheet("Background terminals", close) {
        DataStatus(state) { repo.data.refresh(query, true) }
        val processes =
            state.value
                ?.optJSONArray("data")
                ?.objects()
                .orEmpty()
        LazyColumn(Modifier.weight(1f, fill = false)) {
            if (processes.isEmpty() &&
                !state.loading
            ) {
                item { EmptyState("No background commands", "Commands that keep running will appear here.", Icons.Default.Terminal) }
            }
            items(processes, key = { it.text("processId") }) { process ->
                SettingsGroup {
                    SettingsRow(process.text("command"), process.text("cwd"), Icons.Default.Terminal, trailing = {
                        IconButton(enabled = !busy, onClick = {
                            terminate =
                                process
                        }) { Icon(Icons.Default.Stop, "Stop command") }
                    })
                    if (!process.isNull(
                            "cpuPercent",
                        )
                    ) {
                        Text(
                            "CPU ${process.optDouble("cpuPercent").toInt()}%",
                            Modifier.padding(horizontal = 20.dp, vertical = 8.dp),
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                }
            }
        }
        TextButton(onClick = { repo.data.refresh(query, true) }) { Text("Refresh") }
        if (processes.isNotEmpty()) TextButton(enabled = !busy, onClick = { terminate = JSONObject() }) { Text("Stop all commands") }
    }
    terminate?.let { process ->
        AlertDialog(onDismissRequest = {
            terminate = null
        }, title = {
            Text(if (process.has("processId")) "Stop this command?" else "Stop all background commands?")
        }, text = { Text(process.text("command", "This stops all running background commands in this chat.")) }, confirmButton = {
            TextButton(enabled = !busy, onClick = {
                scope.launch {
                    busy = true
                    try {
                        repo.rpc(
                            server,
                            session,
                            if (process.has("processId")) "thread/backgroundTerminals/terminate" else "thread/backgroundTerminals/clean",
                            obj(
                                "threadId" to thread,
                            ).apply { if (process.has("processId")) put("processId", process.text("processId")) },
                        )
                        terminate = null
                        repo.data.refresh(query, true)
                    } catch (error: Exception) {
                        repo.report(error)
                    } finally {
                        busy = false
                    }
                }
            }) { Text("Stop") }
        }, dismissButton = { TextButton(onClick = { terminate = null }) { Text("Cancel") } })
    }
}

@Composable fun ChatProjectSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    close: () -> Unit,
) {
    val (query, state) = rememberRemoteData(repo, server, session, "project/list")
    val scope = rememberCoroutineScope()
    var busy by remember { mutableStateOf(false) }

    fun choose(id: String) {
        scope.launch {
            busy = true
            try {
                repo.rpc(server, session, "thread/metadata/update", obj("threadId" to thread, "projectId" to id))
                close()
            } catch (
                error: Exception,
            ) {
                repo.report(error)
            } finally {
                busy = false
            }
        }
    }
    NativeSheet("Move to project", close) {
        DataStatus(state) { repo.data.refresh(query, true) }
        LazyColumn(Modifier.weight(1f, fill = false)) {
            item { SettingsRow("No project", enabled = !busy, onClick = { choose("") }) }
            items(
                state.value
                    ?.optJSONArray("data")
                    ?.objects()
                    .orEmpty(),
                key = {
                    it.text("id")
                },
            ) { project ->
                SettingsRow(project.text("name"), icon = Icons.Default.FolderOpen, enabled = !busy, onClick = {
                    choose(project.text("id"))
                })
            }
        }
    }
}
