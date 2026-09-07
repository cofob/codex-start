package wtf.fob.cs.navigation

import android.view.KeyEvent
import org.junit.Assert.*
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

class KeyboardShortcutsTest {
    private fun command(
        code: Int,
        ctrl: Boolean = false,
        shift: Boolean = false,
        alt: Boolean = false,
        meta: Boolean = false,
    ) = keyboardCommand(code, ctrl, shift, alt, meta)

    @Test fun textEditingAndAltGrAreNotAppShortcuts() {
        listOf(KeyEvent.KEYCODE_C, KeyEvent.KEYCODE_V, KeyEvent.KEYCODE_X, KeyEvent.KEYCODE_Z, KeyEvent.KEYCODE_A).forEach {
            assertNull(command(it, ctrl = true))
        }
        assertNull(command(KeyEvent.KEYCODE_K, ctrl = true, alt = true))
        assertNull(command(KeyEvent.KEYCODE_K, ctrl = true, meta = true))
        assertNull(command(KeyEvent.KEYCODE_K))
        assertNull(command(KeyEvent.KEYCODE_K, ctrl = true, shift = true))
    }

    @Test fun enterOnlySendsWithExactControlModifier() {
        for (key in listOf(KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER)) {
            assertEquals(KeyboardCommand.Send, command(key, ctrl = true))
            assertNull(command(key))
            assertNull(command(key, shift = true))
            assertNull(command(key, ctrl = true, shift = true))
            assertNull(command(key, ctrl = true, alt = true))
        }
    }
}
