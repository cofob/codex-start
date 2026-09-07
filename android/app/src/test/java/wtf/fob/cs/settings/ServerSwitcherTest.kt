package wtf.fob.cs.settings

import org.json.JSONArray
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
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

class ServerSwitcherTest {
    @Test fun daemonSnapshotIncludesThreadsAndJobsAndReportsPartialResults() {
        val server = RemoteServer("server", "Workstation", "Connected", "fingerprint")
        val interactive = obj("id" to "session-1", "name" to "Project", "cwd" to "/project")
        val job = obj("id" to "job-1", "name" to "Tests", "cwd" to "/project")
        val snapshot =
            activeChatSnapshot(
                server,
                obj(
                    "data" to
                        JSONArray(
                            listOf(
                                obj("session" to interactive, "thread" to obj("id" to "thread-1", "name" to "Fix tests")),
                                obj("session" to job, "thread" to null),
                            ),
                        ),
                    "unavailableSessions" to JSONArray(listOf(obj("id" to "session-2"))),
                ),
            )

        assertTrue(snapshot.incomplete)
        assertEquals(listOf("thread-1", ""), snapshot.tasks.map { it.chat.text("id") })
        assertEquals("Tests", snapshot.tasks[1].chat.text("name"))
        assertEquals(server, snapshot.tasks[0].server)
    }
}
