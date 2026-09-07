package wtf.fob.cs.terminal

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

class TerminalKeyInputTest {
    @Test fun stickyModifiersPreserveUnicodeAndControlCodes() {
        assertEquals("\u0003", terminalModifiedText("c", true, false))
        assertEquals("\u001b\u0003", terminalModifiedText("c", true, true))
        assertEquals("\u001bb", terminalModifiedText("b", false, true))
        assertEquals("\u001b", terminalModifiedText("[", true, false))
        assertEquals("\u0000", terminalModifiedText(" ", true, false))
        assertEquals("héllo 世界", terminalModifiedText("héllo 世界", false, false))
        assertEquals("\u001b[1;5D", terminalModifiedText("\u001b[D", true, false))
        assertEquals("\u001b[24;7~", terminalModifiedText("\u001b[24~", true, true))
    }

    private fun input(
        code: Int,
        ctrl: Boolean = false,
        shift: Boolean = false,
        alt: Boolean = false,
        meta: Boolean = false,
    ) = terminalKeyInput(code, ctrl, shift, alt, meta)

    @Test fun terminalControlAndNavigationKeysUsePtySequences() {
        assertEquals("\u0003", input(KeyEvent.KEYCODE_C, ctrl = true))
        assertEquals("\u0000", input(KeyEvent.KEYCODE_SPACE, ctrl = true))
        assertEquals("\u001b[1;5D", input(KeyEvent.KEYCODE_DPAD_LEFT, ctrl = true))
        assertEquals("\u001b[Z", input(KeyEvent.KEYCODE_TAB, shift = true))
        assertEquals("\u001b[3~", input(KeyEvent.KEYCODE_FORWARD_DEL))
        assertEquals("\u001bOP", input(KeyEvent.KEYCODE_F1))
        assertEquals("\u001b[24~", input(KeyEvent.KEYCODE_F12))
        assertEquals("\r", input(KeyEvent.KEYCODE_NUMPAD_ENTER))
        assertEquals("\u007f", input(KeyEvent.KEYCODE_DEL))
    }

    @Test fun printableTextClipboardAndAltGrStayWithWebView() {
        assertNull(input(KeyEvent.KEYCODE_A))
        assertNull(input(KeyEvent.KEYCODE_V, ctrl = true, shift = true))
        assertNull(input(KeyEvent.KEYCODE_C, ctrl = true, shift = true))
        assertNull(input(KeyEvent.KEYCODE_Q, ctrl = true, alt = true))
        assertNull(input(KeyEvent.KEYCODE_C, meta = true))
    }
}
