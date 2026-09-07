package wtf.fob.cs.navigation

import androidx.activity.compose.setContent
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.input.key.KeyEvent
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
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
import android.view.KeyEvent as AndroidKeyEvent

@OptIn(ExperimentalTestApi::class)
class KeyboardNavigationTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    private fun chord(
        key: Key,
        shift: Boolean = false,
    ) {
        compose.onAllNodes(isRoot()).onLast().performKeyInput {
            keyDown(Key.CtrlLeft)
            if (shift) keyDown(Key.ShiftLeft)
            pressKey(key)
            if (shift) keyUp(Key.ShiftLeft)
            keyUp(Key.CtrlLeft)
        }
    }

    @Test fun appShortcutsWorkWithoutTouchAndDialogsKeepFocus() {
        if (compose.onAllNodesWithText("Get started").fetchSemanticsNodes().isNotEmpty()) {
            compose.onNodeWithText("Get started").performScrollTo().performClick()
        }
        chord(Key.Comma)
        compose.onNodeWithText("Your Codex").assertIsDisplayed()
        compose.onAllNodes(isRoot()).onLast().performKeyInput {
            keyDown(Key.AltLeft)
            pressKey(Key.DirectionLeft)
            keyUp(Key.AltLeft)
        }
        compose.onNodeWithText("Your Codex").assertDoesNotExist()
        chord(Key.K)
        compose.onNode(hasSetTextAction() and hasText("Search your chats")).assertIsFocused()
        chord(Key.Slash)
        compose.onNodeWithText("Keyboard shortcuts").assertIsDisplayed()
        // A dialog owns the keys. The underlying page must not change.
        chord(Key.Comma)
        compose.onNodeWithText("Close").performClick()
        compose.onNodeWithText("Find a conversation").assertIsDisplayed()
    }

    @Test fun messageEnterAddsLineAndTabMovesFocusWithoutChangingDraft() {
        val repo = (compose.activity.application as CodexApplication).repository
        compose.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    ConversationScreen(
                        repo,
                        "keyboard-test-no-server",
                        obj("id" to "test", "cwd" to "/tmp", "executionCwd" to "/tmp"),
                        "",
                        {},
                        {},
                    )
                }
            }
        }
        val input = compose.onNodeWithTag("message-input")
        input.performClick()
        compose.runOnIdle { repo.clearError() }
        chord(Key.Enter)
        compose.runOnIdle { assertNull(repo.error.value) }
        input.performTextInput("Draft")
        input.performKeyInput { pressKey(Key.Enter) }
        input.assertTextContains("Draft\n")
        input.performKeyInput {
            keyDown(Key.ShiftLeft)
            pressKey(Key.Enter)
            keyUp(Key.ShiftLeft)
        }
        input.assertTextContains("Draft\n\n")
        input.performKeyInput { pressKey(Key.Tab) }
        compose.onNodeWithContentDescription("Send message").assertIsFocused()
        compose.onAllNodes(isRoot()).onLast().performKeyInput {
            keyDown(Key.ShiftLeft)
            pressKey(Key.Tab)
            keyUp(Key.ShiftLeft)
        }
        input.assertIsFocused().assertTextContains("Draft\n\n")
        chord(Key.Enter)
        compose.waitUntil(5000) { repo.error.value != null }
        // The failed send keeps the draft and inserts no extra line.
        input.assertTextEquals("Draft\n\n")
        compose.runOnIdle { repo.clearError() }
    }

    @Test fun heldShortcutRunsOnlyOnceAndHasNativeHelp() {
        var count = 0
        listOf(
            AndroidKeyEvent(1, 1, AndroidKeyEvent.ACTION_DOWN, AndroidKeyEvent.KEYCODE_ENTER, 0, AndroidKeyEvent.META_CTRL_ON),
            AndroidKeyEvent(1, 2, AndroidKeyEvent.ACTION_DOWN, AndroidKeyEvent.KEYCODE_ENTER, 1, AndroidKeyEvent.META_CTRL_ON),
            AndroidKeyEvent(1, 3, AndroidKeyEvent.ACTION_UP, AndroidKeyEvent.KEYCODE_ENTER, 0, AndroidKeyEvent.META_CTRL_ON),
        ).forEach { event -> assertTrue(KeyEvent(event).runShortcut { count++ }) }
        assertEquals(1, count)
        val groups = mutableListOf<android.view.KeyboardShortcutGroup>()
        compose.runOnIdle { compose.activity.onProvideKeyboardShortcuts(groups, null, -1) }
        assertTrue(
            groups.flatMap { it.items }.any {
                it.keycode == AndroidKeyEvent.KEYCODE_ENTER &&
                    it.modifiers == AndroidKeyEvent.META_CTRL_ON
            },
        )
    }
}
