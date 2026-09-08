package wtf.fob.cs.workspace

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import wtf.fob.cs.data.*

class WorkOverviewTest {
    private fun chat(
        type: String,
        vararg flags: String,
    ) = obj("status" to obj("type" to type, "activeFlags" to JSONArray(flags.toList())))

    @Test fun pendingInputDoesNotLookLikeRunningWork() {
        assertEquals(ChatWorkState.Running, chatWorkState(chat("active")))
        assertEquals(ChatWorkState.Approval, chatWorkState(chat("active", "waitingOnApproval")))
        assertEquals(ChatWorkState.Input, chatWorkState(chat("active", "waitingOnUserInput")))
        assertEquals(ChatWorkState.Approval, chatWorkState(chat("active", "waitingOnUserInput", "waitingOnApproval")))
        assertEquals(ChatWorkState.Error, chatWorkState(chat("systemError")))
    }

    @Test fun idleAndUnloadedAreNotReportedAsCompleted() {
        assertEquals(ChatWorkState.Idle, chatWorkState(chat("idle")))
        assertEquals(ChatWorkState.Unknown, chatWorkState(chat("notLoaded")))
        assertEquals(ChatWorkState.Unknown, chatWorkState(chat("future-state")))
        assertEquals(ChatWorkState.Unknown, chatWorkState(JSONObject()))
    }

    @Test fun overviewKeepsProfilesAndSortsRecentWork() {
        val entries = JSONArray()
        listOf("work.a", "work-a").forEachIndexed { index, profile ->
            entries.put(
                obj(
                    "session" to obj("id" to profile, "profile" to profile, "cwd" to "/work/app"),
                    "chat" to chat("idle").put("id", "same-thread").put("updatedAt", index),
                ),
            )
        }
        entries.put(entries.getJSONObject(0))
        entries.put(obj("session" to JSONObject()))
        val result = workChats(obj("projectChats" to entries))
        assertEquals(listOf("work-a", "work.a"), result.map { it.session.text("profile") })
        assertNotEquals(workChatKey(result[0]), workChatKey(result[1]))
        assertEquals(2, workChats(obj("recent" to entries)).size)
        assertTrue(workChats(null).isEmpty())
    }

    @Test fun attentionComesFirstAndSearchIncludesProjectAndProfile() {
        val entries =
            listOf("idle", "active", "systemError", "notLoaded").mapIndexed { index, status ->
                RecentChat(
                    obj("cwd" to "/work/project-$index", "profile" to "profile-$index"),
                    chat(status).put("id", "$index").put("name", "Task $index"),
                )
            }
        assertEquals(listOf("Needs attention", "In progress", "Recent"), workGroups(entries, "", WorkFilter.All).map { it.title })
        assertEquals(
            "2",
            workGroups(entries, "", WorkFilter.Attention)
                .single()
                .chats
                .single()
                .chat
                .text("id"),
        )
        assertEquals(
            "1",
            workGroups(entries, "", WorkFilter.Running)
                .single()
                .chats
                .single()
                .chat
                .text("id"),
        )
        assertEquals(
            "3",
            workGroups(entries, " PROJECT-3 ", WorkFilter.All)
                .single()
                .chats
                .single()
                .chat
                .text("id"),
        )
        assertEquals(
            "0",
            workGroups(entries, "Profile-0", WorkFilter.All)
                .single()
                .chats
                .single()
                .chat
                .text("id"),
        )
        assertTrue(workGroups(entries, "missing", WorkFilter.All).isEmpty())
    }
}
