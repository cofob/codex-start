package wtf.fob.cs.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalWindowInfo
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import org.json.JSONObject
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.workspace.*

@Composable fun SearchField(
    value: String,
    changed: (String) -> Unit,
    hint: String,
    modifier: Modifier = Modifier,
    search: (() -> Unit)? = null,
) {
    val focusManager = androidx.compose.ui.platform.LocalFocusManager.current
    TextField(
        value,
        changed,
        modifier.fillMaxWidth().keyboardTabNavigation(focusManager),
        placeholder = { Text(hint) },
        singleLine = true,
        keyboardOptions =
            androidx.compose.foundation.text
                .KeyboardOptions(imeAction = androidx.compose.ui.text.input.ImeAction.Search),
        keyboardActions =
            androidx.compose.foundation.text
                .KeyboardActions(onSearch = { search?.invoke() }),
        leadingIcon = {
            if (search ==
                null
            ) {
                Icon(Icons.Default.Search, null, Modifier.size(20.dp))
            } else {
                IconButton(onClick = search) { Icon(Icons.Default.Search, "Search", Modifier.size(20.dp)) }
            }
        },
        shape = RoundedCornerShape(28.dp),
        colors =
            TextFieldDefaults.colors(
                focusedContainerColor = MaterialTheme.colorScheme.surfaceContainerLow,
                unfocusedContainerColor = MaterialTheme.colorScheme.surfaceContainerLow,
                focusedIndicatorColor = androidx.compose.ui.graphics.Color.Transparent,
                unfocusedIndicatorColor = androidx.compose.ui.graphics.Color.Transparent,
            ),
    )
}

@Composable fun ProjectRow(
    name: String,
    path: String,
    enabled: Boolean = true,
    open: () -> Unit,
) {
    Row(
        Modifier.fillMaxWidth().clickable(enabled = enabled, onClick = open).padding(vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Surface(shape = RoundedCornerShape(12.dp), color = MaterialTheme.colorScheme.surfaceContainerLow) {
            Icon(Icons.Default.FolderOpen, null, Modifier.padding(11.dp).size(22.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Column(Modifier.weight(1f).padding(horizontal = 12.dp)) {
            Text(name, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Text(
                path,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, null, Modifier.size(20.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

@Composable fun TaskStatus(
    label: String,
    attention: Boolean = false,
    active: Boolean = false,
) {
    val color =
        if (attention) {
            MaterialTheme.colorScheme.error
        } else if (active) {
            MaterialTheme.colorScheme.primary
        } else {
            MaterialTheme.colorScheme.onSurfaceVariant
        }
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        if (active) {
            CircularProgressIndicator(Modifier.size(10.dp), color = color, strokeWidth = 1.5.dp)
        } else {
            Icon(if (attention) Icons.Default.Pending else Icons.Default.CheckCircleOutline, null, Modifier.size(14.dp), tint = color)
        }
        Text(label, color = color, style = MaterialTheme.typography.labelMedium)
    }
}

@Composable fun ChatRow(
    chat: JSONObject,
    subtitle: String = "",
    open: () -> Unit,
) {
    val active = chat.optJSONObject("status")?.text("type") == "active"
    Row(Modifier.fillMaxWidth().clickable(onClick = open).padding(vertical = 15.dp), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f).padding(end = 12.dp)) {
            Text(chatTitle(chat), style = MaterialTheme.typography.bodyLarge, maxLines = 2, overflow = TextOverflow.Ellipsis)
            if (subtitle.isNotEmpty()) {
                Text(
                    subtitle,
                    Modifier.padding(top = 4.dp),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        if (active) {
            TaskStatus("Working", active = true)
        } else {
            Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, null, Modifier.size(18.dp), tint = MaterialTheme.colorScheme.outline)
        }
    }
}

@Composable fun ConversationMessage(
    item: ConversationItem,
    repo: RemoteRepository,
    server: String,
    session: String,
    copy: () -> Unit,
) {
    val user = item.type == "userMessage"
    val assistant = item.type == "agentMessage"
    var selection by remember(item.id) { mutableStateOf<String?>(null) }
    Column(Modifier.fillMaxWidth(), horizontalAlignment = if (user) Alignment.End else Alignment.Start) {
        if (user) {
            Surface(
                Modifier.fillMaxWidth(0.88f),
                color = MaterialTheme.colorScheme.surfaceContainerHigh,
                shape = RoundedCornerShape(22.dp, 22.dp, 5.dp, 22.dp),
            ) {
                Column(Modifier.padding(horizontal = 16.dp, vertical = 12.dp)) {
                    if (item.text.isNotEmpty()) Markdown(item.text)
                    item.details?.let { ConversationImages(repo, server, session, it) }
                }
            }
        } else if (assistant) {
            if (item.text.isNotEmpty()) {
                Box(
                    Modifier
                        .combinedClickable(onClick = {
                        }, onLongClick = { selection = item.text })
                        .testTag("agent-answer"),
                ) { Markdown(item.text, onLongClick = { selection = item.text }) }
            }
            item.details?.let { ConversationImages(repo, server, session, it) }
        } else {
            ConversationActivity(ConversationBlock(listOf(item)), false) { copy() }
        }
        if ((user || assistant) && item.text.isNotEmpty()) {
            Row {
                IconButton(onClick = copy, Modifier.size(48.dp)) {
                    Icon(
                        Icons.Default.ContentCopy,
                        if (assistant) "Copy answer" else "Copy message",
                        Modifier.size(16.dp),
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                if (assistant) TextButton(onClick = { selection = item.text }) { Text("Select text") }
            }
        }
    }
    selection?.let { AnswerSelection(it) { selection = null } }
}

@Composable fun SettingsGroup(
    title: String = "",
    content: @Composable ColumnScope.() -> Unit,
) {
    Column(Modifier.fillMaxWidth().padding(bottom = 24.dp)) {
        if (title.isNotEmpty()) {
            Text(
                title,
                Modifier.padding(start = 16.dp, bottom = 10.dp),
                style = MaterialTheme.typography.labelLarge,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Surface(shape = RoundedCornerShape(28.dp), color = MaterialTheme.colorScheme.surfaceContainerLow) {
            Column(Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(2.dp), content = content)
        }
    }
}

@Composable fun SettingsRow(
    title: String,
    detail: String = "",
    icon: ImageVector? = null,
    enabled: Boolean = true,
    trailing: (@Composable () -> Unit)? = null,
    onClick: (() -> Unit)? = null,
) {
    ListItem(
        modifier =
            Modifier
                .fillMaxWidth()
                .then(
                    if (onClick !=
                        null
                    ) {
                        Modifier.clickable(enabled = enabled, onClick = onClick)
                    } else {
                        Modifier
                    },
                ).padding(vertical = 4.dp),
        colors =
            ListItemDefaults.colors(
                containerColor = MaterialTheme.colorScheme.surfaceContainerLow,
                headlineColor = if (enabled) MaterialTheme.colorScheme.onSurface else MaterialTheme.colorScheme.onSurfaceVariant,
            ),
        headlineContent = { Text(title, style = MaterialTheme.typography.bodyLarge) },
        supportingContent =
            if (detail.isEmpty()) {
                null
            } else {
                { Text(detail, style = MaterialTheme.typography.bodyMedium) }
            },
        leadingContent = icon?.let { { Icon(it, null, Modifier.size(24.dp)) } },
        trailingContent =
            trailing
                ?: if (onClick == null) {
                    null
                } else {
                    { Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, null, Modifier.size(20.dp)) }
                },
    )
}

@Composable fun EmptyState(
    title: String,
    detail: String,
    icon: ImageVector = Icons.Default.ChatBubbleOutline,
    action: (@Composable () -> Unit)? = null,
) {
    Column(
        Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 40.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Surface(shape = CircleShape, color = MaterialTheme.colorScheme.surfaceContainerLow) {
            Icon(icon, null, Modifier.padding(20.dp).size(32.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Text(title, style = MaterialTheme.typography.titleLarge, textAlign = androidx.compose.ui.text.style.TextAlign.Center)
        Text(
            detail,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = androidx.compose.ui.text.style.TextAlign.Center,
        )
        action?.invoke()
    }
}

@Composable fun DataStatus(
    state: RemoteDataState,
    retry: () -> Unit,
) {
    if (state.loading) LinearProgressIndicator(Modifier.fillMaxWidth().padding(vertical = 8.dp))
    if (state.error != null) {
        Surface(shape = MaterialTheme.shapes.medium, color = MaterialTheme.colorScheme.errorContainer) {
            Row(Modifier.padding(start = 16.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    if (state.value !=
                        null
                    ) {
                        "Showing saved data. ${state.error}"
                    } else {
                        state.error
                    },
                    Modifier.weight(1f),
                    style = MaterialTheme.typography.bodySmall,
                )
                TextButton(onClick = retry) { Text("Retry") }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun NativeSheet(
    title: String,
    close: () -> Unit,
    scrollContent: Boolean = false,
    canDismiss: () -> Boolean = { true },
    content: @Composable ColumnScope.() -> Unit,
) {
    val activity = androidx.activity.compose.LocalActivity.current
    val dark = MaterialTheme.colorScheme.background.luminance() < 0.5f
    val currentDark by rememberUpdatedState(dark)
    val currentCanDismiss by rememberUpdatedState(canDismiss)
    val density = LocalDensity.current
    val compact =
        with(density) {
            LocalWindowInfo.current.containerSize.height
                .toDp() < 600.dp
        } ||
            WindowInsets.ime.getBottom(density) > 0
    DisposableEffect(activity) {
        onDispose {
            // A sheet can restore the window flags saved before an appearance change.
            activity?.window?.let { window ->
                window.decorView.post {
                    androidx.core.view.WindowCompat.getInsetsController(window, window.decorView).apply {
                        isAppearanceLightStatusBars = !currentDark
                        isAppearanceLightNavigationBars = !currentDark
                    }
                }
            }
        }
    }
    ModalBottomSheet(
        onDismissRequest = close,
        sheetState =
            rememberModalBottomSheetState(skipPartiallyExpanded = true, confirmValueChange = {
                it != SheetValue.Hidden ||
                    currentCanDismiss()
            }),
        containerColor = MaterialTheme.colorScheme.surfaceContainer,
        dragHandle = { BottomSheetDefaults.DragHandle(Modifier.padding(vertical = if (compact) 6.dp else 16.dp)) },
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .heightIn(max = 720.dp)
                .imePadding()
                .padding(horizontal = if (compact) 16.dp else 20.dp),
        ) {
            Row(Modifier.fillMaxWidth().padding(bottom = if (compact) 4.dp else 12.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    title,
                    Modifier.weight(1f),
                    style = MaterialTheme.typography.titleLarge,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
                IconButton(onClick = close) { Icon(Icons.Default.Close, "Close $title") }
            }
            if (scrollContent) {
                Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()), content = content)
            } else {
                content()
            }
            Spacer(Modifier.height(if (compact) 4.dp else 16.dp))
        }
    }
}
