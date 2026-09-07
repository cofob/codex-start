package wtf.fob.cs.workspace

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test

class ProjectScreensTest {
    @Test fun chatsAreGroupedByProjectPathAndKeepTheirSession() {
        val one = JSONObject().put("id", "one").put("name", "One").put("path", "/work/one")
        val two = JSONObject().put("id", "two").put("name", "Two").put("path", "/work/two")
        val firstSession = JSONObject().put("id", "session-one").put("cwd", "/work/one")
        val secondSession = JSONObject().put("id", "session-two").put("cwd", "/work/two")
        val home =
            JSONObject()
                .put("projects", JSONArray(listOf(one, two)))
                .put(
                    "projectChats",
                    JSONArray(
                        listOf(
                            JSONObject().put("session", secondSession).put("chat", JSONObject().put("id", "chat-two")),
                            JSONObject().put("session", firstSession).put("chat", JSONObject().put("id", "chat-one")),
                        ),
                    ),
                )

        val groups = projectChatGroups(home)

        assertEquals(listOf("one", "two"), groups.map { it.project.getString("id") })
        assertEquals(listOf("chat-one"), groups[0].chats.map { it.chat.getString("id") })
        assertEquals(
            "session-one",
            groups[0]
                .chats
                .single()
                .session
                .getString("id"),
        )
        assertEquals(listOf("chat-two"), groups[1].chats.map { it.chat.getString("id") })
    }
}
