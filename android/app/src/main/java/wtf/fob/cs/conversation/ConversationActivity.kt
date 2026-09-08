package wtf.fob.cs.conversation

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*

data class ConversationBlock(
    val items: List<ConversationItem>,
) {
    val id get() = items.first().id
    val activity get() = items.first().type !in setOf("userMessage", "agentMessage")
}

/** Messages remain separate; consecutive activity shares one disclosure. */
fun conversationBlocks(items: List<ConversationItem>): List<ConversationBlock> {
    val groups = mutableListOf<MutableList<ConversationItem>>()
    for (item in items) {
        if (item.type !in setOf("userMessage", "agentMessage") &&
            groups
                .lastOrNull()
                ?.lastOrNull()
                ?.type
                ?.let { it !in setOf("userMessage", "agentMessage") } == true
        ) {
            groups.last().add(item)
        } else {
            groups.add(mutableListOf(item))
        }
    }
    return groups.map { ConversationBlock(it) }
}

internal fun activityLabel(item: ConversationItem): String =
    when (item.type) {
        "reasoning" -> "Thinking"
        "commandExecution" -> "Command"
        "fileChange" -> "File changes"
        "plan" -> "Plan"
        "mcpToolCall", "dynamicToolCall" -> item.details?.text("tool", "Tool call") ?: "Tool call"
        "webSearch" -> "Web search"
        else -> humanize(item.type)
    }

internal fun activityText(item: ConversationItem): String {
    val details = item.details ?: return item.text
    return when (item.type) {
        "commandExecution" ->
            listOf(
                details.text("command"),
                details.text("aggregatedOutput"),
            ).filter { it.isNotBlank() }.joinToString("\n\n")
        "fileChange" ->
            details
                .optJSONArray("changes")
                ?.objects()
                ?.joinToString("\n\n") {
                    it.text("path") + "\n" + it.text("diff")
                }.orEmpty()
                .ifBlank { item.text }
        else -> item.text
    }.ifBlank { item.text }
}

internal fun activitySummary(items: List<ConversationItem>): String {
    fun count(
        type: String,
        single: String,
        plural: String = "${single}s",
    ): String? {
        val count = items.count { it.type == type }
        return if (count == 0) null else "$count ${if (count == 1) single else plural}"
    }
    val known = setOf("reasoning", "commandExecution", "fileChange", "webSearch", "plan")
    val other = items.count { it.type !in known }
    return listOfNotNull(
        "Thinking".takeIf { items.any { it.type == "reasoning" } },
        count("commandExecution", "command"),
        count("fileChange", "file change"),
        count("webSearch", "search", "searches"),
        "Plan".takeIf { items.any { it.type == "plan" } },
        "$other ${if (other == 1) "tool call" else "tool calls"}".takeIf { other > 0 },
    ).joinToString(" · ")
}

internal fun activityFailed(item: ConversationItem): Boolean =
    item.details?.let {
        it.text("status") in setOf("failed", "declined") ||
            (it.has("exitCode") && !it.isNull("exitCode") && it.optInt("exitCode") != 0)
    } == true

@Composable
fun ConversationActivity(
    block: ConversationBlock,
    working: Boolean,
    copy: (String) -> Unit,
) {
    var expanded by rememberSaveable(block.id) { mutableStateOf(false) }
    var visible by remember(block.id) { mutableIntStateOf(20) }
    val title = activitySummary(block.items)
    val failed = block.items.any(::activityFailed)
    Surface(
        Modifier.fillMaxWidth().testTag("activity-block"),
        shape = RoundedCornerShape(12.dp),
        color = Color.Transparent,
    ) {
        Column {
            Row(
                Modifier
                    .fillMaxWidth()
                    .heightIn(
                        min = 48.dp,
                    ).clickable { expanded = !expanded }
                    .padding(horizontal = 4.dp, vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (working) {
                    CircularProgressIndicator(Modifier.size(12.dp), strokeWidth = 1.5.dp)
                } else {
                    Icon(
                        if (failed) Icons.Default.ErrorOutline else Icons.Default.Check,
                        if (failed) "Activity needs review" else null,
                        Modifier.size(14.dp),
                        tint = if (failed) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Text(
                    title,
                    Modifier.weight(1f),
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Icon(
                    if (expanded) Icons.Default.ExpandLess else Icons.Default.ExpandMore,
                    if (expanded) "Collapse activity" else "Expand activity",
                    Modifier.size(18.dp),
                )
            }
            if (expanded) {
                if (block.items.size > visible) TextButton(onClick = { visible += 20 }) { Text("Show earlier activity") }
                block.items.takeLast(visible).forEach { item ->
                    key(item.id) {
                        var details by rememberSaveable { mutableStateOf(false) }
                        val body = activityText(item)
                        Column(Modifier.padding(start = 20.dp, end = 4.dp)) {
                            Row(
                                Modifier.fillMaxWidth().heightIn(min = 48.dp).clickable { details = !details },
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                Icon(
                                    when (item.type) {
                                        "commandExecution" -> Icons.Default.Terminal
                                        "fileChange" -> Icons.Default.EditNote
                                        "webSearch" -> Icons.Default.Search
                                        "reasoning" -> Icons.Default.AutoAwesome
                                        else -> Icons.Default.Build
                                    },
                                    null,
                                    Modifier.padding(end = 8.dp).size(16.dp),
                                    tint =
                                        if (activityFailed(
                                                item,
                                            )
                                        ) {
                                            MaterialTheme.colorScheme.error
                                        } else {
                                            MaterialTheme.colorScheme.onSurfaceVariant
                                        },
                                )
                                Column(Modifier.weight(1f)) {
                                    Text(activityLabel(item), style = MaterialTheme.typography.labelMedium)
                                    if (body.isNotBlank() && !details) {
                                        Text(
                                            body,
                                            maxLines = 1,
                                            overflow = TextOverflow.Ellipsis,
                                            style = MaterialTheme.typography.bodySmall,
                                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                                        )
                                    }
                                }
                                Icon(
                                    if (details) Icons.Default.ExpandLess else Icons.Default.ExpandMore,
                                    "Activity details",
                                    Modifier.size(16.dp),
                                )
                            }
                            if (details) {
                                if (body.isNotBlank()) {
                                    if (item.type == "commandExecution") {
                                        Surface(color = MaterialTheme.colorScheme.surfaceContainerLow, shape = RoundedCornerShape(12.dp)) {
                                            SelectionContainer {
                                                Text(
                                                    streamTail(body, 32 * 1024),
                                                    Modifier.fillMaxWidth().padding(12.dp),
                                                    fontFamily = FontFamily.Monospace,
                                                    style = MaterialTheme.typography.bodySmall,
                                                )
                                            }
                                        }
                                        if (body.length > 32 * 1024) {
                                            Text(
                                                "Showing the last 32 KiB. Copy includes the full loaded output.",
                                                style = MaterialTheme.typography.labelSmall,
                                            )
                                        }
                                    } else {
                                        Markdown(body, size = 14f, selectable = true)
                                    }
                                }
                                if (item.details == null && body.isNotBlank()) TextButton(onClick = { copy(body) }) { Text("Copy") }
                                item.details?.let { value ->
                                    var raw by remember { mutableStateOf(false) }
                                    Row {
                                        TextButton(onClick = { copy(body.ifBlank { value.toString(2) }) }) { Text("Copy") }
                                        TextButton(onClick = { raw = !raw }) { Text(if (raw) "Hide raw details" else "Raw details") }
                                    }
                                    if (raw) ValueView(value)
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
