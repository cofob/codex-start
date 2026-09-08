package wtf.fob.cs.conversation

import org.junit.Assert.*
import org.junit.Test
import wtf.fob.cs.data.*

class ConversationStreamingTest {
    private fun delta(
        type: String,
        id: String,
        text: String,
        index: Int = 0,
    ) = RemoteEvent(
        "host",
        "session",
        0,
        obj(
            "method" to "item/$type",
            "params" to
                obj(
                    "itemId" to id,
                    "delta" to text,
                    "summaryIndex" to index,
                    "contentIndex" to index,
                ),
        ),
    )

    @Test fun batchesKeepEveryChunkAndPreserveUnchangedItems() {
        val user = ConversationItem("u", "userMessage", "Hello")
        val events = (0..100).map { delta("agentMessage/delta", "a", "$it ") }
        val batched = applyConversationDeltas(listOf(user), events)
        val individual = events.fold(listOf(user)) { state, event -> applyConversationDeltas(state, listOf(event)) }
        assertEquals(individual.last().text, batched.last().text)
        assertSame(user, batched.first())
        assertEquals((0..100).joinToString("") { "$it " }, batched.last().text)
    }

    @Test fun liveCommandOutputUpdatesDetailsWithoutChangingTheCommand() {
        val started =
            conversationItem(
                obj(
                    "id" to "tool",
                    "type" to "commandExecution",
                    "command" to "git status",
                    "aggregatedOutput" to "",
                    "status" to "inProgress",
                ),
            )
        val result =
            applyConversationDeltas(
                listOf(started),
                listOf(
                    delta("commandExecution/outputDelta", "tool", "first\n"),
                    delta("commandExecution/outputDelta", "tool", "second\n"),
                ),
            ).single()
        assertEquals("git status\n\nfirst\nsecond\n", activityText(result))
        assertEquals("", started.details!!.text("aggregatedOutput"))
        assertEquals("inProgress", result.details!!.text("status"))
    }

    @Test fun reasoningPartsKeepTheirOrderAndDoNotMixSummaryWithBody() {
        val result =
            applyConversationDeltas(
                emptyList(),
                listOf(
                    delta("reasoning/summaryTextDelta", "r", "Second", 1),
                    delta("reasoning/textDelta", "r", "Body", 0),
                    delta("reasoning/summaryTextDelta", "r", "First", 0),
                    delta("reasoning/summaryTextDelta", "r", " part", 1),
                ),
            ).single()
        assertEquals("First\nSecond part\n\nBody", result.text)
        assertFalse(result.text.contains("null"))
    }

    @Test fun splitUnicodeAndDisplayBoundsRemainValid() {
        val result =
            applyConversationDeltas(
                emptyList(),
                listOf(
                    delta("agentMessage/delta", "a", "Hi \uD83D"),
                    delta("agentMessage/delta", "a", "\uDE80 世界"),
                ),
            ).single()
        assertEquals("Hi 🚀 世界", result.text)
        assertEquals("abc", streamTail("🚀abc", 4))
        assertEquals("🚀abc", streamTail("x🚀abc", 5))
        assertEquals(512 * 1024, streamTail("x".repeat(600 * 1024)).length)
    }

    @Test fun emptyOrUnrelatedDeltasDoNotChangeTheConversation() {
        val items = listOf(ConversationItem("a", "agentMessage", "Keep"))
        assertSame(items, applyConversationDeltas(items, listOf(delta("unknown", "a", "ignore"))))
        assertSame(items, applyConversationDeltas(items, listOf(delta("agentMessage/delta", "", "ignore"))))
    }
}
