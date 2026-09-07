package wtf.fob.cs.data

import org.json.JSONObject
import org.junit.Assert.*
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

class ProtocolValueTest {
    @Test fun chatLabelsUseCodexNamesAndReadableFallbacks() {
        assertEquals("Fix authentication", chatTitle(JSONObject("""{"id":"raw-id","name":"Fix authentication","preview":"Old prompt"}""")))
        assertEquals("Review changes", chatTitle(JSONObject("""{"id":"raw-id","name":"","preview":"Review changes"}""")))
        assertEquals("New chat", chatTitle(JSONObject("""{"id":"raw-id"}""")))
    }

    @Test fun approvalNotificationKeysSeparateServersSessionsAndJsonIdTypes() {
        val key = inputNotificationTag("server-a", "session-a", 42)
        assertNotEquals(key, inputNotificationTag("server-a", "session-a", "42"))
        assertNotEquals(key, inputNotificationTag("server-b", "session-a", 42))
        assertNotEquals(key, inputNotificationTag("server-a", "session-b", 42))
    }

    @Test fun conversationWindowBoundsLargeToolOutputAndHistoryInBothDirections() {
        val items = (0 until 80).map { ConversationItem("$it", "tool", "x".repeat(512 * 1024)) }
        val latest = boundedConversation(items)
        assertEquals("79", latest.last().id)
        assertEquals(32, latest.size)
        val earlier = boundedConversation(items, keepLatest = false)
        assertEquals("0", earlier.first().id)
        assertEquals(32, earlier.size)
    }

    @Test fun formsKeepIntegerPrecisionAndRejectInvalidNumbers() {
        assertEquals(9_007_199_254_740_993L, scalarValue("9007199254740993", "integer"))
        assertEquals(false, scalarValue("false", "boolean"))
        assertTrue(runCatching { scalarValue("3.4", "integer") }.isFailure)
        assertTrue(runCatching { scalarValue("NaN", "number") }.isFailure)
        assertTrue(runCatching { scalarValue("Infinity", "number") }.isFailure)
    }

    @Test fun conversationPreservesUserTextAndToolOutput() {
        val user =
            conversationItem(JSONObject("""{"id":"user-1","type":"userMessage","content":[{"type":"text","text":"Please fix this"}]}"""))
        assertEquals("Please fix this", user.text)
        val tool =
            conversationItem(JSONObject("""{"id":"tool-1","type":"commandExecution","command":"git diff","aggregatedOutput":"change"}"""))
        assertEquals("tool-1", tool.id)
        assertEquals("change", tool.details!!.getString("aggregatedOutput"))
    }
}
