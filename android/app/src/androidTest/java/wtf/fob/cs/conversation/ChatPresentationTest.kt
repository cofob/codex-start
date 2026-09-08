package wtf.fob.cs.conversation

import android.content.ClipData
import android.content.ClipboardManager
import android.graphics.Bitmap
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.widget.TextView
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.unit.dp
import androidx.test.espresso.Espresso
import androidx.test.espresso.matcher.ViewMatchers
import androidx.test.platform.app.InstrumentationRegistry
import org.hamcrest.Matchers
import org.junit.Assert.*
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import wtf.fob.cs.app.*
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*
import java.io.File

class ChatPresentationTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    @Before fun keepScreenOn() {
        compose.activity.runOnUiThread { compose.activity.window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON) }
    }

    private fun capture(stage: String) {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        instrumentation.uiAutomation.waitForIdle(500, 3000)
        val image = instrumentation.uiAutomation.takeScreenshot()
        File(instrumentation.targetContext.filesDir, "navigation-$stage.png").outputStream().use {
            image.compress(Bitmap.CompressFormat.PNG, 100, it)
        }
        image.recycle()
    }

    private fun chatViews(view: View = compose.activity.window.decorView): List<ChatMarkdownView> =
        when (view) {
            is ChatMarkdownView -> listOf(view)
            is ViewGroup -> (0 until view.childCount).flatMap { chatViews(view.getChildAt(it)) }
            else -> emptyList()
        }

    private fun waitForAnswer(text: String) {
        compose.waitUntil(5000) {
            var found = false
            InstrumentationRegistry.getInstrumentation().runOnMainSync { found = chatViews().any { it.text.toString() == text } }
            found
        }
    }

    private fun longPressAnswer(tap: androidx.test.espresso.action.Tap = androidx.test.espresso.action.Tap.LONG) {
        // Native selection uses Android's clock, not Compose's event clock.
        Espresso.onView(ViewMatchers.isAssignableFrom(ChatMarkdownView::class.java)).perform(
            object : androidx.test.espresso.ViewAction {
                override fun getConstraints() = ViewMatchers.isDisplayingAtLeast(1)

                override fun getDescription() = "Long press the first line of the answer"

                override fun perform(
                    controller: androidx.test.espresso.UiController,
                    view: View,
                ) {
                    val location = IntArray(2)
                    view.getLocationOnScreen(location)
                    tap.sendTap(
                        controller,
                        floatArrayOf(location[0] + 60f, location[1] + 24f),
                        floatArrayOf(1f, 1f),
                    )
                }
            },
        )
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
                    ConversationItem("tool-$it", if (it % 2 == 0) "reasoning" else "commandExecution", "Inspect files $it")
                },
            )
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    LazyColumn(Modifier.fillMaxSize().safeDrawingPadding().padding(horizontal = 16.dp)) {
                        item { ConversationActivity(block, false) {} }
                        item {
                            ConversationMessage(ConversationItem("answer", "agentMessage", answer), repo, "fixture", "fixture") {
                                clipboard.setPrimaryClip(ClipData.newPlainText("answer", answer))
                            }
                        }
                    }
                }
            }
        }
        waitForAnswer(answer.replace("**", ""))
        compose.onAllNodesWithTag("activity-block").assertCountEquals(1)
        assertTrue(
            compose
                .onNodeWithTag("activity-block")
                .fetchSemanticsNode()
                .boundsInRoot.height <=
                52 * compose.activity.resources.displayMetrics.density,
        )
        compose.onNodeWithText("Thinking · 4 commands").assertIsDisplayed().performClick()
        compose.onNodeWithText("Inspect files 1").assertIsDisplayed()
        compose.onNodeWithContentDescription("Collapse activity").performClick()
        compose.onNodeWithText("Inspect files 1").assertDoesNotExist()
        capture("activity")
        longPressAnswer()
        compose.runOnIdle {
            val text = chatViews().single()
            assertTrue(text.isTextSelectable)
            assertTrue("Native long press must select a word", text.selectionEnd > text.selectionStart)
        }
        capture("inline-selection")
        compose.runOnIdle {
            val text = chatViews().single()
            android.text.Selection.setSelection(text.text as android.text.Spannable, 2, 7)
            assertTrue(text.onTextContextMenuItem(android.R.id.copy))
            assertEquals(
                "clear",
                clipboard.primaryClip!!
                    .getItemAt(0)
                    .text
                    .toString(),
            )
        }
        // Ordinary drags must still scroll the chat after selection ends.
        repeat(4) {
            if (!compose.onNodeWithContentDescription("Copy answer").isDisplayed()) {
                compose.onNodeWithTag("agent-answer").performTouchInput { swipeUp() }
            }
        }
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
        compose.onNodeWithContentDescription("Message actions").performClick()
        compose.onNodeWithText("Select text").performClick()
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
        capture("selection")
        Espresso
            .onView(
                Matchers.allOf(
                    ViewMatchers.isAssignableFrom(TextView::class.java),
                    ViewMatchers.withText(Matchers.startsWith("A clear answer")),
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
    }

    @Test fun streamingKeepsSelectionStableAndDoesNotReparseUnchangedText() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val repo = (context.applicationContext as CodexApplication).repository
        val source = mutableStateOf("A clear **answer** with emoji 🧪.")
        val streaming = mutableStateOf(true)
        var selecting = false
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    Column(Modifier.fillMaxSize().safeDrawingPadding().padding(16.dp)) {
                        ConversationMessage(
                            ConversationItem("answer", "agentMessage", source.value),
                            repo,
                            "fixture",
                            "fixture",
                            streaming = streaming.value,
                            selectionChanged = { selecting = it },
                        ) {}
                    }
                }
            }
        }
        waitForAnswer(source.value.replace("**", ""))
        compose.onNodeWithTag("answer-streaming").assertIsDisplayed()
        compose.onNodeWithContentDescription("Copy answer").assertDoesNotExist()
        var renders = 0
        compose.runOnIdle {
            renders = chatViews().single().renderCount
            streaming.value = false
        }
        compose.onNodeWithContentDescription("Copy answer").assertIsDisplayed()
        compose.runOnIdle {
            assertEquals(renders, chatViews().single().renderCount)
            streaming.value = true
        }
        compose.onNodeWithTag("answer-streaming").assertIsDisplayed()
        longPressAnswer()
        var selected = ""
        compose.runOnIdle {
            assertTrue("Native selection must be active", selecting)
            val view = chatViews().single()
            selected = view.text.subSequence(view.selectionStart, view.selectionEnd).toString()
        }
        repeat(12) { index -> compose.runOnIdle { source.value += " Part $index 🧪." } }
        compose.runOnIdle {
            val view = chatViews().single()
            assertTrue(selecting)
            assertEquals(renders, view.renderCount)
            assertEquals(selected, view.text.subSequence(view.selectionStart, view.selectionEnd).toString())
        }
        capture("streaming-selection")
        Espresso.pressBack()
        waitForAnswer(source.value.replace("**", ""))
        compose.runOnIdle {
            assertFalse(selecting)
            assertFalse("Selection focus must close with the toolbar", chatViews().single().hasFocus())
            streaming.value = false
        }
        compose.onNodeWithTag("answer-streaming").assertDoesNotExist()
        compose.onNodeWithContentDescription("Copy answer").assertIsDisplayed()
        capture("streaming-complete")
    }

    @Test fun liveToolOutputStaysInOneExpandedActivityBlock() {
        val command =
            conversationItem(
                obj(
                    "id" to "tool",
                    "type" to "commandExecution",
                    "command" to "git status",
                    "aggregatedOutput" to "",
                    "status" to "inProgress",
                ),
            )
        val items = mutableStateOf(listOf(ConversationItem("thinking", "reasoning", "Check the working tree."), command))
        val working = mutableStateOf(true)
        var copied = ""
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    Column(Modifier.fillMaxSize().safeDrawingPadding().padding(16.dp)) {
                        ConversationActivity(ConversationBlock(items.value), working.value) { copied = it }
                    }
                }
            }
        }
        compose.onNodeWithText("Thinking · 1 command").performClick()
        compose.onNodeWithText("Command").performClick()
        val output = "## This is literal command output\n**not bold** 🧪\n"
        compose.runOnIdle {
            items.value =
                applyConversationDeltas(
                    items.value,
                    listOf(
                        RemoteEvent(
                            "fixture",
                            "fixture",
                            0,
                            obj("method" to "item/commandExecution/outputDelta", "params" to obj("itemId" to "tool", "delta" to output)),
                        ),
                    ),
                )
        }
        compose.onAllNodesWithTag("activity-block").assertCountEquals(1)
        compose.onNodeWithContentDescription("Collapse activity").assertIsDisplayed()
        compose.onNodeWithText("git status\n\n$output").assertIsDisplayed()
        compose.onNodeWithText("Copy").performClick()
        compose.runOnIdle { assertEquals("git status\n\n$output", copied) }
        capture("tool-streaming")
        compose.runOnIdle {
            items.value =
                items.value.map {
                    if (it.id == "tool") {
                        conversationItem(
                            obj(
                                "id" to "tool",
                                "type" to "commandExecution",
                                "command" to "git status",
                                "aggregatedOutput" to output,
                                "status" to "failed",
                                "exitCode" to 1,
                            ),
                        )
                    } else {
                        it
                    }
                }
            working.value = false
        }
        compose.onNodeWithContentDescription("Activity needs review").assertIsDisplayed()
        compose.onNodeWithText("git status\n\n$output").assertIsDisplayed()
        capture("tool-complete")
    }

    @Test fun linksStillOpenOnTapAndSelectOnLongPress() {
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    Column(Modifier.fillMaxSize().safeDrawingPadding().padding(16.dp)) {
                        Markdown("[Clear link](https://example.invalid) with selectable text.", selectable = true)
                    }
                }
            }
        }
        waitForAnswer("Clear link with selectable text.")
        var clicks = 0
        compose.runOnIdle {
            val text = chatViews().single().text as android.text.Spannable
            val links = text.getSpans(0, text.length, android.text.style.ClickableSpan::class.java)
            assertEquals(1, links.size)
            // Keep the real span range, but do not open an external app during this test.
            val start = text.getSpanStart(links.single())
            val end = text.getSpanEnd(links.single())
            text.removeSpan(links.single())
            text.setSpan(
                object : android.text.style.ClickableSpan() {
                    override fun onClick(widget: View) {
                        clicks++
                    }
                },
                start,
                end,
                android.text.Spanned.SPAN_EXCLUSIVE_EXCLUSIVE,
            )
        }
        longPressAnswer(androidx.test.espresso.action.Tap.SINGLE)
        compose.runOnIdle { assertEquals(1, clicks) }
        longPressAnswer()
        compose.runOnIdle {
            assertEquals("A long press must not open the link", 1, clicks)
            val view = chatViews().single()
            assertTrue(view.selectionEnd > view.selectionStart)
        }
        Espresso.pressBack()
    }
}
