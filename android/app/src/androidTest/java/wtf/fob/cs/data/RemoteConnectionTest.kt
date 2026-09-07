package wtf.fob.cs.data

import android.os.SystemClock
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import wtf.fob.cs.app.*
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.nativeclient.NativeClient
import wtf.fob.cs.navigation.*
import wtf.fob.cs.pairing.*
import wtf.fob.cs.settings.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import java.io.File
import java.util.UUID

/** scripts/test-android-remote.py supplies invitations for isolated test daemons. */
@RunWith(AndroidJUnit4::class)
class RemoteConnectionTest {
    @Test fun twoServerGroupsRegisterRestoreAndKeepIndependentCredentials() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val fixture = File(context.filesDir, "remote-smoke-invitations.json")
        assumeTrue("Start isolated daemons with scripts/test-android-remote.py", fixture.exists())
        val invitations = JSONArray(fixture.readText())
        assertEquals(2, invitations.length())
        val cache = File(context.cacheDir, "remote-smoke-${UUID.randomUUID()}").apply { mkdirs() }
        val timing = JSONArray()

        fun measured(
            phase: String,
            group: Int,
            operation: () -> NativeClient,
        ): NativeClient {
            val start = SystemClock.elapsedRealtime()
            return operation().also {
                timing.put(
                    JSONObject().put("phase", phase).put("group", group).put(
                        "milliseconds",
                        SystemClock.elapsedRealtime() - start,
                    ),
                )
            }
        }
        val saved = mutableListOf<String>()
        val clients = mutableListOf<NativeClient>()
        try {
            for (i in 0 until invitations.length()) {
                val client = measured("cold", i) { NativeClient.fromInvitation(invitations.getString(i), cache.absolutePath) }
                try {
                    val registration = JSONObject(client.enroll("Android native transport test"))
                    assertFalse(registration.isNull("token"))
                    saved += client.savedConnection()
                    val activated = measured("activate", i) { client.activate(cache.absolutePath) }
                    try {
                        assertEquals(0, JSONObject(activated.request("session/list", "{}")).getJSONArray("data").length())
                    } finally {
                        activated.destroy()
                    }
                } finally {
                    client.destroy()
                }
            }
            assertNotEquals(
                JSONObject(saved[0]).getJSONObject("discovery").getString("daemonId"),
                JSONObject(saved[1]).getJSONObject("discovery").getString("daemonId"),
            )
            assertNotEquals(JSONObject(saved[0]).getString("deviceSecret"), JSONObject(saved[1]).getString("deviceSecret"))
            for ((i, connection) in saved.withIndex()) {
                clients +=
                    measured("restore", i) { NativeClient.restore(connection, cache.absolutePath) }
            }
            for (client in clients) {
                val sessions = JSONObject(client.request("session/list", "{}"))
                assertEquals(0, sessions.getJSONArray("data").length())
                client.networkChanged()
                assertEquals(0, JSONObject(client.request("session/list", "{}")).getJSONArray("data").length())
            }
            val overlayCount = saved.count { JSONObject(it).getJSONObject("connection").getString("type") == "yggdrasil" }
            if (overlayCount > 0) {
                val deadline = SystemClock.elapsedRealtime() + 10_000
                while (cache.listFiles().orEmpty().count { it.name.startsWith("host-peers-") && it.extension == "json" } < overlayCount &&
                    SystemClock.elapsedRealtime() < deadline
                ) {
                    Thread.sleep(100)
                }
                val peers = cache.listFiles().orEmpty().filter { it.name.startsWith("host-peers-") && it.extension == "json" }
                assertEquals("Save a separate peer cache for each server", overlayCount, peers.size)
                peers.forEach { assertTrue(JSONArray(it.readText()).length() > 0) }
            }
            clients.forEach { it.destroy() }
            clients.clear()
            // Restore both contexts after all native client objects were destroyed.
            for ((i, connection) in saved.withIndex()) {
                val restored = measured("saved-peers", i) { NativeClient.restore(connection, cache.absolutePath) }
                try {
                    assertTrue(JSONObject(restored.request("device/list", "{}")).getJSONArray("data").length() >= 1)
                } finally {
                    restored.destroy()
                }
            }
        } finally {
            File(context.filesDir, "remote-smoke-timing.json").writeText(timing.toString())
            clients.forEach { it.destroy() }
            cache.deleteRecursively()
        }
    }
}
