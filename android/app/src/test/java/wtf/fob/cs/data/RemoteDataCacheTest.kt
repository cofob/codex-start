package wtf.fob.cs.data

import kotlinx.coroutines.*
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

class RemoteDataCacheTest {
    @Test fun partialCatalogueIsVisibleBeforeChatReadsFinishAndDoesNotSkipInvalidation() =
        runBlocking {
            val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
            try {
                val entered = CompletableDeferred<Unit>()
                val release = CompletableDeferred<Unit>()
                var calls = 0
                lateinit var cache: RemoteDataCache
                cache =
                    RemoteDataCache(scope) { query ->
                        calls++
                        cache.publishPartial(query, obj("projects" to "ready"))
                        entered.complete(Unit)
                        release.await()
                        obj("complete" to calls)
                    }
                val query = RemoteQuery("host", "", "ui/home")
                val request = cache.refreshAsync(query, false)
                entered.await()
                assertEquals(
                    "ready",
                    cache
                        .observe(query)
                        .value.value!!
                        .text("projects"),
                )
                assertFalse(cache.observe(query).value.loading)
                assertTrue(cache.observe(query).value.refreshing)
                cache.invalidate("host")
                release.complete(Unit)
                assertEquals(2, request.await().getInt("complete"))
            } finally {
                scope.cancel()
            }
        }

    @Test fun connectionErrorsFitTheServerCard() {
        assertEquals(
            "Host closed the connection",
            connectionStatus("reason=IO error: peer closed connection without sending TLS close_notify: https://docs.rs/rustls/long-link"),
        )
        assertEquals("Connection timed out", connectionStatus("reason=server response timed out"))
        assertEquals("Device access was rejected", connectionStatus("device was not authenticated"))
    }

    @Test fun simultaneousReadsShareOneRequestAndFreshDataIsImmediate() =
        runBlocking {
            val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
            try {
                val entered = CompletableDeferred<Unit>()
                val release = CompletableDeferred<Unit>()
                var calls = 0
                val cache =
                    RemoteDataCache(scope, now = { 1000 }) {
                        calls++
                        entered.complete(Unit)
                        release.await()
                        obj("name" to "Project")
                    }
                val query = RemoteQuery("one", "session", "project/list")
                val first = cache.refreshAsync(query, force = false)
                entered.await()
                val second = cache.refreshAsync(query, force = false)
                assertSame(first, second)
                assertTrue(cache.observe(query).value.loading)
                release.complete(Unit)
                first.await()
                assertEquals("Project", cache.read(query).text("name"))
                assertEquals(1, calls)
            } finally {
                scope.cancel()
            }
        }

    @Test fun eventDuringRequestTriggersAnotherRead() =
        runBlocking {
            val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
            try {
                val entered = CompletableDeferred<Unit>()
                val release = CompletableDeferred<Unit>()
                var calls = 0
                val cache =
                    RemoteDataCache(scope) {
                        calls++
                        if (calls ==
                            1
                        ) {
                            entered.complete(Unit)
                            release.await()
                        }
                        obj("revision" to calls)
                    }
                val query = RemoteQuery("one", "session", "thread/list")
                val request = cache.refreshAsync(query, force = false)
                entered.await()
                cache.invalidate("one")
                release.complete(Unit)
                assertEquals(2, request.await().getInt("revision"))
                assertEquals(
                    2,
                    cache
                        .observe(query)
                        .value.value!!
                        .getInt("revision"),
                )
            } finally {
                scope.cancel()
            }
        }

    @Test fun failedRefreshRetainsDataAndDoesNotMixServersOrSessions() =
        runBlocking {
            val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
            try {
                var fail = false
                val cache =
                    RemoteDataCache(scope) {
                        if (fail) error("Disconnected")
                        obj("server" to it.server)
                    }
                val query = RemoteQuery("one", "session", "thread/list")
                cache.read(query)
                fail = true
                assertTrue(runCatching { cache.read(query, true) }.isFailure)
                val state = cache.observe(query).value
                assertEquals("one", state.value!!.text("server"))
                assertEquals("Disconnected", state.error)
                assertFalse(state.refreshing)
                assertNull(cache.peek(query.copy(server = "two")))
                assertNull(cache.peek(query.copy(session = "other")))
                cache.remove("one")
                assertNull(cache.peek(query))
            } finally {
                scope.cancel()
            }
        }

    @Test fun eventInvalidationIsScopedAndTokenDeltasDoNotReload() {
        val query = RemoteQuery.of("one", "session", "thread/snapshot", obj("threadId" to "chat"))
        assertTrue(queryAffectedByEvent(query, "turn/completed", "session", "chat"))
        assertTrue(queryAffectedByEvent(query, "turn/completed", "active-writer", "chat"))
        assertFalse(queryAffectedByEvent(query.copy(method = "ui/chats"), "turn/completed", "other", "chat"))
        assertFalse(queryAffectedByEvent(query, "turn/completed", "session", "other"))
        assertFalse(queryAffectedByEvent(query, "item/agentMessage/delta", "session", "chat"))
        assertTrue(queryAffectedByEvent(query, "connection/reset", "", ""))
        assertEquals(RemoteQuery.of("one", "", "test", obj("a" to 1, "b" to 2)), RemoteQuery.of("one", "", "test", obj("b" to 2, "a" to 1)))
    }
}
