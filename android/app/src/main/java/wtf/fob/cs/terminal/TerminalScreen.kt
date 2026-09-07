package wtf.fob.cs.terminal

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import androidx.activity.compose.LocalActivity
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.compose.ui.window.DialogWindowProvider
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.ViewModelStoreOwner
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.*
import org.json.JSONObject
import wtf.fob.cs.data.*

@Composable fun TerminalScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    val servers by repo.servers.collectAsState()
    if (servers.firstOrNull { it.id == server }?.capabilities?.contains("terminals") != true) {
        Column {
            Text("Update the host daemon for terminal tabs and host shells.", style = MaterialTheme.typography.bodySmall)
            if (session.text("id").isNotEmpty()) LegacyTerminalScreen(repo, server, session)
        }
        return
    }
    val owner = LocalActivity.current as ViewModelStoreOwner
    val workspace: TerminalWorkspace = viewModel(viewModelStoreOwner = owner, key = "terminals-$server") { TerminalWorkspace(repo, server) }
    var fullscreen by rememberSaveable { mutableStateOf(false) }
    LaunchedEffect(workspace) { workspace.refresh() }
    if (fullscreen) {
        Dialog(onDismissRequest = {
            fullscreen = false
        }, properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false)) {
            val view = LocalView.current
            DisposableEffect(view) {
                (view.parent as? DialogWindowProvider)?.window?.let { window ->
                    window.setLayout(android.view.ViewGroup.LayoutParams.MATCH_PARENT, android.view.ViewGroup.LayoutParams.MATCH_PARENT)
                    window.clearFlags(android.view.WindowManager.LayoutParams.FLAG_DIM_BEHIND)
                    @Suppress("DEPRECATION")
                    window.statusBarColor = android.graphics.Color.rgb(16, 20, 24)
                    androidx.core.view.WindowCompat
                        .getInsetsController(window, view)
                        .isAppearanceLightStatusBars = false
                    androidx.core.view.WindowCompat
                        .getInsetsController(window, view)
                        .isAppearanceLightNavigationBars = false
                }
                onDispose {}
            }
            Surface(
                Modifier.fillMaxSize(),
                color =
                    androidx.compose.ui.graphics
                        .Color(0xff101418),
            ) {
                Box(Modifier.fillMaxSize().safeDrawingPadding().imePadding()) {
                    TerminalWindows(workspace, session, true) { fullscreen = false }
                }
            }
        }
    } else {
        TerminalWindows(workspace, session, false) { fullscreen = true }
    }
}

@Composable private fun TerminalWindows(
    workspace: TerminalWorkspace,
    session: JSONObject,
    fullscreen: Boolean,
    toggleFullscreen: () -> Unit,
) {
    MaterialTheme(
        colorScheme =
            darkColorScheme(
                background =
                    androidx.compose.ui.graphics
                        .Color(0xff101418),
            ),
    ) {
        Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
            TerminalWindowContent(workspace, session, fullscreen, toggleFullscreen)
        }
    }
}

@Composable private fun TerminalWindowContent(
    workspace: TerminalWorkspace,
    session: JSONObject,
    fullscreen: Boolean,
    toggleFullscreen: () -> Unit,
) {
    var newTerminal by remember { mutableStateOf(false) }
    var menu by remember { mutableStateOf(false) }
    var rename by remember { mutableStateOf(false) }
    var closing by remember { mutableStateOf(false) }
    var functionKeys by rememberSaveable { mutableStateOf(false) }
    var font by rememberSaveable { mutableIntStateOf(14) }
    var ctrl by remember { mutableStateOf(false) }
    var alt by remember { mutableStateOf(false) }
    var surface by remember { mutableStateOf<TerminalView?>(null) }
    val context = LocalContext.current
    val clipboard = remember { context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager }
    val selected = workspace.terminals.firstOrNull { it.text("id") == workspace.selected }
    val exitFocus = remember { FocusRequester() }
    val tabs = rememberLazyListState()
    LaunchedEffect(ctrl, alt, surface) { surface?.modifiers(ctrl, alt) }
    LaunchedEffect(workspace.selected) {
        ctrl = false
        alt = false
        val index = workspace.terminals.indexOfFirst { it.text("id") == workspace.selected }
        if (index >= 0) tabs.animateScrollToItem(index)
    }
    Column(Modifier.fillMaxSize()) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            LazyRow(Modifier.weight(1f), state = tabs, verticalAlignment = Alignment.CenterVertically) {
                items(workspace.terminals, key = { it.text("id") }) { terminal ->
                    FilterChip(
                        selected = terminal.text("id") == workspace.selected,
                        onClick = { workspace.selected = terminal.text("id") },
                        label = {
                            Text(
                                terminal.text("name"),
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                modifier = Modifier.widthIn(max = 160.dp),
                            )
                        },
                        leadingIcon = {
                            Icon(
                                if (terminal.text("target") ==
                                    "host"
                                ) {
                                    Icons.Outlined.Computer
                                } else {
                                    Icons.Outlined.Layers
                                },
                                null,
                                Modifier.size(16.dp),
                            )
                        },
                        modifier = Modifier.padding(end = 4.dp),
                    )
                }
            }
            IconButton(onClick = {
                newTerminal = true
            }, enabled = !workspace.busy && workspace.terminals.size < 8) { Icon(Icons.Outlined.Add, "New terminal") }
            IconButton(onClick = {
                surface?.requestFocus()
                surface?.focusTerminal()
            }, enabled = surface != null, modifier = Modifier.focusRequester(exitFocus)) {
                Icon(Icons.Outlined.Keyboard, "Keyboard")
            }
            Box {
                IconButton(onClick = { menu = true }) { Icon(Icons.Outlined.MoreVert, "Terminal options") }
                DropdownMenu(menu, { menu = false }) {
                    fun item(
                        label: String,
                        enabled: Boolean = true,
                        action: () -> Unit,
                    ): @Composable () -> Unit =
                        {
                            DropdownMenuItem(text = { Text(label) }, enabled = enabled, onClick = {
                                menu = false
                                action()
                            })
                        }
                    item(if (fullscreen) "Exit full screen" else "Full screen", action = toggleFullscreen)()
                    item("Rename", selected != null) { rename = true }()
                    item("Copy selection or screen", surface != null) {
                        surface?.copyText { clipboard.setPrimaryClip(ClipData.newPlainText("Terminal", it)) }
                    }()
                    item("Paste", surface != null && selected?.text("status") == "running") {
                        clipboard.primaryClip?.getItemAt(0)?.coerceToText(context)?.toString()?.let { text ->
                            if (text.toByteArray().size <= 64 * 1024) {
                                surface?.paste(text)
                            } else {
                                workspace.failure = "Paste is limited to 64 KB. Use file upload for larger text."
                            }
                        }
                    }()
                    item(if (functionKeys) "Hide function keys" else "Show function keys") { functionKeys = !functionKeys }()
                    item("Larger text", font < 24) { font++ }()
                    item("Smaller text", font > 10) { font-- }()
                    item("Refresh terminals") { workspace.refresh() }()
                    item("Close terminal", selected != null && !workspace.busy) { closing = true }()
                }
            }
        }
        if (workspace.busy) LinearProgressIndicator(Modifier.fillMaxWidth())
        if (workspace.failure.isNotEmpty()) {
            Text(
                workspace.failure,
                color = MaterialTheme.colorScheme.error,
                style = MaterialTheme.typography.bodySmall,
                modifier = Modifier.padding(8.dp),
            )
        }
        if (selected == null) {
            Column(
                Modifier.weight(1f).fillMaxWidth(),
                verticalArrangement = Arrangement.Center,
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Icon(Icons.Outlined.Terminal, null, Modifier.size(32.dp))
                Text(
                    "Your shells stay open when you leave this screen.",
                    modifier = Modifier.padding(16.dp),
                    style = MaterialTheme.typography.bodyMedium,
                )
                Button(onClick = { newTerminal = true }, enabled = !workspace.busy) { Text("New terminal") }
            }
        } else {
            key(workspace.selected) {
                TerminalPane(
                    workspace,
                    selected,
                    font,
                    Modifier.weight(1f),
                    onSurface = { surface = it },
                    onExit = { exitFocus.requestFocus() },
                    onModifiersUsed = {
                        ctrl = false
                        alt = false
                    },
                )
            }
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                listOf(
                    "Esc" to "\u001b",
                    "Tab" to "\t",
                    "Ctrl" to "",
                    "Alt" to "",
                    "←" to "\u001b[D",
                    "↓" to "\u001b[B",
                    "↑" to "\u001b[A",
                    "→" to "\u001b[C",
                ).forEach { (label, value) ->
                    TextButton(
                        onClick = {
                            when (label) {
                                "Ctrl" -> ctrl = !ctrl
                                "Alt" -> alt = !alt
                                else -> surface?.sendKey(value)
                            }
                            surface?.modifiers(ctrl, alt)
                        },
                        enabled = selected.text("status") == "running",
                        modifier = Modifier.weight(1f),
                        contentPadding = PaddingValues(0.dp),
                        colors =
                            ButtonDefaults.textButtonColors(
                                containerColor =
                                    if (label == "Ctrl" &&
                                        ctrl ||
                                        label == "Alt" &&
                                        alt
                                    ) {
                                        MaterialTheme.colorScheme.secondaryContainer
                                    } else {
                                        androidx.compose.ui.graphics.Color.Transparent
                                    },
                            ),
                    ) { Text(label) }
                }
            }
            if (functionKeys) {
                Row(Modifier.fillMaxWidth().horizontalScroll(rememberScrollState())) {
                    val keys =
                        listOf(
                            "Home" to "\u001b[H",
                            "End" to "\u001b[F",
                            "PgUp" to "\u001b[5~",
                            "PgDn" to "\u001b[6~",
                            "Del" to "\u001b[3~",
                        ) +
                            (1..12).map {
                                "F$it" to
                                    terminalKeyInput(android.view.KeyEvent.KEYCODE_F1 + it - 1, false, false, false, false)!!
                            }
                    keys.forEach { (label, value) -> TextButton(onClick = { surface?.sendKey(value) }) { Text(label) } }
                }
            }
        }
    }
    if (newTerminal) {
        NewTerminalDialog(session, onDismiss = { newTerminal = false }) { target, cwd, command ->
            newTerminal = false
            workspace.open(target, session.text("id"), cwd, command)
        }
    }
    if (rename && selected != null) {
        var name by remember(selected.text("id")) { mutableStateOf(selected.text("name")) }
        AlertDialog(onDismissRequest = { rename = false }, title = { Text("Rename terminal") }, text = {
            OutlinedTextField(name, { name = it.take(80) }, label = { Text("Name") }, singleLine = true)
        }, confirmButton = {
            TextButton(onClick = {
                workspace.rename(selected.text("id"), name.trim())
                rename = false
            }, enabled = name.isNotBlank()) { Text("Save") }
        }, dismissButton = {
            TextButton(onClick = {
                rename =
                    false
            }) { Text("Cancel") }
        })
    }
    if (closing &&
        selected != null
    ) {
        AlertDialog(onDismissRequest = { closing = false }, title = { Text("Close ${selected.text("name") }?") }, text = {
            Text("This stops the shell and its foreground program. Unsaved terminal work can be lost.")
        }, confirmButton = {
            TextButton(onClick = {
                workspace.close(selected.text("id"))
                closing = false
            }) { Text("Close terminal") }
        }, dismissButton = {
            TextButton(onClick = {
                closing =
                    false
            }) { Text("Cancel") }
        })
    }
}

@Composable private fun NewTerminalDialog(
    session: JSONObject,
    onDismiss: () -> Unit,
    start: (String, String, String) -> Unit,
) {
    var target by remember { mutableStateOf(if (session.text("id").isEmpty()) "host" else "session") }
    var cwd by remember { mutableStateOf(session.text("cwd")) }
    var command by remember { mutableStateOf("") }
    AlertDialog(onDismissRequest = onDismiss, title = { Text("New terminal") }, text = {
        Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilterChip(
                    target == "session",
                    { target = "session" },
                    enabled = session.text("id").isNotEmpty(),
                    label = { Text("Container / session") },
                )
                FilterChip(target == "host", { target = "host" }, label = { Text("Host") })
            }
            Text(
                if (target ==
                    "host"
                ) {
                    "Shell on the host. It has the daemon user's permissions."
                } else {
                    "Shell in this Codex session's environment."
                },
                style = MaterialTheme.typography.bodySmall,
            )
            if (target == "host") OutlinedTextField(cwd, { cwd = it }, label = { Text("Directory (blank = home)") }, singleLine = true)
            OutlinedTextField(command, {
                command = it
            }, label = { Text("Command (optional)") }, supportingText = { Text("Leave blank to open a login shell.") }, singleLine = true)
        }
    }, confirmButton = {
        TextButton(onClick = {
            start(target, cwd, command)
        }) { Text("Start shell") }
    }, dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } })
}

@Composable private fun TerminalPane(
    workspace: TerminalWorkspace,
    info: JSONObject,
    font: Int,
    modifier: Modifier,
    onSurface: (TerminalView?) -> Unit,
    onExit: () -> Unit,
    onModifiersUsed: () -> Unit,
) {
    val id = info.text("id")
    val writer = remember(id) { workspace.writer(id) }
    val context = LocalContext.current
    val lifecycle = LocalLifecycleOwner.current
    var ready by remember { mutableStateOf(false) }
    var replaying by remember { mutableStateOf(true) }
    var cols by remember { mutableIntStateOf(80) }
    var rows by remember { mutableIntStateOf(24) }
    var failure by remember { mutableStateOf("") }
    var truncated by remember { mutableStateOf(false) }
    var retry by remember { mutableIntStateOf(0) }
    val used by rememberUpdatedState(onModifiersUsed)
    val terminal =
        remember {
            TerminalView(
                context,
                onReady = { c, r ->
                    cols = c
                    rows = r
                    ready = true
                },
                onInput = writer::send,
                onResize = { c, r ->
                    cols = c
                    rows = r
                },
                onExitKeyboardFocus = onExit,
                onModifiersUsed = { used() },
            )
        }
    DisposableEffect(terminal) {
        onSurface(terminal)
        onDispose {
            onSurface(null)
            terminal.close()
        }
    }
    LaunchedEffect(ready, font) { if (ready) terminal.fontSize(font) }
    LaunchedEffect(ready, writer.ready, replaying, info.text("status"), failure) {
        if (ready) terminal.setActive(writer.ready && !replaying && failure.isEmpty() && info.text("status") == "running")
    }
    LaunchedEffect(ready, cols, rows, info.text("status")) {
        if (ready && info.text("status") == "running") {
            delay(150)
            try {
                workspace.call("resize", obj("terminalId" to id, "cols" to cols, "rows" to rows))
            } catch (
                error: CancellationException,
            ) {
                throw error
            } catch (error: Exception) {
                failure = error.message ?: "Resize failed"
            }
        }
    }
    LaunchedEffect(ready, retry) {
        if (!ready) return@LaunchedEffect
        var offset = 0L
        terminal.resetTerminal()
        failure = ""
        replaying = true
        lifecycle.lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
            try {
                while (isActive) {
                    val result = workspace.call("read", obj("terminalId" to id, "offset" to offset))
                    if (result.optBoolean("truncated")) {
                        terminal.resetTerminal()
                        truncated = true
                    }
                    val bytes = result.text("dataBase64")
                    if (bytes.isNotEmpty()) terminal.writeChunk(bytes)
                    offset = result.getLong("nextOffset")
                    if (offset >= result.optLong("endOffset", offset)) replaying = false
                    workspace.status(id, result)
                    if (result.text("status") == "exited" && offset >= result.optLong("endOffset", offset)) break
                }
            } catch (
                error: CancellationException,
            ) {
                throw error
            } catch (error: Exception) {
                failure = error.message ?: "Terminal connection failed"
            }
        }
    }
    Column(modifier.fillMaxWidth()) {
        Row(Modifier.fillMaxWidth().padding(horizontal = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            val status =
                if (replaying) {
                    "Connecting…"
                } else if (info.text("status") ==
                    "exited"
                ) {
                    "Exited · ${info.optInt("exitCode")}"
                } else {
                    "Running"
                }
            Text(
                "${if (info.text("target") == "host") "Host" else "Session"} · $status · $cols×$rows",
                style = MaterialTheme.typography.labelSmall,
                modifier = Modifier.weight(1f),
            )
        }
        if (failure.isNotEmpty()) {
            Text(failure, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
            TextButton(onClick = { retry++ }) { Text("Reconnect terminal") }
        }
        if (writer.failure.isNotEmpty()) {
            Text(writer.failure, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
            TextButton(onClick = { writer.connect() }) { Text("Reconnect input") }
        }
        if (truncated) {
            Text(
                "Older output was removed from the 2 MB buffer. Use Ctrl-L to redraw the screen.",
                style = MaterialTheme.typography.bodySmall,
            )
        }
        if (info
                .text(
                    "error",
                ).isNotEmpty()
        ) {
            Text(info.text("error"), color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
        }
        AndroidView(factory = { terminal }, modifier = Modifier.fillMaxWidth().weight(1f))
    }
}
