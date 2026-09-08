package wtf.fob.cs.workspace

import android.graphics.Bitmap
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.dp
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONArray
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*

class WorkOverviewDeviceTest {
    @get:Rule val compose = createComposeRule()

    private val entries =
        listOf(
            Triple("Approve the test command", "personal", "waitingOnApproval"),
            Triple("Choose a release target", "work", "waitingOnUserInput"),
            Triple("Improve the Android layout", "work", "running"),
            Triple("Review terminal changes", "personal", "idle"),
            Triple("Review terminal changes", "work", "idle"),
        ).mapIndexed { index, (name, profile, state) ->
            obj(
                "session" to obj("id" to "session-$profile", "profile" to profile, "cwd" to "/work/codex-start"),
                "chat" to
                    obj(
                        "id" to (if (index > 2) "shared-id" else "$index"),
                        "name" to name,
                        "updatedAt" to (10 - index),
                        "status" to
                            obj(
                                "type" to (if (state == "idle") "idle" else "active"),
                                "activeFlags" to JSONArray(if (state.startsWith("waiting")) listOf(state) else emptyList<String>()),
                            ),
                    ),
            )
        }
    private val home = obj("projectChats" to JSONArray(entries))

    private fun capture(name: String) {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        instrumentation.uiAutomation.waitForIdle(200, 3000)
        val file = instrumentation.targetContext.getExternalFilesDir("acceptance")!!.resolve(name)
        instrumentation.uiAutomation.takeScreenshot().let { bitmap ->
            file.outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
            bitmap.recycle()
        }
    }

    @Test fun workFiltersKeepProfilesAndOpenTheCorrectSession() {
        var opened: RecentChat? = null
        var choose = false
        var newWork = false
        compose.setContent {
            MaterialTheme(colorScheme = lightColorScheme()) {
                Surface {
                    Column(Modifier.fillMaxSize().safeDrawingPadding().padding(horizontal = 16.dp)) {
                        Text("Work", Modifier.padding(vertical = 16.dp), style = MaterialTheme.typography.headlineLarge)
                        WorkOverview(home, "", false, true, { opened = it }, { choose = true }, newWork = { newWork = true })
                    }
                }
            }
        }
        compose.onNodeWithText("Needs approval").assertIsDisplayed()
        compose.onNodeWithText("Needs input").assertIsDisplayed()
        capture("work-overview-light.png")
        compose.onNodeWithTag("work-filter-Attention").performClick()
        compose.onNodeWithText("Improve the Android layout").assertDoesNotExist()
        compose.onNodeWithText("Choose a release target").performClick()
        compose.runOnIdle { assertEquals("session-work", opened?.session?.text("id")) }
        compose.onNodeWithTag("work-filter-Running").performScrollTo().performClick()
        compose.onNodeWithText("Improve the Android layout").assertIsDisplayed()
        compose.onNodeWithText("Needs approval").assertDoesNotExist()
        compose.onNodeWithTag("work-filter-All").performScrollTo().performClick()
        compose.onNodeWithTag("work-list").performScrollToNode(hasText("Review terminal changes") and hasText("codex-start · work"))
        compose.onNode(hasText("Review terminal changes") and hasText("codex-start · work")).performClick()
        compose.runOnIdle { assertEquals("work", opened?.session?.text("profile")) }
        compose.onNodeWithTag("work-choose-project").performClick()
        compose.runOnIdle { assertTrue(choose) }
        compose.onNodeWithTag("work-new-chat").performClick()
        compose.runOnIdle { assertTrue(newWork) }
    }

    @Test fun largeTextAndDarkModeKeepActionsAvailable() {
        val search = mutableStateOf("work")
        compose.setContent {
            CompositionLocalProvider(LocalDensity provides Density(LocalDensity.current.density, 1.5f)) {
                MaterialTheme(colorScheme = darkColorScheme()) {
                    Surface {
                        Column(Modifier.fillMaxSize().safeDrawingPadding().padding(horizontal = 16.dp)) {
                            Text("Work", Modifier.padding(vertical = 16.dp), style = MaterialTheme.typography.headlineLarge)
                            WorkOverview(home, search.value, false, true, {}, {})
                        }
                    }
                }
            }
        }
        compose.onNodeWithTag("work-filter-Running").performScrollTo().performClick()
        compose.onNodeWithText("Improve the Android layout").assertIsDisplayed()
        compose.onNodeWithTag("work-choose-project").assertIsDisplayed().assertIsEnabled()
        compose.onNodeWithTag("work-new-chat").assertIsDisplayed().assertIsEnabled()
        capture("work-overview-dark-large-text.png")
        compose.runOnIdle { search.value = "no-such-task" }
        compose.onNodeWithText("No matching work").assertIsDisplayed()
    }

    @Test fun disconnectedEmptyStateAndConnectionRecoveryAreExplicit() {
        var retry = false
        var manage = false
        compose.setContent {
            MaterialTheme {
                Column(Modifier.fillMaxSize().safeDrawingPadding().padding(16.dp)) {
                    ConnectionNotice(
                        RemoteServer("test", "Work computer", "Host is unavailable", ""),
                        { retry = true },
                        { manage = true },
                    )
                    WorkOverview(null, "", false, false, {}, {})
                }
            }
        }
        compose.onNodeWithText("Try now").performClick()
        compose.onNodeWithText("Manage connection").performClick()
        compose.runOnIdle { assertTrue(retry && manage) }
        compose.onNodeWithText("Connect this computer to load your tasks.").assertExists()
        compose.onNodeWithTag("work-choose-project").assertIsNotEnabled()
        compose.onNodeWithTag("work-new-chat").assertIsNotEnabled()
    }

    @Test fun olderHostsKeepProjectAccessAndExplainTheRequiredUpdate() {
        compose.setContent {
            MaterialTheme {
                Column(Modifier.fillMaxSize().safeDrawingPadding().padding(16.dp)) {
                    WorkOverview(null, "", false, true, {}, {}, workSupported = false)
                }
            }
        }
        compose.onNodeWithTag("work-new-chat").assertIsNotEnabled()
        compose.onNodeWithTag("work-choose-project").assertIsEnabled()
        compose.onNodeWithText("Update codex-start on this host to start work without a project.").assertIsDisplayed()
    }
}
