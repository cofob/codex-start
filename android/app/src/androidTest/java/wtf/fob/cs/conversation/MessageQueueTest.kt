package wtf.fob.cs.conversation

import android.graphics.Bitmap
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.*
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Rule
import org.junit.Test
import wtf.fob.cs.app.*
import wtf.fob.cs.data.*
import java.io.File

/** Requires --queue-fixture. No user chat or external model is used. */
class MessageQueueTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    @Test fun queueCanBeViewedEditedReorderedAndForceSentDuringAnActiveTurn() =
        runBlocking {
            val instrumentation = InstrumentationRegistry.getInstrumentation()
            val context = instrumentation.targetContext
            val file = File(context.filesDir, "remote-navigation.json")
            assumeTrue("Run test-android-navigation.py --queue-fixture", file.exists())
            val fixture = JSONObject(file.readText())
            assumeTrue("A local test model is required", fixture.optBoolean("queueFixture"))
            val repo = (context.applicationContext as CodexApplication).repository
            val server = fixture.getString("daemonId")
            var session = fixture.getString("sessionId")
            val thread = fixture.getString("threadId")

            suspend fun rpc(
                method: String,
                params: JSONObject = JSONObject(),
            ) = repo.rpc(server, session, method, params.put("threadId", thread))

            suspend fun queue() = rpc("thread/queue/list", obj("limit" to 100)).getJSONArray("data").objects()

            fun waitText(value: String) = compose.waitUntil(30_000) { compose.onAllNodesWithText(value).fetchSemanticsNodes().isNotEmpty() }
            try {
                val (client, _) = repo.pairInvitation(fixture.getString("invitation"))
                repo.finishPairing(client)
                withTimeout(30_000) { while (repo.servers.value.none { it.id == server && it.status == "Connected" }) delay(100) }
                session = rpc("thread/writeAccess").getString("sessionId")
                rpc("turn/start", obj("input" to JSONArray().put(obj("type" to "text", "text" to "Hold first turn"))))
                // Wait until the loopback model is processing the initial turn.
                delay(1500)
                val image =
                    obj(
                        "type" to "image",
                        "url" to
                            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j5hUAAAAASUVORK5CYII=",
                    )
                rpc(
                    "thread/queue/add",
                    obj(
                        "clientUserMessageId" to "fixture-first",
                        "input" to JSONArray().put(obj("type" to "text", "text" to "Original instruction")).put(image),
                    ),
                )
                rpc(
                    "thread/queue/add",
                    obj(
                        "clientUserMessageId" to "fixture-second",
                        "input" to JSONArray().put(obj("type" to "text", "text" to "Send this now")),
                    ),
                )
                val expectedAttachment = queue().first().getJSONArray("input").getJSONObject(1)
                compose.activity.runOnUiThread {
                    compose.activity.window.addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                    compose.activity.setContent {
                        CodexTheme {
                            var showing by remember { mutableStateOf(false) }
                            Box(Modifier.safeDrawingPadding()) {
                                ConversationQueue(repo, server, session, thread, true, showing, {
                                    showing =
                                        true
                                }, { showing = false })
                            }
                        }
                    }
                }
                compose.waitForIdle()
                waitText("Queue (2)")
                compose.onNodeWithTag("message-queue").performClick()
                waitText("1. Original instruction")
                compose.onAllNodesWithText("Edit")[0].performClick()
                compose.onNodeWithTag("queue-draft").performTextReplacement("Edited instruction")
                compose.onNodeWithText("Attached items are kept when you save.").assertIsDisplayed()
                compose.onNodeWithText("Save message").performClick()
                waitText("1. Edited instruction")
                val edited = queue().first().getJSONArray("input")
                assertEquals("Edited instruction", edited.getJSONObject(0).getString("text"))
                assertEquals(canonicalJson(expectedAttachment), canonicalJson(edited.getJSONObject(1)))
                compose.onAllNodesWithContentDescription("Move message up")[1].performClick()
                waitText("1. Send this now")
                compose.onAllNodesWithContentDescription("Remove queued message")[1].performClick()
                compose.waitUntil(30_000) { compose.onAllNodesWithText("2. Edited instruction").fetchSemanticsNodes().isEmpty() }
                assertEquals(1, queue().size)
                compose.onNodeWithTag("queue-draft").performTextReplacement("Keep my draft")
                compose.onNodeWithContentDescription("Close Queued messages").performClick()
                waitText("Discard queue edits?")
                compose.onNodeWithText("Keep editing").performClick()
                compose.onNodeWithTag("queue-draft").assertTextContains("Keep my draft")
                compose.onNodeWithTag("queue-draft").performTextReplacement("")
                compose.onNodeWithTag("queue-draft").performTextReplacement("Another instruction")
                compose.onNodeWithText("Add to queue").performClick()
                waitText("2. Another instruction")
                val snapshot = rpc("thread/snapshot")
                assertTrue(
                    "First turn must still be active before force send",
                    snapshot.getJSONObject("turns").getJSONArray("data").objects().any {
                        it.text("status") ==
                            "inProgress"
                    },
                )
                compose.onAllNodesWithText("Send now")[0].performClick()
                compose.onNodeWithTag("confirm-queue-send").performClick()
                waitText("1. Another instruction")
                withTimeout(30_000) {
                    while (rpc("thread/snapshot").getJSONObject("items").getJSONArray("data").objects().none {
                            it.optJSONObject("item")?.let(::conversationItem)?.text == "Send this now"
                        }
                    ) {
                        delay(100)
                    }
                }
                compose.waitForIdle()
                val screenshot = instrumentation.uiAutomation.takeScreenshot()
                File(
                    context.filesDir,
                    "navigation-queue.png",
                ).outputStream().use { screenshot.compress(Bitmap.CompressFormat.PNG, 100, it) }
                screenshot.recycle()
                compose.onNodeWithContentDescription("Remove queued message").performClick()
                waitText("The queue is empty")
            } finally {
                val screenshot = instrumentation.uiAutomation.takeScreenshot()
                File(
                    context.filesDir,
                    "navigation-queue.png",
                ).outputStream().use { screenshot.compress(Bitmap.CompressFormat.PNG, 100, it) }
                screenshot.recycle()
                repo.forget(server)
            }
        }
}
