package wtf.fob.cs.app

import android.Manifest
import android.app.*
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.edit
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import org.json.JSONArray
import wtf.fob.cs.BuildConfig
import wtf.fob.cs.R
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.pairing.*
import wtf.fob.cs.settings.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import java.io.FileDescriptor
import java.io.PrintWriter

class MonitorService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)

    override fun onBind(intent: Intent?): IBinder? = null

    override fun dump(
        fd: FileDescriptor,
        writer: PrintWriter,
        args: Array<out String>,
    ) {
        if (!BuildConfig.DEBUG) {
            super.dump(fd, writer, args)
            return
        }
        val manager = getSystemService(NotificationManager::class.java)
        val items =
            manager.activeNotifications.map {
                obj(
                    "id" to it.id,
                    "tag" to it.tag,
                    "channel" to it.notification.channelId,
                    "postTime" to it.postTime,
                )
            }
        val channels = manager.notificationChannels.map { obj("id" to it.id, "importance" to it.importance) }
        val repo = (application as CodexApplication).repository
        val queued = runBlocking { repo.pendingAlerts().map { obj("sequence" to it.sequence, "method" to it.message.text("method")) } }
        writer.println(
            "CODEX_START_MONITOR: " +
                obj(
                    "enabled" to manager.areNotificationsEnabled(),
                    "notifications" to JSONArray(items),
                    "channels" to JSONArray(channels),
                    "queued" to JSONArray(queued),
                    "error" to repo.error.value,
                ),
        )
    }

    override fun onCreate() {
        super.onCreate()
        val manager = getSystemService(NotificationManager::class.java)
        listOf(
            "monitor" to "Connection status",
            "complete" to "Task completed",
            "failure" to "Task failed",
            "input" to "Input required",
        ).forEach { (id, title) ->
            manager.createNotificationChannel(
                NotificationChannel(
                    id,
                    title,
                    if (id ==
                        "monitor"
                    ) {
                        NotificationManager.IMPORTANCE_LOW
                    } else {
                        NotificationManager.IMPORTANCE_DEFAULT
                    },
                ),
            )
        }
        val stop =
            PendingIntent.getService(
                this,
                1,
                Intent(this, MonitorService::class.java).setAction("stop"),
                PendingIntent.FLAG_IMMUTABLE,
            )
        val notification =
            NotificationCompat
                .Builder(this, "monitor")
                .setSmallIcon(android.R.drawable.stat_notify_sync)
                .setContentTitle("Codex task monitoring")
                .setContentText("Direct connections are active")
                .setOngoing(true)
                .setContentIntent(openTask("", "", ""))
                .addAction(0, "Stop monitoring", stop)
                .build()
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(1, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(1, notification)
        }
        val repo = (application as CodexApplication).repository
        scope.launch {
            merge(repo.events, repo.servers).collect {
                runCatching {
                    if (!manager.areNotificationsEnabled() ||
                        (
                            Build.VERSION.SDK_INT >= 33 &&
                                checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
                        )
                    ) {
                        return@runCatching
                    }
                    var alerts = repo.pendingAlerts()
                    while (alerts.isNotEmpty()) {
                        var blockedChannel = false
                        for (event in alerts) {
                            val message = event.message
                            val params = message.optJSONObject("params")
                            if (message.text("method") == "serverRequest/resolved") {
                                NotificationManagerCompat
                                    .from(
                                        this@MonitorService,
                                    ).cancel(inputNotificationTag(event.server, event.session, params?.opt("requestId")), 2)
                                repo.acknowledgeAlert(event)
                                continue
                            }
                            val turn = params?.optJSONObject("turn")
                            val channel =
                                when {
                                    message.has("id") -> "input"
                                    message.text("method") == "turn/completed" ->
                                        if (turn?.text("status") ==
                                            "failed"
                                        ) {
                                            "failure"
                                        } else {
                                            "complete"
                                        }
                                    else -> if (params?.optInt("exitCode") == 0) "complete" else "failure"
                                }
                            val title =
                                when (channel) {
                                    "input" -> "Codex needs your input"
                                    "failure" -> "Codex task failed"
                                    else -> "Codex task completed"
                                }
                            if (manager.getNotificationChannel(channel)?.importance ==
                                NotificationManager.IMPORTANCE_NONE
                            ) {
                                blockedChannel = true
                                continue
                            }
                            val thread = params?.text("threadId").orEmpty()
                            val item =
                                NotificationCompat
                                    .Builder(this@MonitorService, channel)
                                    .setSmallIcon(android.R.drawable.stat_notify_chat)
                                    .setContentTitle(
                                        title,
                                    ).setContentText(
                                        "Open the chat on ${repo.servers.value
                                            .firstOrNull { it.id == event.server }
                                            ?.name ?: "your server"}",
                                    ).setContentIntent(
                                        openTask(event.server, event.session, thread),
                                    ).setAutoCancel(true)
                                    .setOnlyAlertOnce(true)
                                    .build()
                            // Reposting the same event after a crash updates the existing notification.
                            if (channel ==
                                "input"
                            ) {
                                NotificationManagerCompat
                                    .from(
                                        this@MonitorService,
                                    ).notify(inputNotificationTag(event.server, event.session, message.opt("id")), 2, item)
                            } else {
                                NotificationManagerCompat
                                    .from(
                                        this@MonitorService,
                                    ).notify("${event.server}/${event.sequence}".hashCode(), item)
                            }
                            repo.acknowledgeAlert(event)
                        }
                        if (blockedChannel) break
                        alerts = repo.pendingAlerts()
                    }
                }.onFailure(repo::report)
            }
        }
    }

    private fun openTask(
        server: String,
        session: String,
        thread: String,
    ): PendingIntent =
        PendingIntent.getActivity(
            this,
            "$server/$session/$thread".hashCode(),
            Intent(this, MainActivity::class.java).putExtra("server", server).putExtra("session", session).putExtra("thread", thread),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

    override fun onStartCommand(
        intent: Intent?,
        flags: Int,
        startId: Int,
    ): Int {
        if (intent?.action ==
            "stop"
        ) {
            getSharedPreferences("monitoring", MODE_PRIVATE).edit { putBoolean("enabled", false) }
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }
        getSharedPreferences("monitoring", MODE_PRIVATE).edit { putBoolean("enabled", true) }
        return START_STICKY
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }
}
