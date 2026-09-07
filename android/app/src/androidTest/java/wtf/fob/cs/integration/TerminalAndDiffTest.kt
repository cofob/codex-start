package wtf.fob.cs.integration

import android.graphics.Bitmap
import android.util.Base64
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.json.JSONTokener
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

class TerminalAndDiffTest {
    @get:Rule val compose = createComposeRule()

    @Test fun terminalInterpretsCursorControlsSplitUtf8AndAlternateScreen() {
        lateinit var terminal: TerminalView
        val ready = AtomicBoolean(false)
        val input = ConcurrentLinkedQueue<String>()
        val sizes = ConcurrentLinkedQueue<Pair<Int, Int>>()
        compose.setContent {
            val context = LocalContext.current
            AndroidView(factory = {
                TerminalView(context, { _, _ -> ready.set(true) }, input::add, { c, r -> sizes.add(c to r) }).also { terminal = it }
            }, modifier = Modifier.fillMaxWidth().height(360.dp))
        }
        compose.waitUntil(15_000) { ready.get() }

        fun write(bytes: ByteArray) = compose.runOnUiThread { terminal.writeBase64(Base64.encodeToString(bytes, Base64.NO_WRAP)) }

        fun write(text: String) = write(text.toByteArray())

        fun state(): JSONObject {
            var output: JSONObject? = null
            val done = AtomicBoolean(false)
            compose.runOnUiThread {
                terminal.evaluateJavascript("JSON.stringify(window.codexTerminal.inspect())") { result ->
                    output = JSONObject(JSONTokener(result).nextValue() as String)
                    done.set(true)
                }
            }
            compose.waitUntil(5000) { done.get() }
            return output!!
        }

        fun awaitLine(text: String) {
            compose.waitUntil(5000) { state().getJSONArray("lines").getString(0) == text }
        }
        try {
            write("old progress\r\u001b[2K\u001b[32mReady\u001b[0m")
            awaitLine("Ready")
            assertEquals(5, state().getInt("cursorX"))
            write("\u001b[2J\u001b[H")
            val utf8 = "€界".toByteArray()
            write(utf8.copyOfRange(0, 1))
            write(utf8.copyOfRange(1, 4))
            write(utf8.copyOfRange(4, utf8.size))
            awaitLine("€界")
            write("\u001b[?1049h\u001b[2J\u001b[HFull screen")
            awaitLine("Full screen")
            assertEquals("alternate", state().getString("type"))
            write("\u001b[?1049l")
            awaitLine("€界")
            compose.runOnUiThread {
                terminal.setActive(true)
                terminal.sendKey("\u0003")
                terminal.sendKey("\u001b[A")
                terminal.sendKey("é\r")
            }
            compose.waitUntil(5000) { input.size >= 3 }
            assertEquals(
                listOf("\u0003", "\u001b[A", "é\r"),
                input.take(3).map { Base64.decode(it, Base64.DEFAULT).toString(Charsets.UTF_8) },
            )
            compose.runOnUiThread { terminal.evaluateJavascript("document.getElementById('terminal').style.width = '200px'", null) }
            compose.waitUntil(5000) { sizes.any { it.first < 30 && it.second > 0 } }
            write("\u001b[2J\u001b[H\u001b[32mGreen\u001b[0m\r\n\u001b[31mRed\u001b[0m\r\nCursor and Unicode: €界")
            awaitLine("Green")
            screenshot("terminal.png")
        } finally {
            compose.runOnUiThread { terminal.close() }
        }
    }

    @Test fun diffShowsGreenAndRedRowsAndSourceNumbers() {
        compose.setContent {
            MaterialTheme(colorScheme = lightColorScheme()) {
                UnifiedDiffView(
                    "diff --git a/demo.kt b/demo.kt\n--- a/demo.kt\n+++ b/demo.kt\n@@ -10,2 +10,2 @@\n same\n-old text\n+new text\n",
                )
            }
        }
        compose.onNodeWithText("+1 added").assertIsDisplayed()
        compose.onNodeWithText("−1 removed").assertIsDisplayed()
        val added = compose.onNodeWithText("      11 │ +new text", substring = true)
        val removed = compose.onNodeWithText("   11       │ -old text", substring = true)
        added.assertIsDisplayed()
        removed.assertIsDisplayed()
        assertEquals(Color(0xFFDAFBE1), added.captureToImage().toPixelMap()[1, 1])
        assertEquals(Color(0xFFFFEBE9), removed.captureToImage().toPixelMap()[1, 1])
        screenshot("diff-light.png")
    }

    @Test fun darkDiffKeepsGreenAndRedRows() {
        compose.setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                UnifiedDiffView("@@ -1 +1 @@\n-old\n+new\n")
            }
        }
        assertEquals(Color(0xFF123122), compose.onNodeWithText("+new", substring = true).captureToImage().toPixelMap()[1, 1])
        assertEquals(Color(0xFF3C1C23), compose.onNodeWithText("-old", substring = true).captureToImage().toPixelMap()[1, 1])
        screenshot("diff-dark.png")
    }

    private fun screenshot(name: String) {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val file = instrumentation.targetContext.getExternalFilesDir("acceptance")!!.resolve(name)
        instrumentation.uiAutomation.waitForIdle(200, 3000)
        instrumentation.uiAutomation.takeScreenshot().let { bitmap ->
            file.outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
            bitmap.recycle()
        }
    }
}
