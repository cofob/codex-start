package wtf.fob.cs.workspace

import android.graphics.Bitmap
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.unit.dp
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.*
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Rule
import org.junit.Test
import wtf.fob.cs.app.CodexTheme
import wtf.fob.cs.app.MainActivity
import wtf.fob.cs.conversation.ConversationScreen
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*
import java.io.File

class HostHistoryTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    private fun capture(stage: String) {
        compose.waitForIdle()
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val bitmap = instrumentation.uiAutomation.takeScreenshot()
        File(instrumentation.targetContext.filesDir, "navigation-$stage.png").outputStream().use {
            bitmap.compress(Bitmap.CompressFormat.PNG, 100, it)
        }
        bitmap.recycle()
    }

    @Test fun mainWorkNavigationOpensSavedChatWithoutLiveOnlyTabs() =
        runBlocking<Unit> {
            val context = InstrumentationRegistry.getInstrumentation().targetContext
            val file = File(context.filesDir, "remote-navigation.json")
            assumeTrue("Run the navigation script with --history-fixture", file.exists())
            val fixture = JSONObject(file.readText())
            val repo = (context.applicationContext as CodexApplication).repository
            val server = fixture.getString("daemonId")
            try {
                compose.runOnUiThread { compose.activity.window.addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON) }
                val (client, _) = repo.pairInvitation(fixture.getString("invitation"))
                repo.finishPairing(client)
                withTimeout(60_000) { while (repo.servers.value.none { it.id == server && it.status == "Connected" }) delay(100) }
                repo.rename(server, "History test server")
                if (compose
                        .onAllNodesWithText(
                            "Get started",
                        ).fetchSemanticsNodes()
                        .isNotEmpty()
                ) {
                    compose.onNodeWithText("Get started").performClick()
                }
                compose.onNodeWithContentDescription("Switch server or active task").performClick()
                compose.onNode(hasText("History test server") and hasText("Connected")).performClick()
                compose.waitUntil(30_000) { compose.onAllNodesWithTag("work-overview").fetchSemanticsNodes().isNotEmpty() }
                compose.onNodeWithText("Search work, projects, and profiles").performTextInput("Desktop Work")
                compose.waitUntil(30_000) { compose.onAllNodesWithText("Desktop Work saved fixture").fetchSemanticsNodes().isNotEmpty() }
                androidx.test.espresso.Espresso
                    .closeSoftKeyboard()
                capture("work")
                compose.onNodeWithText("Desktop Work saved fixture").performClick()
                compose.waitUntil(30_000) { compose.onAllNodesWithTag("conversation-ready").fetchSemanticsNodes().isNotEmpty() }
                compose.onNodeWithText("Read only").assertIsDisplayed()
                compose.onNodeWithText("Files").assertDoesNotExist()
                compose.onNodeWithText("Changes").assertDoesNotExist()
                compose.onNodeWithTag("message-input").assertDoesNotExist()
                capture("read-only")
                assertNull(repo.error.value)
            } finally {
                repo.forget(server)
            }
        }

    @Test fun allLocalClientsPageSearchAndOpenSavedAnswers() =
        runBlocking<Unit> {
            val context = InstrumentationRegistry.getInstrumentation().targetContext
            val file = File(context.filesDir, "remote-navigation.json")
            assumeTrue("Run the navigation script with --history-fixture", file.exists())
            val fixture = JSONObject(file.readText())
            val repo = (context.applicationContext as CodexApplication).repository
            val server = fixture.getString("daemonId")
            val selected = mutableStateOf<RecentChat?>(null)
            val search = mutableStateOf("")
            try {
                val (client, _) = repo.pairInvitation(fixture.getString("invitation"))
                repo.finishPairing(client)
                withTimeout(60_000) { while (repo.servers.value.none { it.id == server && it.status == "Connected" }) delay(100) }
                val first = repo.request(server, "history/list", obj("limit" to 50)) as JSONObject
                assertEquals(65, first.getInt("total"))
                assertEquals(50, first.getJSONArray("data").length())
                val second = repo.request(server, "history/list", obj("cursor" to first.getString("nextCursor"))) as JSONObject
                assertEquals(15, second.getJSONArray("data").length())
                val rows = first.getJSONArray("data").objects() + second.getJSONArray("data").objects()
                assertEquals(65, rows.map { chatIdentity(it.getJSONObject("session"), it.getJSONObject("chat")) }.distinct().size)
                assertTrue(rows.any { it.getJSONObject("chat").opt("source") is JSONObject })
                compose.runOnUiThread {
                    compose.activity.window.addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                    compose.activity.setContent {
                        CodexTheme {
                            Column(Modifier.fillMaxSize().safeDrawingPadding().padding(horizontal = 16.dp)) {
                                if (selected.value == null) {
                                    SearchField(search.value, { search.value = it }, "Search all local chats")
                                    HostHistoryOverview(
                                        repo,
                                        server,
                                        null,
                                        search.value,
                                        true,
                                        { selected.value = it },
                                        {},
                                        {},
                                        showActions = false,
                                    )
                                } else {
                                    ConversationScreen(repo, server, selected.value!!.session, selected.value!!.chat.text("id"), {}, {})
                                }
                            }
                        }
                    }
                }
                compose.waitUntil(30_000) {
                    compose.onAllNodesWithTag("history-load-more").fetchSemanticsNodes().isNotEmpty() ||
                        compose.onAllNodesWithText("All 50").fetchSemanticsNodes().isNotEmpty()
                }
                compose.onNodeWithTag("work-list").performScrollToNode(hasTestTag("history-load-more"))
                compose.onNodeWithTag("history-load-more").performClick()
                compose.waitUntil(30_000) { compose.onAllNodesWithText("All 65").fetchSemanticsNodes().isNotEmpty() }
                compose.onNodeWithTag("history-archived").performClick()
                compose.waitUntil(30_000) { compose.onAllNodesWithText("Saved task 064").fetchSemanticsNodes().isNotEmpty() }
                compose.onNodeWithTag("history-archived").performClick()
                compose.runOnIdle { search.value = "Desktop Work" }
                compose.waitUntil(30_000) { compose.onAllNodesWithText("Desktop Work saved fixture").fetchSemanticsNodes().isNotEmpty() }
                compose.onNodeWithTag("work-list").performScrollToNode(hasText("Desktop Work saved fixture"))
                capture("work")
                compose.onNodeWithText("Desktop Work saved fixture").performClick()
                compose.waitUntil(30_000) { compose.onAllNodesWithTag("conversation-ready").fetchSemanticsNodes().isNotEmpty() }
                val snapshot =
                    repo.rpc(
                        server,
                        selected.value!!.session.text("id"),
                        "thread/snapshot",
                        obj(
                            "threadId" to selected.value!!.chat.text("id"),
                        ),
                    )
                assertTrue(snapshot.getBoolean("recovered"))
                assertTrue(
                    snapshot.getJSONObject("items").getJSONArray("data").objects().any {
                        it.getJSONObject("item").text("text") ==
                            "Saved answer with selectable text."
                    },
                )
                compose.onNodeWithText("Enable writing").assertDoesNotExist()
                compose.onNodeWithText("Queue unavailable").assertDoesNotExist()
                compose.onNodeWithTag("message-input").assertDoesNotExist()
                capture("read-only")
                assertNull(repo.error.value)
            } finally {
                repo.forget(server)
            }
        }
}
