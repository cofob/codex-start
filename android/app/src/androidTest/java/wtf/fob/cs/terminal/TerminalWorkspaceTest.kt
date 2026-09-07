package wtf.fob.cs.terminal

import android.graphics.Bitmap
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.view.inputmethod.EditorInfo
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.*
import org.json.JSONObject
import org.json.JSONTokener
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Rule
import org.junit.Test
import wtf.fob.cs.app.*
import wtf.fob.cs.data.*
import java.util.concurrent.atomic.AtomicBoolean

/** Real host and app-server PTYs. Vim edits only a file in the isolated fixture. */
class TerminalWorkspaceTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    @Test fun multipleWindowsVimImeAndScreenReentry() =
        runBlocking {
            val instrumentation = InstrumentationRegistry.getInstrumentation()
            val context = instrumentation.targetContext
            val file = context.filesDir.resolve("remote-navigation.json")
            assumeTrue("Run scripts/test-android-navigation.py --test-class wtf.fob.cs.terminal.TerminalWorkspaceTest", file.exists())
            val fixture = JSONObject(file.readText())
            val repo = (context.applicationContext as CodexApplication).repository
            val server = fixture.getString("daemonId")
            val session =
                obj(
                    "id" to fixture.getString("sessionId"),
                    "cwd" to fixture.getString("projectPath"),
                    "executionCwd" to fixture.optString("executionCwd", fixture.getString("projectPath")),
                )
            val visible = mutableStateOf(true)

            fun find(view: View): TerminalView? {
                if (view is TerminalView) return view
                if (view is ViewGroup) for (i in 0 until view.childCount) find(view.getChildAt(i))?.let { return it }
                return null
            }

            fun currentSurface(): TerminalView? =
                if (android.os.Build.VERSION.SDK_INT >=
                    29
                ) {
                    android.view.inspector.WindowInspector
                        .getGlobalWindowViews()
                        .asReversed()
                        .firstNotNullOfOrNull(::find)
                } else {
                    find(compose.activity.window.decorView)
                }

            fun surface(): TerminalView = compose.runOnUiThread { currentSurface()!! }

            fun state(): JSONObject {
                var output: JSONObject? = null
                val done = AtomicBoolean(false)
                compose.runOnUiThread {
                    val current = currentSurface()
                    if (current == null) {
                        output = obj("lines" to org.json.JSONArray(), "rows" to 0, "cols" to 0)
                        done.set(true)
                    }
                    current?.evaluateJavascript("JSON.stringify(window.codexTerminal?.inspect())") {
                        output = (JSONTokener(it).nextValue() as? String)?.let(::JSONObject)
                            ?: obj("lines" to org.json.JSONArray(), "rows" to 0, "cols" to 0)
                        done.set(true)
                    }
                }
                compose.waitUntil(5000) { done.get() }
                return output!!
            }

            fun lines() = state().getJSONArray("lines").let { array -> (0 until array.length()).map { array.getString(it) } }

            fun send(text: String) {
                compose.waitUntil(10_000) { state().optBoolean("inputEnabled") }
                compose.runOnUiThread { surface().sendKey(text) }
            }

            fun waitLine(text: String) = compose.waitUntil(15_000) { lines().any { it == text } }

            fun screenshot(stage: String) {
                compose.waitForIdle()
                val image = instrumentation.uiAutomation.takeScreenshot()
                context.filesDir
                    .resolve("navigation-$stage.png")
                    .outputStream()
                    .use { image.compress(Bitmap.CompressFormat.PNG, 100, it) }
                image.recycle()
            }

            fun waitText(text: String) =
                compose.waitUntil(15_000) {
                    compose.onAllNodesWithText(text, substring = true).fetchSemanticsNodes().isNotEmpty()
                }
            try {
                val (client, _) = repo.pairInvitation(fixture.getString("invitation"))
                repo.finishPairing(client)
                withTimeout(60_000) { while (repo.servers.value.none { it.id == server && it.status == "Connected" }) delay(100) }
                compose.runOnUiThread {
                    compose.activity.window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                    compose.activity.setContent {
                        MaterialTheme {
                            Surface(Modifier.fillMaxSize().safeDrawingPadding().imePadding()) {
                                if (visible.value) TerminalScreen(repo, server, session) else Text("Other screen")
                            }
                        }
                    }
                }
                compose.onNodeWithContentDescription("New terminal").performClick()
                compose.onNodeWithText("Host", useUnmergedTree = true).performClick()
                compose.onNodeWithText("Command (optional)").performTextInput("printf '\\nHOST_READY\\n'; exec /bin/sh")
                compose.onNodeWithText("Start shell").performClick()
                waitText("Running")
                waitLine("HOST_READY")
                compose.waitUntil(10_000) { state().getInt("rows") > 10 }
                compose.mainClock.advanceTimeBy(300)
                delay(500)
                send("stty size\r")
                compose.waitUntil(10_000) {
                    val size = state()
                    lines().any { it.trim() == "${size.getInt("rows")} ${size.getInt("cols")}" }
                }
                send("vim -Nu NONE -n terminal-vim.txt\r")
                compose.waitUntil(15_000) { state().text("type") == "alternate" }
                compose.runOnUiThread {
                    val input = surface().onCreateInputConnection(EditorInfo())
                    input.commitText("i", 1)
                    input.setComposingText("wrong", 1)
                    input.setComposingText("héllo 世界", 1)
                    input.commitText("héllo 世界", 1)
                }
                compose.waitUntil(5000) { lines().any { it.contains("héllo 世界") } }
                screenshot("terminal-vim")
                compose.onNodeWithText("Esc", useUnmergedTree = true).performClick()
                send(":wq\r")
                compose.waitUntil(10_000) { state().text("type") == "normal" }
                send("cat terminal-vim.txt\r")
                waitLine("héllo 世界")
                // A sticky modifier must apply to the next IME commit, not terminal response bytes.
                send("sleep 30\r")
                compose.onNodeWithText("Ctrl", useUnmergedTree = true).performClick()
                compose.runOnUiThread { surface().onCreateInputConnection(EditorInfo()).commitText("c", 1) }
                send("printf '\\nINTERRUPTED\\n'\r")
                waitLine("INTERRUPTED")
                compose.onNodeWithContentDescription("Terminal options").performClick()
                compose.onNodeWithText("Rename").performClick()
                compose.onNodeWithText("Name").performTextReplacement("Editor")
                compose.onNodeWithText("Save").performClick()
                waitText("Editor")
                compose.onNodeWithContentDescription("New terminal").performClick()
                compose.onNodeWithText("Command (optional)").performTextInput("printf '\\nSESSION_READY\\n'; exec sh")
                compose.onNodeWithText("Start shell").performClick()
                waitLine("SESSION_READY")
                send("printf '\\nSECOND_WINDOW\\n'\r")
                waitLine("SECOND_WINDOW")
                val editor = if (fixture.optBoolean("containerFixture")) "vim.tiny" else "vim"
                send("$editor -Nu NONE -n session-vim.txt\r")
                compose.waitUntil(15_000) { state().text("type") == "alternate" }
                send("iSession editor")
                compose.onNodeWithText("Esc", useUnmergedTree = true).performClick()
                send(":wq\r")
                compose.waitUntil(10_000) { state().text("type") == "normal" }
                send("cat session-vim.txt\r")
                waitLine("Session editor")
                screenshot("terminal-tabs")
                compose.onNodeWithText("Editor").performClick()
                waitLine("INTERRUPTED")
                assertFalse(lines().any { it == "SECOND_WINDOW" })
                compose.runOnUiThread { visible.value = false }
                waitText("Other screen")
                compose.runOnUiThread { visible.value = true }
                waitLine("INTERRUPTED")
                send("printf '\\nAFTER_RETURN\\n'\r")
                waitLine("AFTER_RETURN")
                compose.onNodeWithContentDescription("Terminal options").performClick()
                compose.onNodeWithText("Full screen").performClick()
                waitLine("AFTER_RETURN")
                compose.onNodeWithContentDescription("Keyboard").assertIsEnabled().performClick()
                compose.waitUntil(10_000) {
                    compose.runOnUiThread {
                        currentSurface()?.let {
                            androidx.core.view.ViewCompat
                                .getRootWindowInsets(
                                    it,
                                )?.isVisible(
                                    androidx.core.view.WindowInsetsCompat.Type
                                        .ime(),
                                )
                        } ==
                            true
                    }
                }
                screenshot("terminal-keyboard")
                androidx.test.espresso.Espresso
                    .closeSoftKeyboard()
                compose.onNodeWithContentDescription("Terminal options").performClick()
                compose.onNodeWithText("Exit full screen").performClick()
                waitLine("AFTER_RETURN")
                // Copy screen uses plain text, never HTML or escape sequences.
                compose.onNodeWithContentDescription("Terminal options").performClick()
                compose.onNodeWithText("Copy selection or screen").performClick()
                val clipboard = context.getSystemService(android.content.Context.CLIPBOARD_SERVICE) as android.content.ClipboardManager
                compose.waitUntil(5000) {
                    clipboard.primaryClip
                        ?.getItemAt(0)
                        ?.text
                        ?.contains("AFTER_RETURN") == true
                }
                send("exit 0\r")
                waitText("Exited · 0")
                compose.onNodeWithContentDescription("Terminal options").performClick()
                compose.onNodeWithText("Close terminal").performClick()
                compose.onNodeWithText("Close terminal").performClick()
                waitLine("SECOND_WINDOW")
                val list = repo.request(server, "terminal/list") as JSONObject
                assertEquals(1, list.getJSONArray("data").length())
            } catch (error: Throwable) {
                screenshot("failure")
                context.filesDir.resolve("navigation-failure.txt").writeText(
                    error.stackTraceToString() + "\n" + runCatching { state().toString(2) }.getOrNull(),
                )
                throw error
            } finally {
                withContext(NonCancellable) {
                    runCatching {
                        val data = (repo.request(server, "terminal/list") as JSONObject).getJSONArray("data")
                        for (i in 0 until data.length()) {
                            repo.request(
                                server,
                                "terminal/close",
                                obj(
                                    "terminalId" to data.getJSONObject(i).text("id"),
                                ),
                            )
                        }
                    }
                }
                repo.forget(server)
            }
        }
}
