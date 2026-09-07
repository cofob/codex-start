package wtf.fob.cs.terminal

import android.os.SystemClock
import android.util.Base64
import android.view.KeyEvent
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.viewinterop.AndroidView
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import wtf.fob.cs.app.*
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.pairing.*
import wtf.fob.cs.settings.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.atomic.AtomicBoolean

class TerminalKeyboardTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    @Test fun physicalKeysReachTerminalAndFocusCanLeaveIt() {
        val input = ConcurrentLinkedQueue<String>()
        val ready = AtomicBoolean(false)
        lateinit var terminal: TerminalView
        compose.runOnUiThread {
            compose.activity.setContent {
                val exit = remember { FocusRequester() }
                val inputMode = androidx.compose.ui.platform.LocalInputModeManager.current
                Column(Modifier.fillMaxSize()) {
                    TextButton(onClick = {}, modifier = Modifier.focusRequester(exit)) { Text("Terminal controls") }
                    AndroidView(factory = { context ->
                        TerminalView(context, { _, _ -> ready.set(true) }, {
                            input.add(String(Base64.decode(it, Base64.NO_WRAP), Charsets.UTF_8))
                        }, { _, _ -> }, {
                            inputMode.requestInputMode(androidx.compose.ui.input.InputMode.Keyboard)
                            exit.requestFocus()
                        }).also { terminal = it }
                    }, modifier = Modifier.weight(1f).fillMaxWidth())
                }
            }
        }
        compose.waitUntil(10_000) { ready.get() }

        fun press(
            code: Int,
            modifiers: Int = 0,
        ) {
            val time = SystemClock.uptimeMillis()
            val instrumentation = InstrumentationRegistry.getInstrumentation()
            instrumentation.sendKeySync(KeyEvent(time, time, KeyEvent.ACTION_DOWN, code, 0, modifiers))
            instrumentation.sendKeySync(KeyEvent(time, time + 1, KeyEvent.ACTION_UP, code, 0, modifiers))
        }
        try {
            compose.runOnUiThread {
                terminal.setActive(true)
                terminal.requestFocus()
                terminal.focusTerminal()
            }
            press(KeyEvent.KEYCODE_C, KeyEvent.META_CTRL_ON)
            press(KeyEvent.KEYCODE_TAB)
            press(KeyEvent.KEYCODE_ESCAPE)
            press(KeyEvent.KEYCODE_DPAD_UP)
            compose.waitUntil(5000) { input.size >= 4 }
            assertEquals(listOf("\u0003", "\t", "\u001b", "\u001b[A"), input.toList())
            press(KeyEvent.KEYCODE_TAB, KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON)
            compose.waitUntil(5000) {
                compose.onAllNodes(hasText("Terminal controls") and isFocused()).fetchSemanticsNodes().isNotEmpty()
            }
            compose.onNodeWithText("Terminal controls").assertIsFocused()
            assertEquals(4, input.size)
        } finally {
            compose.runOnUiThread { terminal.close() }
        }
    }
}
