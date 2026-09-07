package wtf.fob.cs.navigation

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.StateRestorationTester
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.navigation.NavHostController
import androidx.navigation.compose.rememberNavController
import org.junit.Assert.*
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

class ScreenNavigationTest {
    @get:Rule val compose = createComposeRule()
    private lateinit var navigation: NavHostController

    @Test fun recentChatsIdentifyTheirProfile() {
        var selected = ""
        val recent = org.json.JSONArray()
        listOf("work.a", "work-a").forEach { profile ->
            recent.put(obj("session" to obj("profile" to profile), "chat" to obj("id" to "same-chat", "name" to "Shared chat")))
        }
        compose.setContent {
            CodexTheme {
                AppDrawer(
                    rememberDrawerState(DrawerValue.Open),
                    null,
                    obj("recent" to recent),
                    0,
                    true,
                    {},
                    {},
                    {},
                    {},
                    { selected = it.session.text("profile") },
                )
            }
        }
        compose.onNodeWithText("work.a").performScrollTo().assertIsDisplayed()
        compose.onNodeWithText("work-a").performScrollTo().performClick()
        compose.runOnIdle { assertEquals("work-a", selected) }
    }

    @Composable private fun Screens() {
        navigation = rememberNavController()
        CodexTheme {
            AppScreenHost(navigation) { entry ->
                val page = entry.arguments!!.getInt("page")
                Column(Modifier.fillMaxSize().testTag("level-$page")) {
                    Text("Level $page")
                    when (page) {
                        0 -> ProjectRow("Project A", "/a") { navigation.openScreen(1, "server/a") }
                        1 -> {
                            Text("Project A")
                            ChatRow(obj("id" to "chat-a", "name" to "Chat A")) { navigation.openScreen(2, "server/a", "chat-a") }
                        }
                        2 -> {
                            Text("Chat A")
                            val tab by entry.savedStateHandle.getStateFlow(CHAT_TAB, 2).collectAsState()
                            Text("Tab $tab")
                        }
                    }
                }
            }
        }
    }

    private fun start() {
        compose.setContent { Screens() }
    }

    private fun open(
        page: Int,
        project: String = "server/a",
        chat: String = "chat-a",
        feature: String = FeaturePage.Plugins.name,
    ) {
        compose.runOnIdle { navigation.openScreen(page, project, chat, feature) }
        compose.waitForIdle()
    }

    private fun back() {
        compose.runOnIdle { navigation.popBackStack() }
        compose.waitForIdle()
    }

    private fun level(expected: Int) {
        compose.onNodeWithText("Level $expected").assertIsDisplayed()
    }

    @Test fun tabsKeepOneChatEntryAndBackReturnsThroughProject() {
        start()
        open(2)
        val chatEntry = navigation.currentBackStackEntry!!.id
        listOf(3, 4, 2, 4).forEach { open(it) }
        compose.runOnIdle { assertEquals(chatEntry, navigation.currentBackStackEntry!!.id) }
        compose.onNodeWithText("Tab 4").assertIsDisplayed()
        back()
        level(1)
        back()
        level(0)
        compose.runOnIdle { assertNull(navigation.previousBackStackEntry) }
    }

    @Test fun openingAnotherChatOrProjectReplacesTheOldBranch() {
        start()
        open(2)
        open(2, chat = "chat-b")
        back()
        level(1)
        back()
        level(0)
        open(2)
        open(2, project = "server/b", chat = "chat-c")
        back()
        level(1)
        compose.runOnIdle { assertEquals("server/b", navigation.currentBackStackEntry!!.arguments!!.getString("context")) }
        back()
        level(0)
        compose.runOnIdle { assertNull(navigation.previousBackStackEntry) }
    }

    @Test fun settingsAndSiblingDetailsDoNotLoseTheChatTab() {
        start()
        open(4)
        open(5)
        open(7)
        open(7, feature = FeaturePage.Memory.name)
        back()
        level(5)
        open(7, feature = FeaturePage.Memory.name)
        open(5, feature = FeaturePage.Memory.name)
        level(5)
        back()
        level(2)
        compose.onNodeWithText("Tab 4").assertIsDisplayed()
        back()
        level(1)
        back()
        level(0)
    }

    @Test fun restoredHistoryKeepsOnlyTheChatLevelAndSelectedTab() {
        val restoration = StateRestorationTester(compose)
        restoration.setContent { Screens() }
        open(4)
        restoration.emulateSavedInstanceStateRestore()
        compose.onNodeWithText("Tab 4").assertIsDisplayed()
        back()
        level(1)
        back()
        level(0)
    }
}
