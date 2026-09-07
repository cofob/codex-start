package wtf.fob.cs.conversation

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.PlaylistAdd
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeout
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*
import java.util.UUID

internal fun queueText(input: JSONArray): String =
    input
        .objects()
        .filter {
            it.text("type") == "text"
        }.joinToString("\n") { it.text("text") }

/** Text edits must retain images, files, mentions, and other structured input. */
internal fun editQueueInput(
    input: JSONArray,
    text: String,
): JSONArray {
    if (queueText(input) == text) return JSONArray(input.toString())
    val result = JSONArray()
    var inserted = false
    input.objects().forEach {
        if (it.text("type") != "text") {
            result.put(JSONObject(it.toString()))
        } else if (!inserted) {
            if (text.isNotBlank()) result.put(obj("type" to "text", "text" to text))
            inserted = true
        }
    }
    if (!inserted && text.isNotBlank()) result.put(obj("type" to "text", "text" to text))
    return result
}

private fun queueId(entry: JSONObject) = entry.text("id", entry.text("queuedSubmissionId"))

private fun queueInput(entry: JSONObject) = entry.optJSONArray("input") ?: JSONArray()

private fun queueParams(thread: String) = obj("threadId" to thread, "limit" to 100)

/** Keep potentially large attachment data in memory, not in Android's saved-state bundle. */
internal class QueueEditState : ViewModel() {
    val draft = mutableStateOf("")
    val editing = mutableStateOf("")
    val original = mutableStateOf("[]")
    val busy = mutableStateOf(false)
    val failure = mutableStateOf("")
}

@Composable
fun ConversationQueue(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    canWrite: Boolean,
    showing: Boolean,
    open: () -> Unit,
    close: () -> Unit,
) {
    val (_, state) = rememberRemoteData(repo, server, session, "thread/queue/list", queueParams(thread))
    val entries =
        state.value
            ?.optJSONArray("data")
            ?.objects()
            .orEmpty()
    val count = "${entries.size}${if (state.value
            ?.text("nextCursor")
            .orEmpty()
            .isNotEmpty()
    ) {
        "+"
    } else {
        ""
    }}"
    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = 48.dp)
            .testTag("message-queue")
            .clickable(onClick = open)
            .padding(horizontal = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Icon(Icons.AutoMirrored.Filled.PlaylistAdd, null, Modifier.size(18.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(if (state.error != null) "Queue unavailable" else "Queue ($count)", style = MaterialTheme.typography.labelMedium)
        Text(
            entries.firstOrNull()?.let { queueText(queueInput(it)).ifBlank { "Attached content" } } ?: "View queued messages",
            Modifier.weight(1f),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Icon(Icons.Default.ExpandMore, "Open queue", Modifier.size(18.dp))
    }
    if (showing) QueueSheet(repo, server, session, thread, close, canWrite)
}

@Composable
fun QueueSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    close: () -> Unit,
    canWrite: Boolean = true,
) {
    val (query, state) = rememberRemoteData(repo, server, session, "thread/queue/list", queueParams(thread))
    // A refresh invalidates all later pages. Never submit a partial queue order.
    var extra by remember(state.updatedAt) { mutableStateOf<List<JSONObject>>(emptyList()) }
    var cursor by remember(state.updatedAt) { mutableStateOf(state.value?.text("nextCursor").orEmpty()) }
    val entries =
        (
            state.value
                ?.optJSONArray("data")
                ?.objects()
                .orEmpty() + extra
        ).distinctBy(::queueId)
    val edits: QueueEditState = viewModel(key = "queue/$server/$session/$thread")
    var draft by edits.draft
    var editing by edits.editing
    var original by edits.original
    var busy by edits.busy
    var failure by edits.failure
    var discard by remember { mutableStateOf(false) }
    var sendNow by remember { mutableStateOf("") }
    val dirty = draft != queueText(JSONArray(original))
    val scope = edits.viewModelScope

    fun clearEdit() {
        draft = ""
        editing = ""
        original = "[]"
    }

    fun dismiss() {
        if (!busy) {
            if (dirty) {
                discard = true
            } else {
                clearEdit()
                close()
            }
        }
    }

    fun mutate(
        method: String,
        params: JSONObject,
        finishEdit: Boolean = false,
    ) {
        if (busy || !canWrite) return
        busy = true
        failure = ""
        scope.launch {
            runCatching {
                if (finishEdit && editing.isNotEmpty()) {
                    val latest = repo.rpc(server, session, "thread/queue/list", queueParams(thread))
                    // Loaded later pages need a fresh lookup too, without losing the local draft.
                    var page = latest
                    var found: JSONObject? = null
                    val visited = mutableSetOf<String>()
                    while (true) {
                        found = page.optJSONArray("data")?.objects()?.firstOrNull { queueId(it) == editing }
                        if (found != null) break
                        val next = page.text("nextCursor")
                        if (next.isEmpty() || !visited.add(next)) break
                        page = repo.rpc(server, session, "thread/queue/list", queueParams(thread).put("cursor", next))
                    }
                    check(found != null) { "This message is no longer queued. Your draft is kept below." }
                    check(canonicalJson(queueInput(found)) == canonicalJson(JSONArray(original))) {
                        "This message changed on the host. Cancel editing to load the new text. " +
                            "Your draft is kept below."
                    }
                }
                if (method == "thread/queue/start") {
                    fun activeTurn(snapshot: JSONObject) =
                        snapshot
                            .getJSONObject("turns")
                            .getJSONArray("data")
                            .objects()
                            .firstOrNull { it.text("status") == "inProgress" }
                            ?.text("id")
                            .orEmpty()
                    val active = activeTurn(repo.rpc(server, session, "thread/snapshot", obj("threadId" to thread)))
                    if (active.isNotEmpty()) {
                        repo.rpc(server, session, "turn/interrupt", obj("threadId" to thread, "turnId" to active))
                        withTimeout(15_000) {
                            while (activeTurn(repo.rpc(server, session, "thread/snapshot", obj("threadId" to thread))) == active) delay(100)
                        }
                    }
                }
                repo.rpc(server, session, method, params.put("threadId", thread))
            }.onSuccess {
                if (finishEdit) clearEdit()
                repo.data.refresh(query, true)
            }.onFailure { failure = it.message ?: "Queue action failed" }
            busy = false
        }
    }
    NativeSheet("Queued messages", ::dismiss, canDismiss = {
        if (dirty && !busy) discard = true
        if (!dirty && !busy) clearEdit()
        !dirty && !busy
    }) {
        DataStatus(state) { repo.data.refresh(query, true) }
        Text(
            if (canWrite) {
                "Messages wait for the current turn. Send now interrupts that turn and starts the selected message."
            } else {
                "Read only. Enable writing in the chat to change the queue."
            },
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
        LazyColumn(Modifier.weight(1f, fill = false).testTag("queue-items"), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            if (entries.isEmpty() && !state.loading) {
                item {
                    Text("The queue is empty", Modifier.padding(vertical = 16.dp), style = MaterialTheme.typography.bodyMedium)
                }
            }
            itemsIndexed(entries, key = { _, entry -> queueId(entry) }) { index, entry ->
                val id = queueId(entry)
                val input = queueInput(entry)
                val message = queueText(input)
                val attachments = input.objects().count { it.text("type") != "text" }
                Surface(shape = RoundedCornerShape(12.dp), color = MaterialTheme.colorScheme.surfaceContainerLow) {
                    Column(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp)) {
                        Text(
                            "${index + 1}. ${message.ifBlank { "Attached content" }}",
                            maxLines = 4,
                            overflow = TextOverflow.Ellipsis,
                            style = MaterialTheme.typography.bodyMedium,
                        )
                        if (attachments >
                            0
                        ) {
                            Text(
                                "$attachments attached ${if (attachments == 1) "item" else "items"}",
                                style = MaterialTheme.typography.labelSmall,
                            )
                        }
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            TextButton(enabled = canWrite && !busy && editing.isEmpty() && !dirty, onClick = {
                                editing = id
                                original = input.toString()
                                draft = message
                                failure = ""
                            }) { Text("Edit") }
                            TextButton(enabled = canWrite && !busy && editing.isEmpty() && !dirty, onClick = {
                                sendNow = id
                            }) { Text("Send now") }
                            Spacer(Modifier.weight(1f))
                            IconButton(
                                enabled =
                                    canWrite && !busy && editing.isEmpty() && !dirty && index > 0 && cursor.isEmpty(),
                                onClick = {
                                    val ids = entries.map(::queueId).toMutableList()
                                    ids[index] = ids[index - 1]
                                    ids[index - 1] = id
                                    mutate("thread/queue/reorder", obj("queuedSubmissionIds" to JSONArray(ids)))
                                },
                            ) { Icon(Icons.Default.ArrowUpward, "Move message up", Modifier.size(18.dp)) }
                            IconButton(enabled = canWrite && !busy && editing.isEmpty() && !dirty, onClick = {
                                mutate("thread/queue/delete", obj("queuedSubmissionId" to id))
                            }) { Icon(Icons.Default.DeleteOutline, "Remove queued message", Modifier.size(18.dp)) }
                        }
                    }
                }
            }
            if (cursor.isNotEmpty()) {
                item {
                    TextButton(enabled = !busy, onClick = {
                        busy = true
                        val version = state.updatedAt
                        val next = cursor
                        scope.launch {
                            runCatching { repo.rpc(server, session, "thread/queue/list", queueParams(thread).put("cursor", next)) }
                                .onSuccess {
                                    if (repo.data
                                            .observe(query)
                                            .value.updatedAt == version
                                    ) {
                                        extra = extra + it.optJSONArray("data")?.objects().orEmpty()
                                        cursor = it.text("nextCursor")
                                    }
                                }.onFailure { failure = it.message ?: "Could not load the queue" }
                            busy = false
                        }
                    }) { Text("Load more messages") }
                }
            }
        }
        if (canWrite) {
            OutlinedTextField(
                draft,
                { draft = it },
                enabled = !busy,
                label = { Text(if (editing.isEmpty()) "Next instruction" else "Edit instruction") },
                modifier = Modifier.fillMaxWidth().testTag("queue-draft"),
                maxLines = 4,
            )
            if (editing.isNotEmpty() && JSONArray(original).objects().any { it.text("type") != "text" }) {
                Text("Attached items are kept when you save.", style = MaterialTheme.typography.bodySmall)
            }
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                if (editing.isNotEmpty()) {
                    TextButton(enabled = !busy, onClick = {
                        clearEdit()
                        failure = ""
                    }) { Text("Cancel editing") }
                }
                Spacer(Modifier.weight(1f))
                Button(
                    enabled =
                        !busy &&
                            editQueueInput(
                                JSONArray(original),
                                draft,
                            ).length() > 0 &&
                            (editing.isEmpty() || dirty),
                    onClick = {
                        val params = obj("input" to editQueueInput(JSONArray(original), draft))
                        if (editing.isEmpty()) {
                            params.put("clientUserMessageId", UUID.randomUUID().toString())
                        } else {
                            params.put("queuedSubmissionId", editing)
                        }
                        mutate(if (editing.isEmpty()) "thread/queue/add" else "thread/queue/update", params, finishEdit = true)
                    },
                ) {
                    Text(
                        if (busy) {
                            "Working…"
                        } else if (editing.isEmpty()) {
                            "Add to queue"
                        } else {
                            "Save message"
                        },
                    )
                }
            }
        }
    }
    if (sendNow.isNotEmpty()) {
        AlertDialog(
            onDismissRequest = { sendNow = "" },
            title = { Text("Send this message now?") },
            text = { Text("If a turn is running, it will be interrupted.") },
            confirmButton = {
                TextButton(modifier = Modifier.testTag("confirm-queue-send"), onClick = {
                    val id = sendNow
                    sendNow = ""
                    mutate("thread/queue/start", obj("queuedSubmissionId" to id))
                }) { Text("Send now") }
            },
            dismissButton = { TextButton(onClick = { sendNow = "" }) { Text("Cancel") } },
        )
    }
    if (discard) {
        AlertDialog(
            onDismissRequest = { discard = false },
            title = { Text("Discard queue edits?") },
            text = { Text("Your changes have not been saved.") },
            confirmButton = {
                TextButton(onClick = {
                    discard = false
                    clearEdit()
                    close()
                }) { Text("Discard") }
            },
            dismissButton = { TextButton(onClick = { discard = false }) { Text("Keep editing") } },
        )
    }
}
