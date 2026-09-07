package wtf.fob.cs.conversation

import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.ArrowUpward
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.dp
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.*
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

@RunWith(AndroidJUnit4::class)
class ConversationComposerTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    @Test fun readOnlyDraftHasOneSurfaceAndNoUnderline() {
        var background = Color.Unspecified
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    background = MaterialTheme.colorScheme.surfaceContainerLow
                    Box(Modifier.width(320.dp)) {
                        ConversationComposer(
                            TextFieldValue("Saved draft"),
                            {},
                            enabled = false,
                            compact = false,
                            placeholder = "Enable writing to reply",
                            actions = { Send(false) },
                            tools = { Tools() },
                        )
                    }
                }
            }
        }
        compose.onNodeWithTag("message-input").assertIsNotEnabled()
        compose.onNodeWithText("Saved draft").assertExists()
        compose.onNodeWithContentDescription("Send message").assertIsNotEnabled()
        val input = compose.onNodeWithTag("message-input").captureToImage().toPixelMap()
        // The lower input padding previously had a filled rectangle and an underline.
        for (x in listOf(input.width / 4, input.width / 2, input.width * 3 / 4)) {
            for (y in (input.height - 4) until input.height) assertEquals(background, input[x, y])
        }
    }

    @Test fun shortWindowKeepsDraftAndToolsUsableDuringResize() {
        val compact = mutableStateOf(true)
        val value = mutableStateOf(TextFieldValue("Draft for later", TextRange(6)))
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    Column(Modifier.size(320.dp, 240.dp)) {
                        Text("Chat")
                        Box(Modifier.weight(1f)) { Text("Conversation history") }
                        ConversationComposer(
                            value.value,
                            { value.value = it },
                            enabled = true,
                            compact = compact.value,
                            placeholder = "Message Codex…",
                            actions = { Send(true) },
                            tools = { Tools() },
                        )
                    }
                }
            }
        }
        assertTrue(
            compose
                .onNodeWithTag("message-composer")
                .fetchSemanticsNode()
                .boundsInRoot.height <=
                64 * compose.activity.resources.displayMetrics.density,
        )
        compose.onNodeWithText("Conversation history").assertIsDisplayed()
        compose.onNodeWithContentDescription("Send message").assertIsDisplayed()
        compose.onNodeWithContentDescription("Message tools").performClick()
        compose.onNodeWithContentDescription("Message options").assertIsDisplayed()
        compose.onNodeWithContentDescription("Add attachment").assertIsDisplayed()
        compose.runOnIdle { compact.value = false }
        compose.onNodeWithText("Draft for later").assertExists()
        compose.runOnIdle {
            assertEquals(TextRange(6), value.value.selection)
            compact.value = true
        }
        compose.onNodeWithContentDescription("Hide message tools").performClick()
        assertTrue(
            compose
                .onNodeWithTag("message-composer")
                .fetchSemanticsNode()
                .boundsInRoot.height <=
                64 * compose.activity.resources.displayMetrics.density,
        )
        compose.onNodeWithText("Draft for later").assertIsDisplayed()
    }

    @Composable private fun Send(enabled: Boolean) {
        FilledIconButton(onClick = {}, enabled = enabled) { Icon(Icons.Default.ArrowUpward, "Send message") }
    }

    @Composable private fun Tools() {
        IconButton(onClick = {}) { Icon(Icons.Default.Add, "Add attachment") }
        IconButton(onClick = {}) { Icon(Icons.Default.Tune, "Message options") }
    }
}
