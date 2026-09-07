package wtf.fob.cs.tasks

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.json.JSONObject
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.ui.*

@Composable fun TasksScreen(
    repo: RemoteRepository,
    server: String,
    selected: JSONObject?,
    select: (JSONObject) -> Unit,
    open: (String) -> Unit,
) {
    var sessions by remember(server) { mutableStateOf<List<JSONObject>>(emptyList()) }
    var threads by remember(server, selected) { mutableStateOf<List<JSONObject>>(emptyList()) }
    var search by remember { mutableStateOf("") }
    var archived by remember { mutableStateOf(false) }
    var cursor by remember { mutableStateOf("") }
    var logs by remember { mutableStateOf("") }
    var createSession by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    suspend fun refresh(append: Boolean = false) {
        busy = true
        runCatching {
            sessions = (repo.request(server, "session/list") as JSONObject).getJSONArray("data").objects()
            val current = sessions.firstOrNull { it.text("id") == selected?.text("id") }
            if (current != null && current.toString() != selected?.toString()) select(current)
            if (current != null && current.supportsTasks()) {
                val result =
                    repo.rpc(
                        server,
                        current.getString("id"),
                        "thread/list",
                        obj(
                            "limit" to 50,
                            "sourceKinds" to chatSources(),
                            "archived" to archived,
                            "searchTerm" to search,
                            "cursor" to cursor.takeIf { append },
                        ),
                    )
                threads =
                    (
                        (if (append) threads else emptyList()) +
                            result
                                .getJSONArray(
                                    "data",
                                ).objects()
                    ).distinctBy { it.getString("id") }.takeLast(500)
                cursor =
                    result.text("nextCursor")
            } else {
                threads = emptyList()
                cursor = ""
            }
        }.onFailure(repo::report)
        busy = false
    }
    LaunchedEffect(server, selected, archived) { refresh() }
    Column {
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = { createSession = true }) { Text("New session") }
            OutlinedButton(onClick = { scope.launch { refresh() } }, enabled = !busy) { Text("Refresh") }
        }
        ChoiceMenu(
            "Session",
            sessions.map { "${it.text("name")} · ${it.text("environment")}" },
            sessions.indexOfFirst {
                it.text("id") ==
                    selected?.text("id")
            },
        ) { select(sessions[it]) }
        selected?.let { selectedSession ->
            Text(selectedSession.text("cwd"), style = MaterialTheme.typography.bodySmall)
            Text("${selectedSession.text("kind")} · ${selectedSession.text("status")}", style = MaterialTheme.typography.labelLarge)
            Row {
                TextButton(onClick = {
                    scope.launch {
                        runCatching {
                            repo.request(
                                server,
                                "session/stop",
                                obj(
                                    "sessionId" to selectedSession.getString("id"),
                                ),
                            )
                        }.onSuccess { refresh() }.onFailure(repo::report)
                    }
                }) { Text("Stop session") }
                TextButton(onClick = {
                    scope.launch {
                        runCatching {
                            repo.request(
                                server,
                                "session/restart",
                                obj(
                                    "sessionId" to selectedSession.getString("id"),
                                ),
                            )
                        }.onSuccess { refresh() }.onFailure(repo::report)
                    }
                }) { Text("Restart") }
                TextButton(onClick = {
                    scope.launch {
                        runCatching {
                            logs =
                                (
                                    repo.request(
                                        server,
                                        "session/logs",
                                        obj("sessionId" to selectedSession.getString("id")),
                                    ) as JSONObject
                                ).text("text")
                        }.onFailure(repo::report)
                    }
                }) { Text("Logs") }
            }
            if (selectedSession.supportsTasks()) {
                Row {
                    Button(onClick = {
                        scope.launch {
                            runCatching {
                                val result =
                                    repo.rpc(
                                        server,
                                        selectedSession.getString("id"),
                                        "thread/start",
                                        obj(
                                            "cwd" to selectedSession.getString("executionCwd"),
                                            "historyMode" to "paginated",
                                        ),
                                    )
                                open(result.getJSONObject("thread").getString("id"))
                            }.onFailure(repo::report)
                        }
                    }) { Text("New task") }
                    FilterChip(
                        selected = archived,
                        onClick = { archived = !archived },
                        label = { Text("Archived") },
                        modifier = Modifier.padding(start = 8.dp),
                    )
                }
                Row {
                    OutlinedTextField(search, { value -> search = value }, label = { Text("Search tasks") }, modifier = Modifier.weight(1f))
                    TextButton(onClick = { scope.launch { refresh() } }) { Text("Search") }
                }
            } else {
                Text(
                    if (selectedSession.text("kind") ==
                        "job"
                    ) {
                        "This job has no interactive conversation. Use its logs to inspect the result."
                    } else {
                        "The session is not ready for task commands. Start or resume it on the host."
                    },
                    Modifier.padding(8.dp),
                )
            }
        }
        if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
        LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            if (selected ==
                null
            ) {
                items(sessions) { session ->
                    OutlinedCard(onClick = { select(session) }, Modifier.fillMaxWidth()) {
                        Column(Modifier.padding(16.dp)) {
                            Text(session.text("name"))
                            Text(session.text("cwd"), style = MaterialTheme.typography.bodySmall)
                        }
                    }
                }
            }
            items(threads, key = { it.getString("id") }) { item ->
                OutlinedCard(onClick = { open(item.getString("id")) }, modifier = Modifier.fillMaxWidth()) {
                    Column(Modifier.padding(16.dp)) {
                        Text(chatTitle(item), style = MaterialTheme.typography.titleMedium)
                        Text(item.text("cwd"), style = MaterialTheme.typography.bodySmall)
                    }
                }
            }
            if (cursor.isNotEmpty()) item { TextButton(onClick = { scope.launch { refresh(true) } }) { Text("Load more tasks") } }
        }
    }
    if (logs.isNotEmpty()) {
        AlertDialog(onDismissRequest = {
            logs = ""
        }, title = {
            Text("Session log (last 256 KiB)")
        }, text = { LazyColumn { items(logs.lines()) { Text(it, style = MaterialTheme.typography.bodySmall) } } }, confirmButton = {
            TextButton(onClick = {
                logs =
                    ""
            }) { Text("Close") }
        })
    }
    if (createSession) {
        CreateSessionDialog(repo, server, { createSession = false }, { result ->
            createSession = false
            select(result)
            scope.launch { refresh() }
        })
    }
}

@Composable private fun CreateSessionDialog(
    repo: RemoteRepository,
    server: String,
    close: () -> Unit,
    created: (JSONObject) -> Unit,
) {
    var worktree by remember { mutableStateOf(false) }
    var cwd by remember { mutableStateOf("") }
    var name by remember { mutableStateOf("") }
    var environment by remember { mutableStateOf("") }
    var home by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    AlertDialog(onDismissRequest = { if (!busy) close() }, title = { Text("New host session") }, text = {
        Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(cwd, { cwd = it }, label = { Text("Host workspace path") })
            OutlinedTextField(name, { name = it }, label = { Text("Session name") })
            OutlinedTextField(environment, { environment = it }, label = { Text("Environment (optional)") })
            OutlinedTextField(home, {
                home =
                    it
            }, label = { Text("Codex home (optional)") })
            Row {
                Checkbox(worktree, { worktree = it })
                Text("Create a linked worktree", Modifier.padding(top = 12.dp))
            }
            if (busy) {
                LinearProgressIndicator()
                Text("Preparing the host environment…")
            }
        }
    }, confirmButton = {
        TextButton(enabled = cwd.startsWith('/') && !busy, onClick = {
            scope.launch {
                busy = true
                runCatching {
                    repo.request(
                        server,
                        "session/create",
                        obj(
                            "cwd" to cwd,
                            "worktree" to worktree,
                            "name" to name.takeIf(String::isNotBlank),
                            "environment" to environment.takeIf(String::isNotBlank),
                            "home" to home.takeIf(String::isNotBlank),
                        ),
                    ) as JSONObject
                }.onSuccess(created).onFailure(repo::report)
                busy = false
            }
        }) { Text("Create") }
    }, dismissButton = { TextButton(onClick = close, enabled = !busy) { Text("Cancel") } })
}
