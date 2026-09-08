package wtf.fob.cs.workspace

import org.json.JSONArray
import org.junit.Assert.*
import org.junit.Test
import wtf.fob.cs.data.*

class HistoryDiscoveryTest {
    @Test fun sourceFilterIncludesEveryPinnedProtocolSource() {
        val sources = chatSources()
        assertEquals(
            setOf(
                "cli",
                "vscode",
                "exec",
                "appServer",
                "subAgent",
                "subAgentReview",
                "subAgentCompact",
                "subAgentThreadSpawn",
                "subAgentOther",
                "unknown",
            ),
            (
                0 until
                    sources.length()
            ).map(sources::getString).toSet(),
        )
    }

    @Test fun projectFilterMatchesHostAndContainerPaths() {
        val session = obj("cwd" to "/Users/test/project", "executionCwd" to "/project")
        val params = obj("cwd" to chatWorkingDirectories(session))
        assertTrue(params.opt("cwd") is JSONArray)
        assertTrue(matchesChatDirectory(obj("cwd" to "/project"), params))
        assertTrue(matchesChatDirectory(obj("cwd" to "/Users/test/project"), params))
        assertFalse(matchesChatDirectory(obj("cwd" to "/other/project"), params))
        assertEquals("/project", chatWorkingDirectories(obj("cwd" to "/project", "executionCwd" to "/project")))
    }

    @Test fun storedCopiesKeepSeparateIdentitiesAndDoNotClaimLiveState() {
        val session = obj("profile" to "work", "cwd" to "/project")
        val first = obj("id" to "same", "historyStore" to "host", "source" to obj("subagent" to "review"))
        val second = obj("id" to "same", "historyStore" to "managed")
        assertNotEquals(chatIdentity(session, first), chatIdentity(session, second))
        assertEquals("Agent thread", chatOrigin(first))
        assertEquals(ChatWorkState.Unknown, chatWorkState(first))
    }
}
