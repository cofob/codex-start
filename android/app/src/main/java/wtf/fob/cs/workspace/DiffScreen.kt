package wtf.fob.cs.workspace

import android.annotation.SuppressLint
import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*

@Composable fun DiffScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
    thread: String,
) {
    var mode by remember { mutableStateOf("working") }
    var base by remember { mutableStateOf("HEAD") }
    var diff by remember(server, session, thread) { mutableStateOf("") }
    var failure by remember(server, session, thread) { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    var turns by remember(server, session, thread) { mutableStateOf<List<JSONObject>>(emptyList()) }
    var turnCursor by remember(thread) { mutableStateOf("") }
    var selectedTurn by remember(thread) { mutableStateOf("") }
    val scope = rememberCoroutineScope()

    suspend fun loadTurns(append: Boolean = false) {
        if (thread.isEmpty()) return
        val page =
            repo.rpc(
                server,
                session.getString("id"),
                "thread/turns/list",
                obj(
                    "threadId" to thread,
                    "limit" to 25,
                    "sortDirection" to "desc",
                    "itemsView" to "notLoaded",
                    "cursor" to turnCursor.takeIf { append },
                ),
            )
        turns = ((if (append) turns else emptyList()) + page.getJSONArray("data").objects()).takeLast(500)
        turnCursor = page.text("nextCursor")
        if (selectedTurn.isEmpty()) selectedTurn = turns.firstOrNull()?.text("id").orEmpty()
    }

    suspend fun refresh() {
        busy = true
        failure = null
        runCatching {
            if (mode != "turn") {
                val result =
                    repo.request(
                        server,
                        "workspace/diff",
                        obj("sessionId" to session.getString("id"), "mode" to mode, "base" to base),
                    ) as JSONObject
                if (result.optInt("exitCode") != 0) error(result.text("stderr", "Diff failed"))
                diff = result.text("stdout")
            } else if (thread.isNotEmpty()) {
                if (turns.isEmpty()) loadTurns()
                if (selectedTurn.isNotEmpty()) diff = loadTurnChanges(repo, server, session.getString("id"), thread, selectedTurn)
            }
        }.onFailure {
            failure = it.message ?: "Diff failed"
            repo.report(it)
        }
        busy = false
    }
    LaunchedEffect(server, session, thread, mode) { refresh() }
    LaunchedEffect(server, session, thread, mode) {
        repo.events.collect { event ->
            val params = event.message.optJSONObject("params")
            if (mode == "turn" &&
                event.server == server &&
                event.session == session.text("id") &&
                event.message.text("method") == "turn/diff/updated" &&
                params?.text("threadId") == thread &&
                (selectedTurn.isEmpty() || selectedTurn == params.text("turnId"))
            ) {
                diff = params.text("diff").take(2 * 1024 * 1024)
            }
        }
    }
    Column(Modifier.fillMaxSize()) {
        ChoiceMenu(
            "Diff",
            listOf("Working tree", "Staged", "Branch comparison", "Task turn"),
            listOf("working", "staged", "branch", "turn").indexOf(mode),
        ) {
            mode =
                listOf("working", "staged", "branch", "turn")[it]
            diff = ""
        }
        if (mode == "turn") {
            ChoiceMenu(
                "Turn",
                turns.map { "${it.text("id").take(12)} · ${it.text("status")}" },
                turns.indexOfFirst {
                    it.text("id") ==
                        selectedTurn
                },
            ) {
                selectedTurn = turns[it].text("id")
                scope.launch { refresh() }
            }
            if (turnCursor.isNotEmpty()) {
                TextButton(onClick = {
                    scope.launch { runCatching { loadTurns(true) }.onFailure(repo::report) }
                }, enabled = !busy) { Text("Load earlier turns") }
            }
        }
        if (mode == "branch") OutlinedTextField(base, { base = it }, label = { Text("Base branch or commit") })
        Row { TextButton(onClick = { scope.launch { refresh() } }, enabled = !busy) { Text("Refresh") } }
        if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
        failure?.let { Text(it, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(16.dp)) }
        if (diff.isEmpty() &&
            !busy &&
            failure == null
        ) {
            Text(
                if (mode == "turn" &&
                    thread.isEmpty()
                ) {
                    "Open a task to inspect its turn changes."
                } else {
                    "No changes"
                },
                Modifier.padding(16.dp),
            )
        }
        UnifiedDiffView(diff, Modifier.weight(1f))
    }
}

@SuppressLint("UnusedBoxWithConstraintsScope")
@Composable
fun UnifiedDiffView(
    diff: String,
    modifier: Modifier = Modifier,
) {
    val lines = remember(diff) { parseUnifiedDiff(diff) }
    val added = remember(lines) { lines.count { it.kind == DiffLineKind.Added } }
    val removed = remember(lines) { lines.count { it.kind == DiffLineKind.Removed } }
    val dark = MaterialTheme.colorScheme.surface.luminance() < 0.5f
    val green = if (dark) Color(0xFF7EE787) else Color(0xFF116329)
    val red = if (dark) Color(0xFFFFA198) else Color(0xFFA40E26)
    val greenBackground = if (dark) Color(0xFF123122) else Color(0xFFDAFBE1)
    val redBackground = if (dark) Color(0xFF3C1C23) else Color(0xFFFFEBE9)
    Column(modifier.fillMaxWidth()) {
        if (lines.isNotEmpty()) {
            Row(Modifier.padding(horizontal = 12.dp, vertical = 6.dp), horizontalArrangement = Arrangement.spacedBy(16.dp)) {
                Text("+$added added", color = green, style = MaterialTheme.typography.labelMedium)
                Text("−$removed removed", color = red, style = MaterialTheme.typography.labelMedium)
            }
        }
        BoxWithConstraints(Modifier.fillMaxSize()) {
            val viewportWidth = maxWidth
            val horizontal = rememberScrollState()
            LazyColumn(Modifier.fillMaxSize().horizontalScroll(horizontal)) {
                itemsIndexed(lines) { _, line ->
                    val background =
                        when (line.kind) {
                            DiffLineKind.Added -> greenBackground
                            DiffLineKind.Removed -> redBackground
                            DiffLineKind.Hunk -> MaterialTheme.colorScheme.secondaryContainer
                            DiffLineKind.Header -> MaterialTheme.colorScheme.surfaceContainer
                            else -> Color.Transparent
                        }
                    val foreground =
                        when (line.kind) {
                            DiffLineKind.Added -> green
                            DiffLineKind.Removed -> red
                            DiffLineKind.Hunk -> MaterialTheme.colorScheme.onSecondaryContainer
                            else -> MaterialTheme.colorScheme.onSurface
                        }
                    val gutter = "${line.oldNumber?.toString().orEmpty().padStart(
                        5,
                    )} ${line.newNumber?.toString().orEmpty().padStart(5)} │ "
                    SelectionContainer {
                        Text(
                            gutter + line.text,
                            Modifier.widthIn(min = viewportWidth).background(background).padding(horizontal = 8.dp, vertical = 2.dp),
                            color = foreground,
                            fontFamily = FontFamily.Monospace,
                            style = MaterialTheme.typography.bodySmall,
                            softWrap = false,
                        )
                    }
                }
            }
        }
    }
}

private suspend fun loadTurnChanges(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    turn: String,
): String {
    val output = StringBuilder()
    var cursor: String? = null
    val limit = 2 * 1024 * 1024
    do {
        val page =
            repo.rpc(
                server,
                session,
                "thread/items/list",
                obj(
                    "threadId" to thread,
                    "turnId" to turn,
                    "limit" to 50,
                    "sortDirection" to "asc",
                    "cursor" to cursor,
                ),
            )
        for (entry in page.getJSONArray("data").objects()) {
            val item = entry.getJSONObject("item")
            if (item.text("type") != "fileChange") continue
            for (change in item.optJSONArray("changes")?.objects().orEmpty()) {
                val patch = "\n${change.text("path")} · ${change.optJSONObject("kind")?.text("type").orEmpty()}\n${change.text("diff")}\n"
                if (output.length + patch.length > limit) {
                    output
                        .append(
                            patch.take(limit - output.length),
                        ).append("\nPreview limited to 2 MiB. Download the file or inspect it on the host for the full content.")
                    return output.toString()
                }
                output.append(patch)
            }
        }
        cursor = page.text("nextCursor").takeIf(String::isNotEmpty)
    } while (cursor != null)
    return output.toString()
}
