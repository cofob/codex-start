package wtf.fob.cs.conversation

import android.content.ClipboardManager
import android.graphics.Bitmap
import android.widget.TextView
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import wtf.fob.cs.app.*
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*
import java.io.File

class ChatPresentationTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    private fun capture(stage: String) {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        instrumentation.uiAutomation.waitForIdle(500, 3000)
        val image = instrumentation.uiAutomation.takeScreenshot()
        File(instrumentation.targetContext.filesDir, "navigation-$stage.png").outputStream().use {
            image.compress(Bitmap.CompressFormat.PNG, 100, it)
        }
        image.recycle()
    }

    @Test fun activityIsCompactAndAnswersCanBeCopiedAndSelected() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val repo = (context.applicationContext as CodexApplication).repository
        val answer =
            "A clear **answer** with selectable text.\n\n" +
                (1..20).joinToString("\n\n") { "Paragraph $it: scroll and select part of this answer." }
        val clipboard = context.getSystemService(ClipboardManager::class.java)
        val block =
            ConversationBlock(
                (1..8).map {
                    ConversationItem(
                        "tool-$it",
                        if (it % 2 ==
                            0
                        ) {
                            "reasoning"
                        } else {
                            "commandExecution"
                        },
                        "Inspect files $it",
                    )
                },
            )
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    LazyColumn(Modifier.fillMaxSize().safeDrawingPadding()) {
                        item { ConversationActivity(block, false) {} }
                        item {
                            ConversationMessage(ConversationItem("answer", "agentMessage", answer), repo, "fixture", "fixture") {
                                clipboard.setPrimaryClip(android.content.ClipData.newPlainText("answer", answer))
                            }
                        }
                    }
                }
            }
        }
        compose.onAllNodesWithTag("activity-block").assertCountEquals(1)
        assertTrue(
            compose
                .onNodeWithTag("activity-block")
                .fetchSemanticsNode()
                .boundsInRoot.height <=
                52 * compose.activity.resources.displayMetrics.density,
        )
        compose.onNodeWithText("Thinking · 4 tool calls").assertIsDisplayed().performClick()
        compose.onNodeWithText("Inspect files 1").assertIsDisplayed()
        compose.onNodeWithContentDescription("Collapse activity").performClick()
        compose.onNodeWithText("Inspect files 1").assertDoesNotExist()
        capture("activity")
        // Native TextView long presses use Android's clock, not Compose's event clock.
        androidx.test.espresso.Espresso
            .onView(
                androidx.test.espresso.matcher.ViewMatchers
                    .withText(org.hamcrest.Matchers.startsWith("A clear answer")),
            ).perform(
                object : androidx.test.espresso.ViewAction {
                    override fun getConstraints() =
                        androidx.test.espresso.matcher.ViewMatchers
                            .isDisplayingAtLeast(1)

                    override fun getDescription() = "Long press the start of the answer with Android's clock"

                    override fun perform(
                        controller: androidx.test.espresso.UiController,
                        view: android.view.View,
                    ) {
                        val location = IntArray(2)
                        view.getLocationOnScreen(location)
                        androidx.test.espresso.action.Tap.LONG.sendTap(
                            controller,
                            floatArrayOf(location[0] + view.width / 3f, location[1] + 24f),
                            floatArrayOf(1f, 1f),
                        )
                    }
                },
            )
        try {
            compose.waitUntil(5000) { compose.onNodeWithText("Copy full answer").isDisplayed() }
        } finally {
            capture("selection")
        }
        compose.onNodeWithText("Touch and hold the answer, then move the selection handles.").assertIsDisplayed()
        compose.onNodeWithText("Copy full answer").assertIsDisplayed().performClick()
        compose.runOnIdle {
            assertEquals(
                answer,
                clipboard.primaryClip!!
                    .getItemAt(0)
                    .text
                    .toString(),
            )
        }
        // The sheet is a separate window. Espresso searches all active roots.
        androidx.test.espresso.Espresso
            .onView(
                org.hamcrest.Matchers.allOf(
                    androidx.test.espresso.matcher.ViewMatchers
                        .isAssignableFrom(TextView::class.java),
                    androidx.test.espresso.matcher.ViewMatchers
                        .withText(org.hamcrest.Matchers.startsWith("A clear answer")),
                ),
            ).check { view, error ->
                if (error != null) throw error
                val text = view as TextView
                assertTrue(text.isTextSelectable)
                android.text.Selection.setSelection(text.text as android.text.Spannable, 2, 7)
                assertTrue(text.onTextContextMenuItem(android.R.id.copy))
            }
        compose.runOnIdle {
            assertEquals(
                "clear",
                clipboard.primaryClip!!
                    .getItemAt(0)
                    .text
                    .toString(),
            )
        }
        compose.onNodeWithContentDescription("Close Select text").performClick()
        compose.onNodeWithTag("agent-answer").performTouchInput { swipeUp() }
        compose.onNodeWithContentDescription("Copy answer").assertIsDisplayed().performClick()
        compose.runOnIdle {
            assertEquals(
                answer,
                clipboard.primaryClip!!
                    .getItemAt(0)
                    .text
                    .toString(),
            )
        }
        compose.onNodeWithText("Select text").performClick()
        compose.onNodeWithTag("answer-selection").assertIsDisplayed()
    }
}
