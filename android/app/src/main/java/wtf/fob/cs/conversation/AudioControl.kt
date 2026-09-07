package wtf.fob.cs.conversation

import android.Manifest
import android.content.pm.PackageManager
import android.media.*
import android.util.Base64
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.MicOff
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.core.content.edit
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import kotlinx.coroutines.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import kotlin.time.Duration.Companion.milliseconds

@Composable fun AudioButton(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var active by remember { mutableStateOf(false) }
    var showControls by remember { mutableStateOf(false) }
    var text by remember { mutableStateOf("") }
    var sending by remember { mutableStateOf(false) }
    val preferences = remember(server) { context.getSharedPreferences("voice", 0) }
    var voice by remember(server) { mutableStateOf(preferences.getString(server, "").orEmpty()) }
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    DisposableEffect(lifecycle) {
        val observer = LifecycleEventObserver { _, event -> if (event == Lifecycle.Event.ON_STOP) active = false }
        lifecycle.addObserver(observer)
        onDispose { lifecycle.removeObserver(observer) }
    }
    val permission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { active = it }
    LaunchedEffect(active, server, session, thread) {
        if (!active) return@LaunchedEffect
        var recorder: AudioRecord? = null
        var player: AudioTrack? = null
        try {
            repo.rpc(
                server,
                session,
                "thread/realtime/start",
                obj(
                    "threadId" to thread,
                    "outputModality" to "audio",
                    "transport" to obj("type" to "websocket"),
                ).apply { if (voice.isNotEmpty()) put("voice", voice).put("version", "v2") },
            )
            withContext(Dispatchers.IO) {
                val rate = 24000
                val size = maxOf(4800, AudioRecord.getMinBufferSize(rate, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT))
                check(ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
                    "Microphone permission is required"
                }
                recorder =
                    AudioRecord(
                        MediaRecorder.AudioSource.VOICE_COMMUNICATION,
                        rate,
                        AudioFormat.CHANNEL_IN_MONO,
                        AudioFormat.ENCODING_PCM_16BIT,
                        size * 2,
                    )
                check(recorder!!.state == AudioRecord.STATE_INITIALIZED) { "Microphone is unavailable" }
                player =
                    AudioTrack
                        .Builder()
                        .setAudioAttributes(
                            AudioAttributes
                                .Builder()
                                .setUsage(
                                    AudioAttributes.USAGE_VOICE_COMMUNICATION,
                                ).setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                                .build(),
                        ).setAudioFormat(
                            AudioFormat
                                .Builder()
                                .setSampleRate(
                                    rate,
                                ).setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                                .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                                .build(),
                        ).setBufferSizeInBytes(
                            maxOf(9600, AudioTrack.getMinBufferSize(rate, AudioFormat.CHANNEL_OUT_MONO, AudioFormat.ENCODING_PCM_16BIT)),
                        ).build()
                player!!.play()
                recorder!!.startRecording()
                coroutineScope {
                    val playback =
                        launch {
                            repo.events.collect { event ->
                                if (event.server == server &&
                                    event.session == session &&
                                    event.message.text("method") == "thread/realtime/outputAudio/delta"
                                ) {
                                    val params = event.message.getJSONObject("params")
                                    if (params.text("threadId") == thread) {
                                        val chunk = params.getJSONObject("audio")
                                        check(
                                            chunk.getInt("sampleRate") == rate && chunk.getInt("numChannels") == 1,
                                        ) { "The host returned an unsupported audio format" }
                                        val bytes = Base64.decode(chunk.getString("data"), Base64.DEFAULT)
                                        player!!.write(bytes, 0, bytes.size)
                                    }
                                }
                            }
                        }
                    try {
                        val bytes = ByteArray(4800)
                        while (isActive) {
                            val count = recorder!!.read(bytes, 0, bytes.size)
                            check(count > 0) { "Microphone read failed" }
                            repo.rpc(
                                server,
                                session,
                                "thread/realtime/appendAudio",
                                obj(
                                    "threadId" to thread,
                                    "audio" to
                                        obj(
                                            "data" to Base64.encodeToString(bytes, 0, count, Base64.NO_WRAP),
                                            "sampleRate" to rate,
                                            "numChannels" to 1,
                                            "samplesPerChannel" to count / 2,
                                        ),
                                ),
                            )
                        }
                    } finally {
                        playback.cancel()
                    }
                }
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            repo.report(e)
            active = false
        } finally {
            runCatching { recorder?.stop() }
            recorder?.release()
            player?.release()
            withContext(NonCancellable) {
                withTimeoutOrNull(3000.milliseconds) {
                    runCatching {
                        repo.rpc(
                            server,
                            session,
                            "thread/realtime/stop",
                            obj(
                                "threadId" to thread,
                            ),
                        )
                    }
                }
            }
        }
    }
    IconButton(onClick = {
        showControls = true
    }) { Icon(if (active) Icons.Default.MicOff else Icons.Default.Mic, if (active) "Voice controls" else "Start audio") }
    if (showControls) {
        val (query, state) = rememberRemoteData(repo, server, session, "thread/realtime/listVoices")
        val voices =
            state.value
                ?.optJSONObject("voices")
                ?.optJSONArray("v2")
                ?.let { values ->
                    (0 until values.length()).map { values.getString(it) }
                }.orEmpty()
        NativeSheet("Voice", { showControls = false }) {
            Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState())) {
                Text(if (active) "The microphone is on." else "Talk with Codex about this chat.", Modifier.padding(bottom = 16.dp))
                if (!active) {
                    DataStatus(state) { repo.data.refresh(query, true) }
                    SettingsGroup("Voice") {
                        (listOf("") + voices).forEach { choice ->
                            SettingsRow(if (choice.isEmpty()) "Host default" else humanize(choice), trailing = {
                                RadioButton(
                                    voice == choice,
                                    onClick = {
                                        voice = choice
                                        preferences.edit { putString(server, choice) }
                                    },
                                )
                            }, onClick = {
                                voice = choice
                                preferences.edit { putString(server, choice) }
                            })
                        }
                    }
                } else {
                    OutlinedTextField(text, { text = it }, label = { Text("Type a message") }, modifier = Modifier.fillMaxWidth())
                    Row {
                        listOf(
                            "thread/realtime/appendText" to "Send",
                            "thread/realtime/appendSpeech" to "Read aloud",
                        ).forEach { (method, title) ->
                            TextButton(enabled = !sending && text.isNotBlank(), onClick = {
                                scope.launch {
                                    sending = true
                                    try {
                                        repo.rpc(server, session, method, obj("threadId" to thread, "text" to text))
                                        text = ""
                                    } catch (
                                        error: Exception,
                                    ) {
                                        repo.report(error)
                                    } finally {
                                        sending = false
                                    }
                                }
                            }) { Text(title) }
                        }
                    }
                }
            }
            Button(modifier = Modifier.fillMaxWidth(), onClick = {
                if (active) {
                    active = false
                } else if (ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) ==
                    PackageManager.PERMISSION_GRANTED
                ) {
                    active = true
                } else {
                    permission.launch(Manifest.permission.RECORD_AUDIO)
                }
            }) { Text(if (active) "End voice chat" else "Start voice chat") }
        }
    }
}
