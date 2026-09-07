package wtf.fob.cs.conversation

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
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

@Composable
fun ConversationActivity(
    block: ConversationBlock,
    working: Boolean,
    copy: (String) -> Unit,
) {
    var expanded by rememberSaveable(block.id) { mutableStateOf(false) }
    var visible by remember(block.id) { mutableIntStateOf(20) }
    val thinking = block.items.count { it.type == "reasoning" }
    val tools = block.items.size - thinking
    val title =
        listOfNotNull(
            "Thinking".takeIf { thinking > 0 },
            "$tools ${if (tools == 1) "tool call" else "tool calls"}".takeIf { tools > 0 },
        ).joinToString(" · ")
    Surface(
        Modifier.fillMaxWidth().testTag("activity-block"),
        shape = RoundedCornerShape(12.dp),
        color = MaterialTheme.colorScheme.surfaceContainerLow,
    ) {
        Column {
            Row(
                Modifier
                    .fillMaxWidth()
                    .heightIn(
                        min = 48.dp,
                    ).clickable { expanded = !expanded }
                    .padding(horizontal = 12.dp, vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (working) {
                    CircularProgressIndicator(Modifier.size(12.dp), strokeWidth = 1.5.dp)
                } else {
                    Icon(Icons.Default.Check, null, Modifier.size(14.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
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
                        Column(Modifier.padding(horizontal = 12.dp, vertical = 4.dp)) {
                            Row(
                                Modifier.fillMaxWidth().heightIn(min = 48.dp).clickable { details = !details },
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
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
                                if (body.isNotBlank()) Markdown(body, size = 13f)
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
