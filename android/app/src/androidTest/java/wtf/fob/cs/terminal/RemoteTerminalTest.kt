package wtf.fob.cs.terminal

import android.view.View
import android.view.ViewGroup
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.*
import org.json.JSONArray
import org.json.JSONObject
import org.json.JSONTokener
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
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
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.time.Duration.Companion.milliseconds

/** Isolated real shell and Git, through the Android client and TLS gateway. No model calls. */
class RemoteTerminalTest {
    @get:Rule val compose = createComposeRule()

    @Test fun shellInputResizeExitAndWorkspaceDiffWork() =
        runBlocking {
            val context = InstrumentationRegistry.getInstrumentation().targetContext
            val file = context.filesDir.resolve("remote-navigation.json")
            assumeTrue("Run scripts/test-android-navigation.py --test-class wtf.fob.cs.terminal.RemoteTerminalTest", file.exists())
            val fixture = JSONObject(file.readText())
            val repo = (context.applicationContext as CodexApplication).repository
            val server = fixture.getString("daemonId")
            val session = obj("id" to fixture.getString("sessionId"), "executionCwd" to fixture.getString("projectPath"))
            lateinit var root: View
            try {
                val (client, _) = repo.pairInvitation(fixture.getString("invitation"))
                repo.finishPairing(client)
                withTimeout(60_000.milliseconds) {
                    while (repo.servers.value.none { it.id == server && it.status == "Connected" }) delay(100.milliseconds)
                }
                compose.setContent {
                    root = LocalView.current.rootView
                    MaterialTheme { Surface(Modifier.fillMaxSize().safeDrawingPadding()) { LegacyTerminalScreen(repo, server, session) } }
                }
                compose.waitUntil(15_000) { compose.onAllNodes(hasText("Start shell") and isEnabled()).fetchSemanticsNodes().isNotEmpty() }
                compose.onNodeWithText("Command (optional)").performTextInput("printf '\\033[32mREADY\\033[0m\\n'; exec sh")
                compose.onNodeWithText("Run").performClick()

                fun find(view: View): TerminalView? {
                    if (view is TerminalView) return view
                    if (view is ViewGroup) for (i in 0 until view.childCount) find(view.getChildAt(i))?.let { return it }
                    return null
                }
                val terminal = compose.runOnUiThread { find(root)!! }

                fun state(): JSONObject {
                    var output: JSONObject? = null
                    val done = AtomicBoolean(false)
                    compose.runOnUiThread {
                        terminal.evaluateJavascript("JSON.stringify(window.codexTerminal.inspect())") {
                            output = JSONObject(JSONTokener(it).nextValue() as String)
                            done.set(true)
                        }
                    }
                    compose.waitUntil(5000) { done.get() }
                    return output!!
                }

                fun lines() = state().getJSONArray("lines").let { array -> (0 until array.length()).map { array.getString(it) } }

                fun send(value: String) = compose.runOnUiThread { terminal.sendKey(value) }
                compose.waitUntil(15_000) { lines().any { it.startsWith("READY") } }
                send("stty size\r")
                compose.waitUntil(10_000) {
                    val size = state()
                    lines().any { it.trim() == "${size.getInt("rows")} ${size.getInt("cols")}" }
                }
                compose.runOnUiThread { terminal.evaluateJavascript("document.getElementById('terminal').style.width = '240px'", null) }
                compose.waitUntil(5000) { state().getInt("cols") < 30 }
                // Resize is debounced on the Compose clock; advance it before asking
                // the real shell for its size on the wall clock.
                compose.waitForIdle()
                compose.mainClock.advanceTimeBy(300)
                delay(500.milliseconds)
                send("stty size\r")
                compose.waitUntil(10_000) {
                    val size = state()
                    lines().any { it.trim() == "${size.getInt("rows")} ${size.getInt("cols")}" }
                }
                // Exceed the old gateway's two-minute request deadline.
                delay(125_000.milliseconds)
                compose.onNodeWithText("Running").assertExists()
                send("printf '\\nAFTER_IDLE\\n'\r")
                compose.waitUntil(10_000) { lines().any { it == "AFTER_IDLE" } }
                send("exit 0\r")
                compose.waitUntil(10_000) { compose.onAllNodesWithText("Process exited: 0").fetchSemanticsNodes().isNotEmpty() }
                assertNull(repo.error.value)

                val setup =
                    repo.rpc(
                        server,
                        session.text("id"),
                        "command/exec",
                        obj(
                            "cwd" to session.text("executionCwd"),
                            "sandboxPolicy" to
                                obj("type" to "workspaceWrite", "writableRoots" to JSONArray(listOf(session.text("executionCwd")))),
                            "command" to
                                JSONArray(
                                    listOf(
                                        "sh",
                                        "-c",
                                        "printf 'new\\n' > demo.txt",
                                    ),
                                ),
                        ),
                    )
                assertEquals(setup.toString(), 0, setup.getInt("exitCode"))
                val diff = repo.request(server, "workspace/diff", obj("sessionId" to session.text("id"), "mode" to "working")) as JSONObject
                assertEquals(0, diff.getInt("exitCode"))
                assertFalse(diff.text("stdout").contains('\u001b'))
                val patch = parseUnifiedDiff(diff.text("stdout"))
                assertTrue(patch.any { it.kind == DiffLineKind.Removed && it.text == "-old" && it.oldNumber == 1 })
                assertTrue(patch.any { it.kind == DiffLineKind.Added && it.text == "+new" && it.newNumber == 1 })
            } finally {
                repo.forget(server)
            }
        }
}
