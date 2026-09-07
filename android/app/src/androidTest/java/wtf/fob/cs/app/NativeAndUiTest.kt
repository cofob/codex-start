package wtf.fob.cs.app

import androidx.activity.compose.setContent
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import wtf.fob.cs.app.*
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.nativeclient.protocolVersion
import wtf.fob.cs.navigation.*
import wtf.fob.cs.pairing.*
import wtf.fob.cs.settings.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import java.util.UUID

@RunWith(AndroidJUnit4::class)
class NativeAndUiTest {
    @get:Rule val compose = createAndroidComposeRule<MainActivity>()

    @Test fun nativeLibraryLoadsAndMaterialConnectionControlsOpen() {
        assertEquals(1u, protocolVersion())
        if (compose
                .onAllNodesWithText(
                    "Get started",
                ).fetchSemanticsNodes()
                .isNotEmpty()
        ) {
            compose.onNodeWithText("Get started").performScrollTo().performClick()
        }
        if (compose
                .onAllNodesWithText(
                    "Add server",
                ).fetchSemanticsNodes()
                .isEmpty()
        ) {
            compose.onNodeWithContentDescription("Switch server or active task").performClick()
        }
        compose.onNodeWithText("Add server").performClick()
        compose.onNodeWithText("Connect to a server").assertIsDisplayed()
        compose.onNodeWithText("Scan QR code").assertIsDisplayed()
        compose.onNodeWithText("Host name or IP address").assertExists()
        compose.onNodeWithText("Port (optional)").assertExists()
    }

    @Test fun guideAndEmptyHomeHaveClearNextActions() {
        var started = false
        var added = false
        var settings = false
        compose.activity.runOnUiThread { compose.activity.setContent { CodexTheme { GuideScreen { started = true } } } }
        compose.onNodeWithText("1. Connect a computer").assertExists()
        compose.onNodeWithText("2. Choose a project").assertExists()
        compose.onNodeWithText("Get started").performScrollTo().performClick()
        compose.runOnIdle { assertTrue(started) }
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    EmptyServersScreen(
                        { added = true },
                        { settings = true },
                    )
                }
            }
        }
        compose.onNodeWithText("Add server").performClick()
        compose.onNodeWithText("Settings").performClick()
        compose.runOnIdle {
            assertTrue(added)
            assertTrue(settings)
        }
    }

    @Test fun keystoreRoundTripSurvivesNewStoreInstanceAndRemoval() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val name = "test-${UUID.randomUUID()}"
        val saved = """{"discovery":{"daemonId":"test-daemon"},"deviceSecret":"private-test-key","token":"private-test-token"}"""
        val store = ConnectionStore(context, name)
        store.save(saved)
        assertEquals(listOf(saved), ConnectionStore(context, name).read())
        val encrypted = context.getSharedPreferences(name, 0).getString("encrypted", "")!!
        assertFalse(encrypted.contains("private-test"))
        store.remove("test-daemon")
        assertTrue(ConnectionStore(context, name).read().isEmpty())
    }

    @Test fun connectedToolInputsUseTheToolSchema() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val catalog = ProtocolCatalog(context)
        val schema =
            JSONObject(
                """{"type":"object","properties":{"query":{"type":"string","description":"Search query"},"limit":{"type":"integer"}},"required":["query"]}""",
            )
        val value = mutableStateOf<Any?>(JSONObject())
        compose.activity.runOnUiThread {
            compose.activity.setContent {
                CodexTheme {
                    androidx.compose.foundation.layout.Column {
                        SchemaField(catalog, schema, "Search files", value.value, { value.value = it })
                    }
                }
            }
        }
        compose.onNodeWithText("Query").performTextInput("build instructions")
        compose.runOnIdle { assertEquals("build instructions", (value.value as JSONObject).getString("query")) }
    }
}
