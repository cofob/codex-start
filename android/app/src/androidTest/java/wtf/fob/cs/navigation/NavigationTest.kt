package wtf.fob.cs.navigation

import android.graphics.Bitmap
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.*
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
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
import java.io.File
import kotlin.time.Duration.Companion.milliseconds

/** Real Codex app-server, isolated home, no model requests. */
@RunWith(AndroidJUnit4::class)
class NavigationTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    private fun text(value: String) = compose.onNodeWithText(value)

    private fun waitText(value: String) = compose.waitUntil(30_000) { compose.onAllNodesWithText(value).fetchSemanticsNodes().isNotEmpty() }

    private fun screenshot(name: String) {
        compose.waitForIdle()
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        instrumentation.uiAutomation.waitForIdle(500, 3000)
        val image = instrumentation.uiAutomation.takeScreenshot()
        File(instrumentation.targetContext.filesDir, "navigation-$name.png").outputStream().use {
            image.compress(Bitmap.CompressFormat.PNG, 100, it)
        }
        image.recycle()
    }

    @Test fun projectsFoldersChatNamesAndServerSettingsWork() =
        runBlocking {
            val context = InstrumentationRegistry.getInstrumentation().targetContext
            val file = File(context.filesDir, "remote-navigation.json")
            assumeTrue("Run scripts/test-android-navigation.py", file.exists())
            val fixture = JSONObject(file.readText())
            val repo = (context.applicationContext as CodexApplication).repository
            val id = fixture.getString("daemonId")
            val appearance = context.getSharedPreferences("appearance", 0)
            val previousTheme = appearance.getString("theme", null)
            try {
                val (client, _) = repo.pairInvitation(fixture.getString("invitation"))
                repo.finishPairing(client)
                withTimeout(60_000.milliseconds) {
                    while (repo.servers.value.none { it.id == id && it.status == "Connected" }) delay(100.milliseconds)
                }
                assertEquals(
                    fixture.getString("hostname"),
                    repo.servers.value
                        .first { it.id == id }
                        .name,
                )
                repo.rename(id, "Navigation test server")
                if (compose
                        .onAllNodesWithText(
                            "Get started",
                        ).fetchSemanticsNodes()
                        .isNotEmpty()
                ) {
                    text("Get started").performScrollTo().performClick()
                }
                compose.onNodeWithContentDescription("Switch server or active task").performClick()
                compose.onNode(hasText("Navigation test server") and hasText("Connected")).performClick()
                compose.waitUntil(5_000) { compose.onAllNodesWithText("Your workspace").fetchSemanticsNodes().isEmpty() }
                waitText("Navigation project")
                waitText("Review navigation changes")
                screenshot("projects")
                compose.onNodeWithContentDescription("Settings").performClick()
                text("Rename").performScrollTo().performClick()
                compose.onNodeWithText("Name on this device").performTextReplacement("Cancelled name")
                text("Cancel").performClick()
                text("Navigation test server").assertExists()
                text("Rename").performScrollTo().performClick()
                compose.onNodeWithText("Name on this device").performTextReplacement("Work computer")
                text("Save").performClick()
                text("Work computer").assertExists()
                screenshot("settings")
                text("Appearance").performScrollTo().performClick()
                text("Dark").performClick()
                text("Done").performScrollTo().performClick()
                screenshot("settings-dark")
                compose.onNodeWithContentDescription("Back").performClick()
                compose.onNodeWithContentDescription("Open navigation").performClick()
                waitText("Recent chats")
                screenshot("drawer")
                compose.runOnIdle { compose.activity.onBackPressedDispatcher.onBackPressed() }
                text("Add project").performClick()
                waitText(fixture.getString("home"))
                val listing = repo.request(id, "directory/list") as JSONObject
                val folder = listing.getJSONArray("data").objects().firstOrNull { it.getBoolean("directory") }
                if (folder != null) {
                    text(folder.getString("name")).performScrollTo().performClick()
                    waitText(folder.getString("path"))
                    compose.onNodeWithContentDescription("Parent folder").performClick()
                    waitText(fixture.getString("home"))
                }
                text("Use this folder").performClick()
                waitText("Projects")
                compose.onNode(hasText("Navigation project") and hasText(fixture.getString("projectPath"))).performScrollTo().performClick()
                waitText("New chat")
                waitText("Review navigation changes")
                compose.onNodeWithText(fixture.getString("threadId")).assertDoesNotExist()
                screenshot("chats")
                text("Review navigation changes").performClick()
                waitText("Changes")
                text("Changes").assertIsDisplayed()
                text("Files").assertIsDisplayed()
                waitText("Ready")
                text("Read only").assertExists()
                if (compose.onAllNodesWithContentDescription("Message tools").fetchSemanticsNodes().isNotEmpty()) {
                    compose.onNodeWithContentDescription("Message tools").performClick()
                }
                compose.onNodeWithContentDescription("Message options").performClick()
                waitText("Configure")
                text("Use host default").assertExists()
                screenshot("configure")
                text("Done").performClick()
                compose.onNodeWithContentDescription("Task actions").performClick()
                text("Rename chat").assertExists()
                compose.onNodeWithText("thread/name/set").assertDoesNotExist()
                text("Rename chat").performClick()
                text("Chat name").performTextReplacement("Native controls work")
                text("Confirm").performClick()
                waitText("Native controls work")
                compose.onNodeWithTag("message-input").performTextInput("Draft only; do not send")
                compose.onNodeWithContentDescription("Send message").assertIsEnabled()
                text("Read only").performClick()
                waitText("Enable writing")
                compose.onNodeWithContentDescription("Send message").assertIsNotEnabled()
                waitText("Changes")
                screenshot("read-only")
                text("Changes").performClick()
                text("Chat").performClick()
                text("Draft only; do not send").assertExists()
                compose.onNodeWithContentDescription("Back").performClick()
                compose.waitUntil(30_000) { compose.onAllNodes(hasText("New chat") and isEnabled()).fetchSemanticsNodes().isNotEmpty() }
                text("New chat").performClick()
                waitText("Changes")
                compose.waitUntil(30_000) { compose.onAllNodesWithTag("conversation-ready").fetchSemanticsNodes().isNotEmpty() }
                assertNull("A new empty chat must not show a history error", repo.error.value)
                screenshot("new-chat")
                val chats = repo.rpc(id, fixture.getString("sessionId"), "thread/loaded/list").getJSONArray("data")
                assertTrue("A new Codex chat must exist", chats.length() >= 2)
                assertEquals(
                    "Work computer",
                    repo.servers.value
                        .first { it.id == id }
                        .name,
                )
            } catch (error: Throwable) {
                android.util.Log.e("NavigationTest", "Navigation failed", error)
                File(context.filesDir, "navigation-failure.txt").writeText(error.stackTraceToString())
                runCatching {
                    screenshot("failure")
                    File(
                        context.filesDir,
                        "navigation-failure.txt",
                    ).appendText(compose.onAllNodes(isRoot(), useUnmergedTree = true).printToString())
                }
                throw error
            } finally {
                repo.forget(id)
                appearance.edit().putString("theme", previousTheme).apply()
            }
        }
}
