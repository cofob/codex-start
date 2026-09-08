package wtf.fob.cs.workspace

import android.graphics.Bitmap
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.*
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Rule
import org.junit.Test
import wtf.fob.cs.app.*
import wtf.fob.cs.conversation.ConversationScreen
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*
import java.io.File
import java.util.UUID

/** Real TLS gateway and Codex app-server. All folders and model requests are isolated. */
class NewWorkTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    private fun capture(stage: String) {
        compose.waitForIdle()
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        instrumentation.uiAutomation.waitForIdle(500, 3000)
        val bitmap = instrumentation.uiAutomation.takeScreenshot()
        File(instrumentation.targetContext.filesDir, "navigation-$stage.png").outputStream().use {
            bitmap.compress(Bitmap.CompressFormat.PNG, 100, it)
        }
        bitmap.recycle()
    }

    @Test fun newWorkUsesItsOwnHostFolderAndSelectedProfile() =
        runBlocking<Unit> {
            val context = InstrumentationRegistry.getInstrumentation().targetContext
            val file = File(context.filesDir, "remote-navigation.json")
            assumeTrue("Run the navigation script with --work-fixture --queue-fixture", file.exists())
            val fixture = JSONObject(file.readText())
            val work = fixture.getJSONObject("work")
            assumeTrue(fixture.optBoolean("queueFixture") && work.has("workId"))
            val repo = (context.applicationContext as CodexApplication).repository
            val server = fixture.getString("daemonId")
            val selected = mutableStateOf<RecentChat?>(null)
            val thread = mutableStateOf("")
            try {
                val (client, _) = repo.pairInvitation(fixture.getString("invitation"))
                repo.finishPairing(client)
                withTimeout(60_000) {
                    while (repo.servers.value.none { it.id == server && it.status == "Connected" }) delay(100)
                }
                assertTrue(
                    repo.servers.value
                        .first { it.id == server }
                        .capabilities
                        .contains("workProjects"),
                )
                // A new request creates a new folder. Retrying preserves its registered identity.
                val operation = UUID.randomUUID().toString()
                val created = repo.request(server, "project/createWork", obj("workId" to operation)) as JSONObject
                val retried = repo.request(server, "project/createWork", obj("workId" to operation)) as JSONObject
                assertEquals(created.getString("id"), retried.getString("id"))
                assertEquals(work.getString("root") + "/work-" + operation, created.getString("path"))
                val listing =
                    repo.request(
                        server,
                        "directory/list",
                        obj("path" to created.getString("path"), "hidden" to true),
                    ) as JSONObject
                assertTrue(listing.getJSONArray("data").objects().any { it.text("name") == ".git" && it.optBoolean("directory") })
                compose.runOnUiThread {
                    compose.activity.window.addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                    compose.activity.setContent {
                        CodexTheme {
                            Column(Modifier.fillMaxSize().safeDrawingPadding()) {
                                if (selected.value == null) {
                                    NewWorkSheet(repo, server, work.getString("workId"), {}) { selected.value = it }
                                } else {
                                    ConversationScreen(repo, server, selected.value!!.session, thread.value, { thread.value = it }, {})
                                }
                            }
                        }
                    }
                }
                compose.waitUntil(30_000) { compose.onAllNodesWithText("work").fetchSemanticsNodes().isNotEmpty() }
                compose.onNodeWithText("work").performClick()
                compose.onNodeWithText("work").assertIsSelected()
                capture("new-work")
                compose.onNodeWithTag("work-start").performClick()
                compose.waitUntil(30_000) { selected.value != null }
                assertEquals(work.getString("sessionId"), selected.value!!.session.text("id"))
                assertEquals("work", selected.value!!.session.text("profile"))
                assertEquals(work.getJSONObject("project").getString("path"), selected.value!!.session.text("cwd"))
                compose.onNodeWithTag("message-input").performTextInput("Projectless work fixture")
                compose.onNodeWithContentDescription("Send message").assertIsEnabled().performClick()
                compose.waitUntil(30_000) { thread.value.isNotEmpty() }
                compose.waitUntil(30_000) {
                    compose.onAllNodesWithContentDescription("Interrupt task").fetchSemanticsNodes().isNotEmpty()
                }
                val chat =
                    repo
                        .rpc(
                            server,
                            selected.value!!.session.text("id"),
                            "thread/read",
                            obj("threadId" to thread.value),
                        ).getJSONObject("thread")
                assertEquals(work.getJSONObject("project").getString("path"), chat.getString("cwd"))
                assertNotEquals(created.getString("path"), chat.getString("cwd"))
                compose.waitUntil(30_000) { compose.onAllNodesWithTag("conversation-ready").fetchSemanticsNodes().isNotEmpty() }
                assertNull(repo.error.value)
                androidx.test.espresso.Espresso
                    .closeSoftKeyboard()
                capture("work-chat")
                compose.onNodeWithContentDescription("Interrupt task").performClick()
            } finally {
                repo.forget(server)
            }
        }
}
