package wtf.fob.cs.tasks

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
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

val taskActionLabels =
    linkedMapOf(
        "thread/name/set" to "Rename chat",
        "thread/fork" to "Fork chat",
        "thread/queue/list" to "Queued messages",
        "thread/searchOccurrences" to "Find in chat",
        "thread/timeline/list" to "Chat history",
        "review/start" to "Review code",
        "thread/goal/set" to "Goal",
        "thread/turns/list" to "Turn history",
        "thread/backgroundTerminals/list" to "Background terminals",
        "thread/metadata/update" to "Move to project",
        "thread/shellCommand" to "Run a shell command",
        "thread/section/move" to "Move to section",
        "thread/compact/start" to "Compact context",
        "thread/rollback" to "Roll back turns",
        "thread/archive" to "Archive chat",
        "thread/unarchive" to "Restore chat",
        "thread/delete" to "Delete chat",
    )

@Composable fun TaskControlSheet(
    method: String,
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
    thread: String,
    open: (String) -> Unit,
    removed: () -> Unit,
    close: () -> Unit,
) {
    if (method == "thread/queue/list") {
        QueueSheet(repo, server, session.text("id"), thread, close)
        return
    }
    if (method == "thread/turns/list") {
        TurnHistorySheet(repo, server, session.text("id"), thread, close)
        return
    }
    if (method in
        setOf("thread/searchOccurrences", "thread/timeline/list")
    ) {
        ChatHistorySheet(
            repo,
            server,
            session.text("id"),
            thread,
            method == "thread/searchOccurrences",
            close,
        )
        return
    }
    if (method == "thread/backgroundTerminals/list") {
        BackgroundTerminalsSheet(repo, server, session.text("id"), thread, close)
        return
    }
    if (method == "thread/metadata/update") {
        ChatProjectSheet(repo, server, session.text("id"), thread, close)
        return
    }
    val scope = rememberCoroutineScope()
    var text by remember(method) { mutableStateOf("") }
    var budget by remember(method) { mutableStateOf("") }
    var count by remember(method) { mutableStateOf("1") }
    var choice by remember(method) { mutableIntStateOf(0) }
    var busy by remember(method) { mutableStateOf(false) }
    var failure by remember(method) { mutableStateOf("") }
    var goalStatus by remember { mutableStateOf("active") }
    val (metadataQuery, metadata) =
        rememberRemoteData(
            repo,
            server,
            session.text("id"),
            when (method) {
                "thread/goal/set" -> "thread/goal/get"
                "thread/section/move" -> "threadSection/list"
                else -> "thread/read"
            },
            if (method == "thread/section/move") JSONObject() else obj("threadId" to thread),
        )
    LaunchedEffect(metadata.value) {
        if (text.isEmpty()) {
            text =
                when (method) {
                    "thread/name/set" ->
                        metadata.value
                            ?.optJSONObject("thread")
                            ?.let(::chatTitle)
                            .orEmpty()
                    "thread/goal/set" ->
                        metadata.value
                            ?.optJSONObject("goal")
                            ?.text("objective")
                            .orEmpty()
                    else -> ""
                }
        }
        if (method ==
            "thread/goal/set"
        ) {
            metadata.value?.optJSONObject("goal")?.let {
                goalStatus = it.text("status", "active")
                budget =
                    it.text("tokenBudget")
            }
        }
    }
    val sections =
        metadata.value
            ?.optJSONArray("data")
            ?.objects()
            .orEmpty()

    fun submit(operation: String = method) {
        scope.launch {
            busy = true
            failure = ""
            try {
                val params = obj("threadId" to thread)
                when (operation) {
                    "thread/name/set" -> {
                        require(text.isNotBlank()) { "Enter a name" }
                        params.put("name", text.trim())
                    }
                    "thread/fork" -> params.put("excludeTurns", true)
                    "thread/rollback" ->
                        params.put(
                            "numTurns",
                            count.toIntOrNull()?.takeIf { it > 0 } ?: error("Enter a positive turn count"),
                        )
                    "thread/shellCommand" -> {
                        require(text.isNotBlank()) { "Enter a command" }
                        params.put("command", text).put("timeoutMs", 600000)
                    }
                    "thread/goal/set" -> {
                        require(text.isNotBlank()) { "Enter a goal" }
                        params.put("objective", text.trim()).put("status", goalStatus)
                        params.put(
                            "tokenBudget",
                            if (budget.isBlank()) {
                                JSONObject.NULL
                            } else {
                                budget.toLongOrNull()?.takeIf { it > 0 }
                                    ?: error("Enter a positive token budget")
                            },
                        )
                    }
                    "thread/section/move" ->
                        params.put(
                            "sectionId",
                            if (choice ==
                                0
                            ) {
                                JSONObject.NULL
                            } else {
                                sections[choice - 1].getString("id")
                            },
                        )
                    "review/start" ->
                        params
                            .put(
                                "target",
                                when (choice) {
                                    0 -> obj("type" to "uncommittedChanges")
                                    1 -> {
                                        require(text.isNotBlank()) { "Enter a base branch" }
                                        obj("type" to "baseBranch", "branch" to text.trim())
                                    }
                                    2 -> {
                                        require(text.isNotBlank()) { "Enter a commit" }
                                        obj("type" to "commit", "sha" to text.trim())
                                    }
                                    else -> {
                                        require(text.isNotBlank()) { "Enter review instructions" }
                                        obj("type" to "custom", "instructions" to text)
                                    }
                                },
                            ).put("delivery", "inline")
                }
                val result = repo.rpc(server, session.text("id"), operation, params)
                if (operation == "thread/fork") open(result.getJSONObject("thread").getString("id"))
                if (operation in setOf("thread/archive", "thread/delete")) removed()
                close()
            } catch (
                error: Exception,
            ) {
                if (error is kotlinx.coroutines.CancellationException) throw error
                failure = error.message.orEmpty()
            } finally {
                busy = false
            }
        }
    }
    NativeSheet(taskActionLabels[method] ?: "Chat", close) {
        Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            when (method) {
                "thread/name/set" ->
                    OutlinedTextField(text, {
                        text = it
                    }, label = { Text("Chat name") }, singleLine = true, modifier = Modifier.fillMaxWidth())
                "thread/fork" -> Text("Continue from a copy of this chat. The new chat includes its completed history.")
                "thread/archive" -> Text("Move this chat to the archive. You can restore it from the project’s Archived list.")
                "thread/unarchive" -> Text("Return this chat to your project.")
                "thread/delete" -> Text("Permanently delete this chat and its stored history. This cannot be undone.")
                "thread/compact/start" -> Text("Summarize earlier messages to make room for more work.")
                "thread/shellCommand" -> {
                    Text(
                        "Run a command in this chat’s working directory. This command has full host access and does not use the chat sandbox.",
                    )
                    OutlinedTextField(
                        text,
                        { text = it },
                        label = { Text("Shell command") },
                        minLines = 2,
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
                "thread/rollback" -> {
                    Text("Remove the most recent turns from this chat. File changes remain in the workspace.")
                    OutlinedTextField(count, { count = it }, label = { Text("Number of turns") }, singleLine = true)
                }
                "review/start" -> {
                    ChoiceMenu("Review", listOf("Current changes", "Compare a branch", "A commit", "Custom instructions"), choice) {
                        choice =
                            it
                        text = ""
                    }
                    if (choice >
                        0
                    ) {
                        OutlinedTextField(text, {
                            text = it
                        }, label = {
                            Text(
                                listOf("", "Base branch", "Commit", "Review instructions")[choice],
                            )
                        }, modifier = Modifier.fillMaxWidth())
                    }
                }
                "thread/goal/set" -> {
                    DataStatus(metadata) { repo.data.refresh(metadataQuery, true) }
                    OutlinedTextField(
                        text,
                        { text = it },
                        label = { Text("What should Codex achieve?") },
                        minLines = 3,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    OutlinedTextField(budget, {
                        budget = it
                    }, label = { Text("Token budget (optional)") }, singleLine = true, modifier = Modifier.fillMaxWidth())
                    val statuses = listOf("active", "paused", "blocked", "usageLimited", "budgetLimited", "complete")
                    ChoiceMenu("Goal status", statuses.map(::humanize), statuses.indexOf(goalStatus)) { goalStatus = statuses[it] }
                    TextButton(onClick = { submit("thread/goal/clear") }, enabled = !busy) { Text("Remove goal") }
                }
                "thread/section/move" -> {
                    DataStatus(metadata) { repo.data.refresh(metadataQuery, true) }
                    ChoiceMenu("Section", listOf("No section") + sections.map { it.text("name") }, choice) { choice = it }
                }
            }
            if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
        }
        Button(
            onClick = { submit() },
            enabled = !busy,
            modifier = Modifier.fillMaxWidth().padding(top = 20.dp),
            colors =
                if (method ==
                    "thread/delete"
                ) {
                    ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.error)
                } else {
                    ButtonDefaults.buttonColors()
                },
        ) {
            Text(
                if (busy) {
                    "Saving…"
                } else {
                    when (method) {
                        "review/start" -> "Start review"
                        "thread/fork" -> "Fork chat"
                        "thread/delete" -> "Delete chat"
                        "thread/rollback" -> "Roll back"
                        else -> "Confirm"
                    }
                },
            )
        }
    }
}

@Composable fun TurnHistorySheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    close: () -> Unit,
) {
    val (query, state) =
        rememberRemoteData(
            repo,
            server,
            session,
            "thread/turns/list",
            obj(
                "threadId" to thread,
                "limit" to 50,
                "sortDirection" to "desc",
                "itemsView" to "notLoaded",
            ),
        )
    var revert by remember { mutableStateOf<JSONObject?>(null) }
    val scope = rememberCoroutineScope()
    NativeSheet("Turn history", close) {
        DataStatus(state) { repo.data.refresh(query, true) }
        LazyColumn(Modifier.weight(1f, fill = false)) {
            val turns =
                state.value
                    ?.optJSONArray("data")
                    ?.objects()
                    .orEmpty()
            if (turns.isEmpty() && !state.loading) item { EmptyState("No turns yet", "Your conversation history will appear here.") }
            items(turns, key = { it.text("id") }) { turn ->
                SettingsRow("Turn ${turns.size - turns.indexOf(turn)}", humanize(turn.text("status")), Icons.Default.History, onClick = {
                    revert =
                        turn
                })
            }
        }
    }
    revert?.let { turn ->
        AlertDialog(onDismissRequest = {
            revert = null
        }, title = {
            Text("Revert to this turn?")
        }, text = { Text("Remove this turn and all later turns, and revert their file changes where supported.") }, confirmButton = {
            TextButton(onClick = {
                scope.launch {
                    runCatching {
                        repo.rpc(
                            server,
                            session,
                            "thread/revert",
                            obj(
                                "threadId" to thread,
                                "beforeTurnId" to turn.text("id"),
                            ),
                        )
                    }.onSuccess {
                        revert = null
                        repo.data.refresh(query, true)
                    }.onFailure(repo::report)
                }
            }) { Text("Revert") }
        }, dismissButton = {
            TextButton(onClick = {
                revert =
                    null
            }) { Text("Cancel") }
        })
    }
}
