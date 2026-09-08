package wtf.fob.cs.conversation

import android.graphics.Bitmap
import android.text.Selection
import android.text.Spannable
import android.text.Spanned
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.dp
import androidx.test.platform.app.InstrumentationRegistry
import io.noties.markwon.Markwon
import io.noties.markwon.core.spans.OrderedListItemSpan
import org.junit.Assert.*
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import wtf.fob.cs.app.MainActivity
import wtf.fob.cs.data.CodexApplication
import wtf.fob.cs.ui.ConversationMessage
import java.io.File

/** Real-device regression tests: no model calls or changes to saved user conversations. */
class ChatScrollTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    @Before fun keepScreenOn() {
        compose.activity.runOnUiThread { compose.activity.window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON) }
    }

    private fun answer(index: Int): String =
        "## Answer $index\n\n" +
            "Text with **bold**, *emphasis*, `inline code`, and a [link](https://example.invalid). 🧪\n\n" +
            "- First item\n- Second item\n\n123. Numbered item\n124. Another item\n\n> A quoted paragraph.\n\n" +
            "```kotlin\nval answer = $index\nprintln(answer)\n```\n\n" +
            (1..8).joinToString("\n\n") { "Paragraph $it: scroll past this answer and return. العربية · 日本語 · 🧪" }

    private fun views(view: View = compose.activity.window.decorView): List<ChatMarkdownView> =
        when (view) {
            is ChatMarkdownView -> listOf(view)
            is ViewGroup -> (0 until view.childCount).flatMap { views(view.getChildAt(it)) }
            else -> emptyList()
        }

    private fun measure(view: ChatMarkdownView) {
        view.measure(
            View.MeasureSpec.makeMeasureSpec(800, View.MeasureSpec.EXACTLY),
            View.MeasureSpec.makeMeasureSpec(0, View.MeasureSpec.UNSPECIFIED),
        )
        view.layout(0, 0, view.measuredWidth, view.measuredHeight)
    }

    @Test fun firstBindingAndRestoredBindingHaveTheFullHeightImmediately() {
        compose.runOnIdle {
            val cache = ChatMarkdownCache()
            val source = answer(7)
            val expected = Markwon.create(compose.activity).toMarkdown(source)
            val first = ChatMarkdownView(compose.activity, cache).apply { setTextIsSelectable(true) }
            first.submit(source)
            // This fails with the old async-only first binding, before the UI queue can run.
            assertEquals(expected.toString(), first.text.toString())
            measure(first)
            val height = first.height
            assertTrue(height > 500)
            assertEquals(expected.length, first.layout.getLineEnd(first.lineCount - 1))
            first.close()
            repeat(20) {
                val restored = ChatMarkdownView(compose.activity, cache).apply { setTextIsSelectable(true) }
                restored.submit(source)
                measure(restored)
                assertEquals(expected.toString(), restored.text.toString())
                assertEquals(height, restored.height)
                assertEquals(1, restored.renderCount)
                restored.submit(source)
                assertEquals(1, restored.renderCount)
                restored.close()
            }
        }
    }

    @Test fun cachedDocumentsDoNotShareSelectionOrLoseSmallRowsForAnOversizeAnswer() {
        compose.runOnIdle {
            val cache = ChatMarkdownCache()
            val source = answer(9)
            val first = ChatMarkdownView(compose.activity, cache).apply { setTextIsSelectable(true) }
            val second = ChatMarkdownView(compose.activity, cache).apply { setTextIsSelectable(true) }
            first.submit(source)
            Selection.setSelection(first.text as Spannable, 0, 6)
            second.submit(source)
            assertTrue(first.selectionEnd > first.selectionStart)
            assertEquals("The second view must not inherit a selection", second.selectionStart, second.selectionEnd)
            val marker = Any()
            (first.text as Spannable).setSpan(marker, 0, 1, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            assertEquals(-1, (second.text as Spanned).getSpanStart(marker))
            val firstList = (first.text as Spanned).getSpans(0, first.text.length, OrderedListItemSpan::class.java).first()
            val secondList = (second.text as Spanned).getSpans(0, second.text.length, OrderedListItemSpan::class.java).first()
            assertNotSame("Drawing metrics must not be shared between views", firstList, secondList)
            val oversize = "x".repeat(600_000)
            cache.put(oversize, Markwon.create(compose.activity).parse(oversize))
            assertNotNull(cache.get(source))
            assertNull(cache.get(oversize))
            first.close()
            second.close()
        }
    }

    private fun capture(name: String): Bitmap {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        instrumentation.uiAutomation.waitForIdle(200, 3000)
        val screenshot = instrumentation.uiAutomation.takeScreenshot()
        File(instrumentation.targetContext.filesDir, "navigation-$name.png").outputStream().use {
            screenshot.compress(Bitmap.CompressFormat.PNG, 100, it)
        }
        // Compare only the chat viewport, not clocks or system status icons.
        val bounds = compose.onNodeWithTag("scroll-fixture").fetchSemanticsNode().boundsInRoot
        val location = IntArray(2)
        compose.runOnIdle {
            compose.activity.window.decorView
                .getLocationOnScreen(location)
        }
        return Bitmap
            .createBitmap(
                screenshot,
                location[0] + bounds.left.toInt(),
                location[1] + bounds.top.toInt(),
                bounds.width.toInt(),
                bounds.height.toInt(),
            ).also { if (it !== screenshot) screenshot.recycle() }
    }

    private fun checkScroll(
        dark: Boolean,
        fontScale: Float,
    ) {
        val repo = (compose.activity.application as CodexApplication).repository
        val state = LazyListState()
        val sources = mutableStateOf((0..39).map(::answer))
        val suffix = if (dark) "dark-large" else "light"
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                val density = LocalDensity.current
                CompositionLocalProvider(LocalDensity provides Density(density.density, fontScale)) {
                    MaterialTheme(colorScheme = if (dark) darkColorScheme() else lightColorScheme()) {
                        Surface {
                            LazyColumn(
                                state = state,
                                modifier =
                                    Modifier
                                        .fillMaxSize()
                                        .safeDrawingPadding()
                                        .padding(
                                            horizontal = 16.dp,
                                        ).testTag("scroll-fixture"),
                                verticalArrangement = Arrangement.spacedBy(10.dp),
                            ) {
                                items(sources.value.indices.toList(), key = { "answer-$it" }) { index ->
                                    ConversationMessage(
                                        ConversationItem("answer-$index", "agentMessage", sources.value[index]),
                                        repo,
                                        "fixture",
                                        "fixture",
                                    ) {}
                                }
                            }
                        }
                    }
                }
            }
        }
        val list = compose.onNodeWithTag("scroll-fixture")
        list.performScrollToIndex(12)
        compose.waitForIdle()
        val before = capture("scroll-$suffix-before")
        repeat(4) {
            list.performTouchInput { swipeUp(durationMillis = 250) }
            list.performTouchInput { swipeDown(durationMillis = 250) }
            list.performScrollToIndex(32)
            list.performScrollToIndex(0)
            list.performScrollToIndex(12)
            compose.runOnIdle {
                assertEquals(12, state.firstVisibleItemIndex)
                assertEquals(0, state.firstVisibleItemScrollOffset)
                views().filter { it.isShown && it.height > 0 }.forEach { view ->
                    assertTrue("No restored answer may be blank", view.text.isNotEmpty())
                    assertTrue("The whole answer must be measured", view.height >= view.layout.height)
                    assertEquals(view.text.length, view.layout.getLineEnd(view.lineCount - 1))
                    assertEquals("TextView must not scroll independently", 0, view.scrollY)
                }
            }
        }
        val after = capture("scroll-$suffix-after")
        assertTrue("Returning to a response must restore identical pixels", before.sameAs(after))
        before.recycle()
        after.recycle()
        // Append to the visible answer while the reader is partway through it.
        list.performTouchInput { swipeUp(durationMillis = 300) }
        var anchor = 0 to 0
        compose.runOnIdle { anchor = state.firstVisibleItemIndex to state.firstVisibleItemScrollOffset }
        val streamedIndex = anchor.first
        repeat(12) { index ->
            compose.runOnIdle {
                sources.value =
                    sources.value.toMutableList().also { it[streamedIndex] += "\n\nStreaming paragraph $index with **bold** and 🧪." }
            }
        }
        val expected = Markwon.create(compose.activity).toMarkdown(sources.value[streamedIndex]).toString()
        compose.waitUntil(5000) {
            var complete = false
            InstrumentationRegistry.getInstrumentation().runOnMainSync { complete = views().any { it.text.toString() == expected } }
            complete
        }
        compose.runOnIdle {
            assertEquals(
                "Streaming must keep the reading position",
                anchor,
                state.firstVisibleItemIndex to state.firstVisibleItemScrollOffset,
            )
        }
        capture("scroll-$suffix-streaming").recycle()
    }

    @Test fun mixedMarkdownSurvivesScrollingAndStreamingInLightMode() = checkScroll(false, 1f)

    @Test fun mixedMarkdownSurvivesScrollingAndStreamingInDarkModeWithLargeText() = checkScroll(true, 1.5f)
}
