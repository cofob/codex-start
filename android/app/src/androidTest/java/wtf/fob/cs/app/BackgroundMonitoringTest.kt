package wtf.fob.cs.app

import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
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
import kotlin.time.Duration.Companion.milliseconds

/** The host harness controls process death and Doze outside instrumentation. */
@RunWith(AndroidJUnit4::class)
class BackgroundMonitoringTest {
    private val context get() = InstrumentationRegistry.getInstrumentation().targetContext

    private fun fixture(): Pair<File, JSONObject> {
        val file = File(context.filesDir, "remote-background.json")
        assumeTrue("Run scripts/test-android-background.py", file.exists())
        return file to JSONObject(file.readText())
    }

    @Test fun prepare() =
        runBlocking {
            val (file, data) = fixture()
            val preferences = context.getSharedPreferences("monitoring", Context.MODE_PRIVATE)
            data.put("monitorWasEnabled", preferences.getBoolean("enabled", false))
            file.writeText(data.toString())
            val repo = (context.applicationContext as CodexApplication).repository
            val (client, _) = repo.pairInvitation(data.getString("invitation"))
            repo.finishPairing(client)
            withTimeout(
                30_000.milliseconds,
            ) {
                while (repo.servers.value.none {
                        it.id ==
                            data.getString(
                                "daemonId",
                            ) &&
                            it.status == "Connected"
                    }
                ) {
                    delay(100.milliseconds)
                }
            }
            ActivityScenario.launch(MainActivity::class.java).use { scenario ->
                scenario.onActivity { it.monitor() }
                withTimeout(10_000.milliseconds) {
                    while (!preferences.getBoolean("enabled", false)) delay(100.milliseconds)
                }
            }
        }

    @Test fun inspectNotifications() {
        val (_, data) = fixture()
        val manager = context.getSystemService(NotificationManager::class.java)
        val notifications = manager.activeNotifications
        val cutoff = data.getLong("cutoff")
        for (expectation in data.getJSONArray("expected").objects()) {
            val id = "${data.getString("daemonId")}/${expectation.getLong("sequence")}".hashCode()
            val matches =
                if (expectation.has("requestId")) {
                    val tag = inputNotificationTag(data.getString("daemonId"), data.getString("sessionId"), expectation.get("requestId"))
                    notifications.filter { it.tag == tag && it.id == 2 }
                } else {
                    notifications.filter { it.id == id }
                }
            assertEquals("One notification for event ${expectation.getLong("sequence")}", 1, matches.size)
            assertEquals(expectation.getString("channel"), matches.single().notification.channelId)
            assertTrue("Notification must be delivered before inspection starts", matches.single().postTime <= cutoff)
        }
        val forbidden = data.optJSONArray("forbidden") ?: JSONArray()
        for (i in 0 until forbidden.length()) {
            val id = "${data.getString("daemonId")}/${forbidden.getLong(i)}".hashCode()
            assertTrue("Acknowledged events must not alert again", notifications.none { it.id == id })
        }
        val requests = data.optJSONArray("forbiddenRequests") ?: JSONArray()
        for (i in 0 until requests.length()) {
            val tag = inputNotificationTag(data.getString("daemonId"), data.getString("sessionId"), requests.get(i))
            assertTrue("Resolved approvals must dismiss their notification", notifications.none { it.tag == tag && it.id == 2 })
        }
    }

    @Test fun cleanup() =
        runBlocking {
            val (_, data) = fixture()
            val repo = (context.applicationContext as CodexApplication).repository
            repo.forget(data.getString("daemonId"))
            val manager = context.getSystemService(NotificationManager::class.java)
            for (sequence in 1..16) manager.cancel("${data.getString("daemonId")}/$sequence".hashCode())
            manager.cancel(inputNotificationTag(data.getString("daemonId"), data.getString("sessionId"), 42), 2)
            val enabled = data.optBoolean("monitorWasEnabled")
            context
                .getSharedPreferences("monitoring", Context.MODE_PRIVATE)
                .edit()
                .putBoolean("enabled", enabled)
                .commit()
            if (!enabled) context.stopService(Intent(context, MonitorService::class.java))
        }
}
