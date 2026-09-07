package wtf.fob.cs.navigation

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.StateRestorationTester
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.dp
import androidx.test.espresso.Espresso.closeSoftKeyboard
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

class AdaptiveNavigationTest {
    @get:Rule val compose = createComposeRule()

    @Test fun resizeRetainsDraftAndRestoresDrawerNavigation() {
        val width = mutableStateOf(400.dp)
        compose.setContent {
            CompositionLocalProvider(LocalDensity provides Density(1f)) {
                Box(Modifier.requiredSize(width.value, 1000.dp)) {
                    AdaptiveNavigation(rememberDrawerState(DrawerValue.Closed), navigation = { Text("Project navigation") }) { persistent ->
                        var draft by remember { mutableStateOf("") }
                        Column {
                            Text(if (persistent) "Two panes" else "One pane")
                            TextField(draft, { draft = it }, Modifier.testTag("draft"))
                        }
                    }
                }
            }
        }
        compose.onNodeWithTag("draft").performTextInput("Keep this draft")
        closeSoftKeyboard()
        compose.runOnIdle { width.value = 900.dp }
        compose.onNodeWithText("Two panes").assertExists()
        compose.onNodeWithText("Project navigation").assertExists()
        compose.onNodeWithTag("draft").assertTextContains("Keep this draft")
        compose.runOnIdle { width.value = 400.dp }
        compose.onNodeWithText("One pane").assertExists()
        compose.onNodeWithTag("draft").assertTextContains("Keep this draft")
    }

    @Test fun savedDraftSurvivesRecreation() {
        val restoration = StateRestorationTester(compose)
        restoration.setContent {
            AdaptiveNavigation(rememberDrawerState(DrawerValue.Closed), navigation = { Text("Navigation") }) {
                var draft by rememberSaveable { mutableStateOf("") }
                TextField(draft, { draft = it }, Modifier.testTag("draft"))
            }
        }
        compose.onNodeWithTag("draft").performTextInput("Saved draft")
        restoration.emulateSavedInstanceStateRestore()
        compose.onNodeWithTag("draft").assertTextContains("Saved draft")
    }
}
