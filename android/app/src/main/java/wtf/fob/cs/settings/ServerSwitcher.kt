package wtf.fob.cs.settings

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import org.json.JSONObject
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*

private val ACTIVE_TASK_EVENTS =
    setOf(
        "turn/started",
        "turn/completed",
        "thread/status/changed",
        "thread/closed",
        "job/exited",
        "session/changed",
        "session/disconnected",
    )

data class ActiveChat(
    val server: RemoteServer,
    val session: JSONObject,
    val chat: JSONObject,
)

data class ActiveChatSnapshot(
    val tasks: List<ActiveChat>,
    val incomplete: Boolean,
)

fun activeChatSnapshot(
    server: RemoteServer,
    response: JSONObject,
): ActiveChatSnapshot {
    val tasks =
        response
            .optJSONArray("data")
            ?.objects()
            .orEmpty()
            .mapNotNull { item ->
                val session = item.optJSONObject("session") ?: return@mapNotNull null
                val thread =
                    item.optJSONObject("thread")
                        ?: obj(
                            "id" to "",
                            "name" to session.text("name").ifBlank { "Running job" },
                        )
                ActiveChat(server, session, thread)
            }
    return ActiveChatSnapshot(
        tasks,
        response.optJSONArray("unavailableSessions")?.length()?.let { it > 0 } == true,
    )
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ServerSwitcher(
    repo: RemoteRepository,
    servers: List<RemoteServer>,
    selected: String,
    close: () -> Unit,
    selectServer: (String) -> Unit,
    selectTask: (ActiveChat) -> Unit,
    add: () -> Unit,
) {
    var tab by remember { mutableIntStateOf(0) }
    var busy by remember { mutableStateOf(false) }
    var active by remember { mutableStateOf<List<ActiveChat>>(emptyList()) }
    var unavailable by remember { mutableStateOf<List<String>>(emptyList()) }
    var refresh by remember { mutableIntStateOf(0) }
    LaunchedEffect(tab) {
        if (tab != 1) return@LaunchedEffect
        repo.events.collect { event ->
            if (event.message.text("method") in ACTIVE_TASK_EVENTS) {
                refresh++
            }
        }
    }
    LaunchedEffect(tab, servers, refresh) {
        if (tab != 1) return@LaunchedEffect
        busy = true
        val limit = Semaphore(4)
        val results =
            coroutineScope {
                servers
                    .filter { it.status == "Connected" }
                    .map { server ->
                        async {
                            limit.withPermit {
                                server.name to
                                    runCatching {
                                        activeChatSnapshot(
                                            server,
                                            repo.request(server.id, "task/active/list") as JSONObject,
                                        )
                                    }
                            }
                        }
                    }.awaitAll()
            }
        active =
            results.flatMap {
                it.second
                    .getOrNull()
                    ?.tasks
                    .orEmpty()
            }
        unavailable =
            results
                .filter { it.second.isFailure || it.second.getOrNull()?.incomplete == true }
                .map { it.first }
        busy = false
    }
    ModalBottomSheet(onDismissRequest = close) {
        Column(Modifier.fillMaxWidth().heightIn(max = 640.dp).padding(horizontal = 16.dp)) {
            Row(Modifier.fillMaxWidth(), verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
                Text("Your workspace", Modifier.weight(1f), style = MaterialTheme.typography.headlineSmall)
                if (tab == 1) IconButton(onClick = { refresh++ }, enabled = !busy) { Icon(Icons.Default.Refresh, "Refresh active tasks") }
            }
            Text(
                "Connections stay active on every configured server.",
                Modifier.padding(vertical = 8.dp),
                style = MaterialTheme.typography.bodySmall,
            )
            TabRow(tab) {
                Tab(selected = tab == 0, onClick = { tab = 0 }, text = { Text("Servers") })
                Tab(selected = tab == 1, onClick = { tab = 1 }, text = { Text("Active tasks") })
            }
            if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
            LazyColumn(Modifier.weight(1f, fill = false), contentPadding = PaddingValues(vertical = 12.dp)) {
                if (tab == 0) {
                    items(servers, key = { it.id }) { server ->
                        OutlinedCard(onClick = { selectServer(server.id) }, Modifier.fillMaxWidth().padding(bottom = 8.dp)) {
                            ListItem(leadingContent = {
                                Icon(Icons.Default.Dns, null)
                            }, headlineContent = { Text(server.name) }, supportingContent = { Text(server.status) }, trailingContent = {
                                if (selected ==
                                    server.id
                                ) {
                                    Icon(Icons.Default.Check, "Selected server")
                                }
                            })
                        }
                    }
                    item {
                        FilledTonalButton(onClick = add, Modifier.fillMaxWidth()) {
                            Icon(Icons.Default.Add, null)
                            Text("Add server")
                        }
                    }
                } else {
                    if (!busy && active.isEmpty()) item { Text("No tasks are running right now.", Modifier.padding(vertical = 24.dp)) }
                    items(active, key = { "${it.server.id}/${it.session.text("id")}/${it.chat.text("id")}" }) { task ->
                        OutlinedCard(onClick = { selectTask(task) }, Modifier.fillMaxWidth().padding(bottom = 8.dp)) {
                            val needsInput =
                                repo.pendingApprovals(task.server.id, task.session.getString("id")).any {
                                    it.message.optJSONObject("params")?.text("threadId") ==
                                        task.chat.text("id")
                                }
                            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                                val status = chatWorkState(task.chat)
                                TaskStatus(
                                    if (needsInput) "Needs your input" else status.label.ifBlank { "Working" },
                                    attention = needsInput || status.attention,
                                    active = !needsInput && !status.attention && status != ChatWorkState.Idle,
                                )
                                Text(chatTitle(task.chat), style = MaterialTheme.typography.titleMedium)
                                Text(
                                    "${task.server.name} · ${projectTitle(task.session.getString("cwd"))}",
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        }
                    }
                    items(unavailable) { name -> Text("Cannot read all active tasks from $name.", Modifier.padding(8.dp)) }
                }
            }
            Spacer(Modifier.height(16.dp))
        }
    }
}
