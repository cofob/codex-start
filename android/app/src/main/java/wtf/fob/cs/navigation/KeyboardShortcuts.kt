package wtf.fob.cs.navigation

import android.view.KeyboardShortcutGroup
import android.view.KeyboardShortcutInfo
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusDirection
import androidx.compose.ui.focus.FocusManager
import androidx.compose.ui.input.key.*
import androidx.compose.ui.unit.dp
import wtf.fob.cs.data.*
import wtf.fob.cs.settings.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import android.view.KeyEvent as AndroidKeyEvent

internal enum class KeyboardCommand(
    val label: String,
    val keyCode: Int,
    val modifiers: Int,
    val keys: String,
    val group: String,
) {
    Search("Search all chats", AndroidKeyEvent.KEYCODE_K, AndroidKeyEvent.META_CTRL_ON, "Ctrl+K", "Navigation"),
    SwitchServer(
        "Switch server or task",
        AndroidKeyEvent.KEYCODE_S,
        AndroidKeyEvent.META_CTRL_ON or AndroidKeyEvent.META_SHIFT_ON,
        "Ctrl+Shift+S",
        "Navigation",
    ),
    Settings("Open settings", AndroidKeyEvent.KEYCODE_COMMA, AndroidKeyEvent.META_CTRL_ON, "Ctrl+,", "Navigation"),
    Projects(
        "Open projects",
        AndroidKeyEvent.KEYCODE_H,
        AndroidKeyEvent.META_CTRL_ON or AndroidKeyEvent.META_SHIFT_ON,
        "Ctrl+Shift+H",
        "Navigation",
    ),
    Back("Go back", AndroidKeyEvent.KEYCODE_DPAD_LEFT, AndroidKeyEvent.META_ALT_ON, "Alt+Left", "Navigation"),
    Escape("Close navigation or go back", AndroidKeyEvent.KEYCODE_ESCAPE, 0, "Escape", "Navigation"),
    Chat("Open chat", AndroidKeyEvent.KEYCODE_1, AndroidKeyEvent.META_CTRL_ON, "Ctrl+1", "Current task"),
    Changes("Open changes", AndroidKeyEvent.KEYCODE_2, AndroidKeyEvent.META_CTRL_ON, "Ctrl+2", "Current task"),
    Files("Open files", AndroidKeyEvent.KEYCODE_3, AndroidKeyEvent.META_CTRL_ON, "Ctrl+3", "Current task"),
    NewChat("New chat in the project chat list", AndroidKeyEvent.KEYCODE_N, AndroidKeyEvent.META_CTRL_ON, "Ctrl+N", "Current task"),
    FocusMessage(
        "Focus the message field",
        AndroidKeyEvent.KEYCODE_L,
        AndroidKeyEvent.META_CTRL_ON or AndroidKeyEvent.META_SHIFT_ON,
        "Ctrl+Shift+L",
        "Message",
    ),
    Send(
        "Send, queue, or steer from the message field",
        AndroidKeyEvent.KEYCODE_ENTER,
        AndroidKeyEvent.META_CTRL_ON,
        "Ctrl+Enter",
        "Message",
    ),
    Attach(
        "Add an attachment from the message field",
        AndroidKeyEvent.KEYCODE_A,
        AndroidKeyEvent.META_CTRL_ON or AndroidKeyEvent.META_SHIFT_ON,
        "Ctrl+Shift+A",
        "Message",
    ),
    ExitTerminal(
        "Move focus from the terminal to app controls",
        AndroidKeyEvent.KEYCODE_TAB,
        AndroidKeyEvent.META_CTRL_ON or AndroidKeyEvent.META_SHIFT_ON,
        "Ctrl+Shift+Tab",
        "Terminal",
    ),
    Help("Show keyboard shortcuts", AndroidKeyEvent.KEYCODE_SLASH, AndroidKeyEvent.META_CTRL_ON, "Ctrl+/", "Navigation"),
}

// Use exact modifiers. In particular, Ctrl+Alt (AltGr) must remain text input.
internal fun keyboardCommand(
    keyCode: Int,
    ctrl: Boolean,
    shift: Boolean,
    alt: Boolean,
    meta: Boolean,
): KeyboardCommand? {
    if (meta) return null
    val modifiers =
        (if (ctrl) AndroidKeyEvent.META_CTRL_ON else 0) or
            (if (shift) AndroidKeyEvent.META_SHIFT_ON else 0) or (if (alt) AndroidKeyEvent.META_ALT_ON else 0)
    return KeyboardCommand.entries.firstOrNull {
        (it.keyCode == keyCode || it == KeyboardCommand.Send && keyCode == AndroidKeyEvent.KEYCODE_NUMPAD_ENTER) &&
            it.modifiers == modifiers
    }
}

internal fun KeyEvent.command() = keyboardCommand(nativeKeyEvent.keyCode, isCtrlPressed, isShiftPressed, isAltPressed, isMetaPressed)

/** Consume the complete chord, but run the action only on its first key down. */
internal fun KeyEvent.runShortcut(action: () -> Unit): Boolean {
    if (type == KeyEventType.KeyDown && nativeKeyEvent.repeatCount == 0) action()
    return type == KeyEventType.KeyDown || type == KeyEventType.KeyUp
}

/** Text fields insert a tab by default. Hardware Tab must move between controls. */
internal fun Modifier.keyboardTabNavigation(focusManager: FocusManager) =
    onPreviewKeyEvent { event ->
        if (event.key == Key.Tab && !event.isCtrlPressed && !event.isAltPressed && !event.isMetaPressed) {
            event.runShortcut { focusManager.moveFocus(if (event.isShiftPressed) FocusDirection.Previous else FocusDirection.Next) }
        } else {
            false
        }
    }

internal fun keyboardShortcutGroups(): List<KeyboardShortcutGroup> =
    KeyboardCommand.entries.groupBy { it.group }.map { (group, commands) ->
        KeyboardShortcutGroup(group, commands.map { KeyboardShortcutInfo(it.label, it.keyCode, it.modifiers) })
    }

@Composable
internal fun KeyboardShortcutsDialog(dismiss: () -> Unit) {
    AlertDialog(onDismissRequest = dismiss, title = { Text("Keyboard shortcuts") }, text = {
        Column(Modifier.verticalScroll(rememberScrollState())) {
            KeyboardCommand.entries.groupBy { it.group }.forEach { (group, commands) ->
                Text(group, style = MaterialTheme.typography.titleSmall, modifier = Modifier.padding(top = 12.dp))
                commands.forEach { command ->
                    Text("${command.keys} — ${command.label}", Modifier.padding(vertical = 4.dp))
                }
            }
            Text(
                "Enter and Shift+Enter add a new line in a message. Tab and Shift+Tab move between controls. Standard text editing shortcuts remain available.",
                Modifier.padding(top = 12.dp),
            )
            Text(
                "When the terminal has focus, it receives keyboard input. Use its keyboard focus exit shortcut, Ctrl+Shift+Tab, to return to app controls.",
                Modifier.padding(top = 12.dp),
            )
        }
    }, confirmButton = { TextButton(onClick = dismiss) { Text("Close") } })
}

internal class KeyboardActions {
    private val actions = mutableMapOf<KeyboardCommand, () -> Unit>()

    fun register(
        command: KeyboardCommand,
        action: () -> Unit,
    ) {
        actions[command] = action
    }

    fun remove(
        command: KeyboardCommand,
        action: () -> Unit,
    ) {
        if (actions[command] === action) actions.remove(command)
    }

    fun handle(event: KeyEvent): Boolean = actions[event.command()]?.let { event.runShortcut(it) } ?: false
}

internal val LocalKeyboardActions = staticCompositionLocalOf<KeyboardActions?> { null }

/** Register only actions from the current destination, with its latest state. */
@Composable
internal fun KeyboardAction(
    command: KeyboardCommand,
    action: () -> Unit,
) {
    val actions = LocalKeyboardActions.current
    val current by rememberUpdatedState(action)
    DisposableEffect(actions, command) {
        val handler = { current() }
        actions?.register(command, handler)
        onDispose { actions?.remove(command, handler) }
    }
}
