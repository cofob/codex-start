package wtf.fob.cs.workspace

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*

enum class ChatWorkState(
    val label: String,
    val attention: Boolean = false,
) {
    Approval("Needs approval", true),
    Input("Needs input", true),
    Error("Task error", true),
    Running("Working"),
    Idle("Idle"),
    Unknown(""),
}

/** Idle and unloaded threads are not proof of a completed task or a result to review. */
fun chatWorkState(chat: JSONObject): ChatWorkState {
    val status = chat.optJSONObject("status") ?: return ChatWorkState.Unknown
    val flags = status.optJSONArray("activeFlags") ?: JSONArray()
    return when (status.text("type")) {
        "active" ->
            when {
                (0 until flags.length()).any { flags.optString(it) == "waitingOnApproval" } -> ChatWorkState.Approval
                (0 until flags.length()).any { flags.optString(it) == "waitingOnUserInput" } -> ChatWorkState.Input
                else -> ChatWorkState.Running
            }
        "systemError" -> ChatWorkState.Error
        "idle" -> ChatWorkState.Idle
        else -> ChatWorkState.Unknown
    }
}

enum class WorkFilter(
    val label: String,
) {
    All("All"),
    Attention("Needs attention"),
    Running("Running"),
}

data class WorkGroup(
    val title: String,
    val chats: List<RecentChat>,
)

internal fun workChatKey(chat: RecentChat): String = chatIdentity(chat.session, chat.chat)

fun workChats(home: JSONObject?): List<RecentChat> =
    (home?.optJSONArray("projectChats") ?: home?.optJSONArray("recent"))
        ?.objects()
        .orEmpty()
        .mapNotNull { item ->
            val session = item.optJSONObject("session") ?: return@mapNotNull null
            val chat = item.optJSONObject("chat") ?: return@mapNotNull null
            if (chat.text("id").isBlank()) null else RecentChat(session, chat)
        }.distinctBy(::workChatKey)
        .sortedByDescending { it.chat.optLong("updatedAt") }

fun workGroups(
    chats: List<RecentChat>,
    search: String,
    filter: WorkFilter,
): List<WorkGroup> {
    val matching =
        chats.filter { item ->
            val state = chatWorkState(item.chat)
            val matches =
                listOf(
                    chatTitle(item.chat),
                    item.session.text("cwd"),
                    item.session.text("profile"),
                    item.chat.text("historyHome"),
                    chatOrigin(item.chat),
                ).any { it.contains(search.trim(), ignoreCase = true) }
            matches &&
                when (filter) {
                    WorkFilter.All -> true
                    WorkFilter.Attention -> state.attention
                    WorkFilter.Running -> state == ChatWorkState.Running
                }
        }
    return listOf(
        WorkGroup("Needs attention", matching.filter { chatWorkState(it.chat).attention }),
        WorkGroup("In progress", matching.filter { chatWorkState(it.chat) == ChatWorkState.Running }),
        WorkGroup(
            "Pinned",
            matching.filter {
                it.chat.optBoolean("isPinned") &&
                    chatWorkState(it.chat) in setOf(ChatWorkState.Idle, ChatWorkState.Unknown)
            },
        ),
        WorkGroup(
            "Recent",
            matching.filter {
                !it.chat.optBoolean("isPinned") &&
                    chatWorkState(it.chat) in setOf(ChatWorkState.Idle, ChatWorkState.Unknown)
            },
        ),
    ).filter { it.chats.isNotEmpty() }
}

@Composable
fun WorkOverview(
    home: JSONObject?,
    search: String,
    loading: Boolean,
    connected: Boolean,
    open: (RecentChat) -> Unit,
    chooseProject: () -> Unit,
    workSupported: Boolean = true,
    newWork: () -> Unit = {},
    historyComplete: Boolean = false,
    hasMore: Boolean = false,
    loadMore: () -> Unit = {},
    showActions: Boolean = true,
) {
    val chats = workChats(home)
    var filter by rememberSaveable { mutableStateOf(WorkFilter.All) }
    val groups = workGroups(chats, search, filter)
    Column(Modifier.fillMaxSize().testTag("work-overview")) {
        LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            items(WorkFilter.entries) { option ->
                val count =
                    when (option) {
                        WorkFilter.All -> chats.size
                        WorkFilter.Attention -> chats.count { chatWorkState(it.chat).attention }
                        WorkFilter.Running -> chats.count { chatWorkState(it.chat) == ChatWorkState.Running }
                    }
                FilterChip(
                    selected = filter == option,
                    onClick = { filter = option },
                    label = { Text("${option.label} $count", maxLines = 1) },
                    modifier = Modifier.testTag("work-filter-${option.name}"),
                )
            }
        }
        LazyColumn(Modifier.weight(1f).testTag("work-list"), contentPadding = PaddingValues(bottom = 16.dp)) {
            if (groups.isEmpty() && !loading) {
                item {
                    EmptyState(
                        when {
                            search.isNotBlank() -> "No matching work"
                            filter == WorkFilter.Attention -> "No tasks need attention"
                            filter == WorkFilter.Running -> "No tasks are running"
                            else -> "What would you like to work on?"
                        },
                        when {
                            search.isNotBlank() -> "Search by chat, project, or profile."
                            chats.isNotEmpty() -> "Choose All to see your recent chats."
                            !connected -> "Connect this computer to load your tasks."
                            else -> "Start new work or choose a project. Files and tools stay on your computer."
                        },
                        if (search.isNotBlank()) Icons.Default.SearchOff else Icons.Default.WorkOutline,
                    )
                }
            }
            groups.forEach { group ->
                item(key = "section/${group.title}") {
                    Text(
                        group.title,
                        Modifier.padding(top = 20.dp, bottom = 4.dp),
                        style = MaterialTheme.typography.labelLarge,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                items(group.chats, key = ::workChatKey) { item -> WorkChatRow(item) { open(item) } }
            }
            if (hasMore) {
                item {
                    TextButton(
                        onClick = loadMore,
                        enabled = connected && !loading,
                        modifier = Modifier.fillMaxWidth().testTag("history-load-more"),
                    ) {
                        Text(if (loading) "Loading…" else "Load more chats")
                    }
                }
            }
            if (chats.isNotEmpty()) {
                item {
                    Text(
                        if (historyComplete) {
                            "Local Codex history from this computer. Saved copies show their home. " +
                                "Cloud-only ChatGPT chats are not in this list."
                        } else {
                            "Recent tasks from this computer. Open a project for older chats."
                        },
                        Modifier.padding(vertical = 16.dp),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
        if (showActions) {
            Row(Modifier.fillMaxWidth().padding(vertical = 8.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilledTonalButton(
                    onClick = newWork,
                    enabled = connected && workSupported,
                    modifier = Modifier.weight(1f).testTag("work-new-chat"),
                ) {
                    Icon(Icons.Default.Add, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text("New work")
                }
                TextButton(onClick = chooseProject, enabled = connected, modifier = Modifier.testTag("work-choose-project")) {
                    Text("Use a project")
                }
            }
        }
        if (showActions && connected && !workSupported) {
            Text(
                "Update codex-start on this host to start work without a project.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun WorkChatRow(
    item: RecentChat,
    open: () -> Unit,
) {
    val status = chatWorkState(item.chat)
    Column(Modifier.fillMaxWidth().clickable(onClick = open).padding(vertical = 10.dp)) {
        Text(chatTitle(item.chat), style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
        Text(
            listOf(projectTitle(item.session.text("cwd")), item.session.text("profile"))
                .filter { it.isNotBlank() }
                .joinToString(" · "),
            Modifier.padding(top = 4.dp),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        if (item.chat.text("historyHome").isNotBlank()) {
            Text(
                listOf(chatOrigin(item.chat), item.chat.text("historyHome"), "Saved")
                    .filter { it.isNotBlank() }
                    .joinToString(" · "),
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        if (status != ChatWorkState.Unknown && status != ChatWorkState.Idle) {
            Row(Modifier.padding(top = 6.dp), verticalAlignment = Alignment.CenterVertically) {
                TaskStatus(status.label, attention = status.attention, active = status == ChatWorkState.Running)
            }
        }
    }
}
