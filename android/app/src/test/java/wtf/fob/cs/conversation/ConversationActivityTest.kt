package wtf.fob.cs.conversation

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import wtf.fob.cs.data.*

class ConversationActivityTest {
    @Test fun consecutiveThinkingAndToolsShareOneStableBlock() {
        val items =
            listOf("userMessage", "reasoning", "commandExecution", "reasoning", "fileChange", "agentMessage", "reasoning")
                .mapIndexed { index, type -> ConversationItem("$index", type, type) }
        val blocks = conversationBlocks(items)
        assertEquals(listOf(1, 4, 1, 1), blocks.map { it.items.size })
        assertEquals(listOf(false, true, false, true), blocks.map { it.activity })
        assertEquals("1", blocks[1].id)
        assertEquals("1", conversationBlocks(items.take(3))[1].id)
        assertTrue(conversationBlocks(emptyList()).isEmpty())
    }

    @Test fun assistantAndUserMessagesRemainSeparate() {
        val items =
            listOf("agentMessage", "agentMessage", "userMessage", "userMessage")
                .mapIndexed { index, type -> ConversationItem("$index", type, "message") }
        assertEquals(4, conversationBlocks(items).size)
    }

    @Test fun activityShowsReadableThinkingAndCommandOutput() {
        val thinking =
            conversationItem(JSONObject("""{"id":"r","type":"reasoning","summary":["Checking files",{"text":"Checking tests"}]}"""))
        assertEquals("Checking files\nChecking tests", thinking.text)
        val command =
            conversationItem(
                obj(
                    "id" to "c",
                    "type" to "commandExecution",
                    "command" to "git diff",
                    "aggregatedOutput" to "result",
                ),
            )
        assertEquals("git diff\n\nresult", activityText(command))
    }

    @Test fun queueEditsKeepStructuredAttachmentsAndRemoveStaleTextOffsets() {
        val input =
            JSONArray(
                """[{"type":"text","text":"old","text_elements":[{"byte_range":{"start":0,"end":3}}]},{"type":"localImage","path":"/tmp/image.png"},{"type":"text","text":"extra"},{"type":"mention","name":"docs","path":"app://docs"}]""",
            )
        val edited = editQueueInput(input, "new instruction")
        assertEquals(3, edited.length())
        assertEquals("new instruction", queueText(edited))
        assertFalse(edited.getJSONObject(0).has("text_elements"))
        assertEquals(input.getJSONObject(1).toString(), edited.getJSONObject(1).toString())
        assertEquals(input.getJSONObject(3).toString(), edited.getJSONObject(2).toString())
        assertEquals(4, input.length())
        assertEquals("old\nextra", queueText(input))
    }

    @Test fun unchangedQueueTextKeepsItsOriginalMetadata() {
        val input = JSONArray("""[{"type":"text","text":"same","text_elements":[]},{"type":"image","url":"data:image/png;base64,abc"}]""")
        assertEquals(input.toString(), editQueueInput(input, "same").toString())
        assertEquals(1, editQueueInput(input, "").length())
        assertEquals(0, editQueueInput(JSONArray(), " ").length())
    }
}
