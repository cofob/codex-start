package wtf.fob.cs.data

import android.os.SystemClock
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.*
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
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
import java.io.File
import java.util.Collections
import kotlin.time.Duration.Companion.milliseconds

/** Uses the app's saved connections and automatic reconnect loop. No model calls. */
@RunWith(AndroidJUnit4::class)
class YggdrasilStabilityTest {
    @Test fun twoGroupsStayConnectedAndRecoverAfterDaemonRestart() =
        runBlocking {
            val context = InstrumentationRegistry.getInstrumentation().targetContext
            val fixture = File(context.filesDir, "remote-stability.json")
            assumeTrue("Run scripts/test-android-yggdrasil-stability.py", fixture.exists())
            val config = JSONObject(fixture.readText())
            val duration = config.getLong("durationSeconds")
            require(duration in 360..3600)
            val repo = (context.applicationContext as CodexApplication).repository
            val servers = mutableListOf<String>()
            val devices = mutableListOf<String>()
            val hostKeys = mutableListOf<String>()
            val samples = Collections.synchronizedList(mutableListOf<JSONObject>())
            val report = File(context.filesDir, "remote-stability-report.json")

            fun save(
                elapsed: Long,
                finished: Boolean = false,
            ) {
                val snapshot = synchronized(samples) { JSONArray(samples.toList()) }
                val next = File(report.path + ".new")
                next.writeText(obj("ready" to true, "finished" to finished, "elapsedSeconds" to elapsed, "samples" to snapshot).toString())
                check(next.renameTo(report))
            }
            try {
                val invitations = config.getJSONArray("invitations")
                assertEquals(2, invitations.length())
                for (i in 0 until invitations.length()) {
                    val (client, registration) = repo.pairInvitation(invitations.getString(i))
                    devices += registration.getString("deviceId")
                    val saved = JSONObject(client.savedConnection())
                    val id = saved.getJSONObject("discovery").getString("daemonId")
                    hostKeys += saved.getJSONObject("connection").getString("publicKey")
                    servers += id
                    repo.finishPairing(client)
                }
                assertNotEquals(servers[0], servers[1])
                withTimeout(120_000.milliseconds) {
                    while (servers.any { id -> repo.servers.value.none { it.id == id && it.status == "Connected" } }) {
                        delay(250.milliseconds)
                    }
                }
                val start = SystemClock.elapsedRealtime()
                save(0)
                coroutineScope {
                    val probes =
                        servers.mapIndexed { index, id ->
                            launch(Dispatchers.IO) {
                                while ((SystemClock.elapsedRealtime() - start) / 1000 < duration) {
                                    val elapsed = (SystemClock.elapsedRealtime() - start) / 1000
                                    // The first connection sends only native heartbeat traffic for a minute.
                                    if (index == 0 && elapsed in 60 until 120) {
                                        delay(1000.milliseconds)
                                        continue
                                    }
                                    val before = SystemClock.elapsedRealtime()
                                    val result =
                                        runCatching {
                                            val response = repo.request(id, "device/list") as JSONObject
                                            assertTrue(
                                                "Saved device registration must survive restart",
                                                response.getJSONArray("data").objects().any {
                                                    it.text("id") ==
                                                        devices[index]
                                                },
                                            )
                                        }
                                    val latency = SystemClock.elapsedRealtime() - before
                                    val direct =
                                        runCatching {
                                            repo
                                                .transportPeers(
                                                    id,
                                                ).firstOrNull { it.active && it.publicKey == hostKeys[index] }
                                        }.getOrNull()
                                    samples +=
                                        obj(
                                            "server" to index,
                                            "second" to elapsed,
                                            "latencyMs" to latency,
                                            "ok" to result.isSuccess,
                                            "directLan" to (direct != null),
                                            "directRttMs" to direct?.rttMs,
                                            "directReceivedBytes" to direct?.receivedBytes?.toString(),
                                            "directSentBytes" to direct?.sentBytes?.toString(),
                                            "state" to
                                                repo.servers.value.firstOrNull { it.id == id }?.status?.let {
                                                    if (it ==
                                                        "Connected"
                                                    ) {
                                                        "connected"
                                                    } else {
                                                        "reconnecting"
                                                    }
                                                },
                                        )
                                    delay(3000.milliseconds)
                                }
                            }
                        }
                    while (probes.any { it.isActive }) {
                        save((SystemClock.elapsedRealtime() - start) / 1000)
                        delay(1000.milliseconds)
                    }
                }
                save((SystemClock.elapsedRealtime() - start) / 1000, true)
                val copy = synchronized(samples) { samples.toList() }
                if (config.optBoolean("requireLan")) {
                    for (index in servers.indices) {
                        val lan = copy.filter { it.getInt("server") == index && it.getBoolean("directLan") }
                        assertTrue("Multicast must discover a direct link to host $index", lan.isNotEmpty())
                        assertTrue(
                            "The direct link must carry traffic for host $index",
                            lan.any {
                                it.getString("directReceivedBytes").toLong() >
                                    0 &&
                                    it.getString("directSentBytes").toLong() > 0
                            },
                        )
                    }
                }
                assertTrue("Control daemon must stay connected", copy.filter { it.getInt("server") == 1 }.all { it.getBoolean("ok") })
                val first = copy.filter { it.getInt("server") == 0 }
                assertTrue("The test must observe the planned outage", first.any { !it.getBoolean("ok") })
                val lastFailure = first.last { !it.getBoolean("ok") }.getLong("second")
                assertTrue("The first daemon must recover", first.any { it.getBoolean("ok") && it.getLong("second") > lastFailure })
                assertTrue(
                    "The first daemon must remain connected after recovery",
                    first
                        .filter {
                            it.getLong("second") > duration - 60
                        }.all { it.getBoolean("ok") },
                )
            } finally {
                servers.forEach { repo.forget(it) }
            }
        }
}
