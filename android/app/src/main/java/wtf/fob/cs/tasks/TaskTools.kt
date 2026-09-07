package wtf.fob.cs.tasks

import android.content.Context
import android.net.Uri
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.ui.*

@Composable fun AgentTasksDialog(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    open: (String) -> Unit,
    close: () -> Unit,
) {
    var children by remember { mutableStateOf(true) }
    var tasks by remember { mutableStateOf<List<JSONObject>>(emptyList()) }
    var cursor by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    suspend fun load(append: Boolean = false) {
        busy = true
        runCatching {
            if (children) {
                val page =
                    repo.rpc(
                        server,
                        session,
                        "thread/list",
                        obj(
                            "ancestorThreadId" to thread,
                            "limit" to 50,
                            "cursor" to cursor.takeIf { append },
                        ),
                    )
                tasks = ((if (append) tasks else emptyList()) + page.getJSONArray("data").objects()).takeLast(500)
                cursor = page.text("nextCursor")
            } else {
                val ids = repo.rpc(server, session, "thread/loaded/list").getJSONArray("data")
                val loaded = mutableListOf<JSONObject>()
                for (i in 0 until minOf(ids.length(), 100)) {
                    loaded +=
                        repo
                            .rpc(
                                server,
                                session,
                                "thread/read",
                                obj("threadId" to ids.getString(i), "includeTurns" to false),
                            ).getJSONObject("thread")
                }
                tasks = loaded
                cursor = ""
            }
        }.onFailure(repo::report)
        busy = false
    }
    LaunchedEffect(children) { load() }
    AlertDialog(onDismissRequest = close, title = { Text("Agents and tasks") }, text = {
        Column(Modifier.heightIn(max = 520.dp)) {
            Row {
                FilterChip(children, { children = true }, label = { Text("Subagents") })
                FilterChip(!children, { children = false }, label = { Text("Active tasks") }, modifier = Modifier.padding(start = 8.dp))
            }
            if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
            if (tasks.isEmpty() && !busy) Text("No tasks in this view")
            LazyColumn {
                items(tasks, key = { it.getString("id") }) { task ->
                    TextButton(onClick = {
                        open(task.getString("id"))
                        close()
                    }) {
                        Column(Modifier.fillMaxWidth()) {
                            Text(task.text("agentNickname", task.text("name", task.text("preview", "Untitled task"))).take(120))
                            Text(
                                "${task.text("agentRole")} · ${task.optJSONObject("status")?.text("type").orEmpty()}",
                                style = MaterialTheme.typography.bodySmall,
                            )
                        }
                    }
                }
                if (cursor.isNotEmpty()) {
                    item {
                        TextButton(
                            onClick = { scope.launch { load(true) } },
                            enabled = !busy,
                        ) { Text("Load more") }
                    }
                }
            }
        }
    }, confirmButton = { TextButton(onClick = close) { Text("Close") } })
}

suspend fun exportConversationJson(
    context: Context,
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    destination: Uri,
) = withContext(Dispatchers.IO) {
    context.contentResolver.openOutputStream(destination)!!.bufferedWriter().use { output ->
        var cursor: String? = null
        do {
            val page =
                repo.rpc(
                    server,
                    session,
                    "thread/items/list",
                    obj(
                        "threadId" to thread,
                        "limit" to 50,
                        "sortDirection" to "asc",
                        "cursor" to cursor,
                    ),
                )
            page.getJSONArray("data").objects().forEach { output.append(it.toString()).append('\n') }
            cursor = page.text("nextCursor").takeIf(String::isNotEmpty)
        } while (cursor != null)
    }
}
