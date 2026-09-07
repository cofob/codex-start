package wtf.fob.cs.settings

import android.graphics.Bitmap
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.safeDrawingPadding
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
import wtf.fob.cs.data.*
import java.io.File

/** Uses only the temporary host settings from test-android-navigation.py. */
class LauncherSettingsTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    private fun text(value: String) = compose.onNodeWithText(value)

    private fun waitText(value: String) =
        compose.waitUntil(30_000) {
            compose.onAllNodesWithText(value).fetchSemanticsNodes().isNotEmpty()
        }

    private fun showSettings(
        repo: RemoteRepository,
        server: String,
    ) {
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    Box(Modifier.safeDrawingPadding()) { LauncherSettingsScreen(repo, server) }
                }
            }
        }
    }

    private fun edit(value: String) {
        compose.waitUntil(30_000) {
            compose.onAllNodes(hasText("Settings (TOML)") and isEnabled()).fetchSemanticsNodes().isNotEmpty()
        }
        text("Settings (TOML)").performScrollTo().performTextReplacement(value)
        text("Save settings").performScrollTo().performClick()
        compose.waitUntil(30_000) { compose.onAllNodesWithText("Save settings").fetchSemanticsNodes().isEmpty() }
    }

    @Test fun globalProjectAndTwoProfilesCanBeEditedAndConflictsKeepTheDraft() =
        runBlocking {
            val instrumentation = InstrumentationRegistry.getInstrumentation()
            val context = instrumentation.targetContext
            val fixtureFile = File(context.filesDir, "remote-navigation.json")
            assumeTrue("Run scripts/test-android-navigation.py with LauncherSettingsTest", fixtureFile.exists())
            val fixture = JSONObject(fixtureFile.readText())
            val repo = (context.applicationContext as CodexApplication).repository
            val server = fixture.getString("daemonId")
            try {
                val (client, _) = repo.pairInvitation(fixture.getString("invitation"))
                repo.finishPairing(client)
                withTimeout(30_000) {
                    while (repo.servers.value.none { it.id == server && it.status == "Connected" }) delay(100)
                }
                showSettings(repo, server)
                waitText("Edit global settings")
                text("Edit global settings").performScrollTo().performClick()
                edit("# Mobile settings\n[settings]\nnetwork = \"bridge\"\n")
                for (profile in listOf("mobile-work", "mobile-review")) {
                    text("New profile name").performScrollTo().performTextReplacement(profile)
                    text("Add profile").performScrollTo().performClick()
                    edit("[settings]\nnetwork = \"offline\"\nrebuild = false\n")
                    waitText(profile)
                }
                val names = (repo.request(server, "launcher/list") as JSONObject).getJSONArray("profiles")
                assertEquals(2, names.length())
                text("Navigation project").performScrollTo().performClick()
                edit("[settings]\nprofile = \"mobile-work\"\n")
                val project =
                    (repo.request(server, "project/list") as JSONObject).getJSONArray("data").objects().first {
                        it.text("name") ==
                            "Navigation project"
                    }
                val projectSettings =
                    repo.request(
                        server,
                        "launcher/read",
                        obj("scope" to "project", "projectId" to project.text("id")),
                    ) as JSONObject
                assertTrue(projectSettings.text("text").contains("mobile-work"))
                text("Edit global settings").performScrollTo().performClick()
                compose.waitUntil(
                    30_000,
                ) { compose.onAllNodes(hasText("Settings (TOML)") and isEnabled()).fetchSemanticsNodes().isNotEmpty() }
                val global = repo.request(server, "launcher/read", obj("scope" to "global")) as JSONObject
                repo.request(
                    server,
                    "launcher/write",
                    obj(
                        "scope" to "global",
                        "version" to global.text("version"),
                        "text" to (global.text("text") + "\n# Changed by another client\n"),
                    ),
                )
                text("Settings (TOML)").performScrollTo().performTextReplacement("[settings]\nrebuild=true\n")
                compose.activityRule.scenario.recreate()
                showSettings(repo, server)
                waitText("Settings (TOML)")
                text("Settings (TOML)").assertTextContains("[settings]\nrebuild=true\n")
                text("Save settings").performScrollTo().performClick()
                compose.waitUntil(30_000) {
                    compose.onAllNodesWithText("settings changed on the host", substring = true).fetchSemanticsNodes().isNotEmpty()
                }
                text("Settings (TOML)").assertTextContains("[settings]\nrebuild=true\n")
                text("Reload").performScrollTo().performClick()
                waitText("Discard edits?")
                text("Discard").performClick()
                compose.waitUntil(30_000) {
                    compose.onAllNodesWithText("Changed by another client", substring = true).fetchSemanticsNodes().isNotEmpty()
                }
                compose.waitForIdle()
                instrumentation.uiAutomation.waitForIdle(500, 3000)
                val image = instrumentation.uiAutomation.takeScreenshot()
                File(
                    context.filesDir,
                    "navigation-launcher-settings.png",
                ).outputStream().use { image.compress(Bitmap.CompressFormat.PNG, 100, it) }
                image.recycle()
            } finally {
                repo.forget(server)
            }
        }
}
