package wtf.fob.cs.pairing

import android.app.Activity
import android.content.Intent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.*
import org.json.JSONObject
import wtf.fob.cs.app.*
import wtf.fob.cs.data.*
import wtf.fob.cs.nativeclient.NativeClient
import wtf.fob.cs.ui.*
import kotlin.time.Duration.Companion.milliseconds

@Composable fun AddServerDialog(
    repo: RemoteRepository,
    invitation: String,
    close: () -> Unit,
) {
    var uri by remember { mutableStateOf(invitation) }
    var host by remember { mutableStateOf("") }
    var port by remember { mutableStateOf("") }
    var found by remember { mutableStateOf<List<JSONObject>>(emptyList()) }
    var pending by remember { mutableStateOf<NativeClient?>(null) }
    var code by remember { mutableStateOf("") }
    var fingerprint by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf("") }
    var scanStatus by remember { mutableStateOf("") }
    var scanned by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val scanner =
        rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
            val contents =
                if (result.resultCode ==
                    Activity.RESULT_OK
                ) {
                    result.data?.getStringExtra(QrCaptureActivity.EXTRA_CONTENTS)
                } else {
                    null
                }
            if (contents == null) {
                scanStatus = "Scanning closed. No QR code was read."
                error = result.data?.getStringExtra(QrCaptureActivity.EXTRA_ERROR).orEmpty()
            } else {
                scanned = true
                uri = contents
                if (isInvitationLink(contents)) {
                    scanStatus = "QR code read. Tap Use invitation to connect."
                    error = ""
                } else {
                    scanStatus = "QR code read."
                    error =
                        "Unsupported invitation format. Expected https://cs.fob.wtf/c# followed by uppercase Base32 data. Scan the QR shown by the current codex-start connect command."
                }
            }
        }

    fun scanAgain() {
        pending?.destroy()
        pending = null
        code = ""
        fingerprint = ""
        uri = ""
        error = ""
        scanStatus = ""
        scanner.launch(Intent(context, QrCaptureActivity::class.java))
    }

    fun register(
        invite: Boolean,
        address: String = "",
        number: Int = 0,
    ) {
        scope.launch {
            if (invite && !isInvitationLink(uri.trim())) {
                error = "Unsupported invitation format. Expected https://cs.fob.wtf/c# followed by uppercase Base32 data."
                return@launch
            }
            busy = true
            error = ""
            runCatching {
                val (client, result) = if (invite) repo.pairInvitation(uri) else repo.pairDirect(address, number)
                if (!result.isNull("token")) {
                    repo.finishPairing(client)
                    close()
                } else {
                    pending = client
                    code = result.getString("code")
                    fingerprint =
                        JSONObject(client.savedConnection()).getJSONObject("discovery").getString("fingerprint")
                }
            }.onFailure { error = it.message.orEmpty() }
            busy = false
        }
    }
    DisposableEffect(Unit) { onDispose { pending?.destroy() } }
    LaunchedEffect(pending) {
        val client = pending ?: return@LaunchedEffect
        val deadline = System.nanoTime() + 600_000_000_000L
        while (System.nanoTime() < deadline) {
            delay(2000.milliseconds)
            try {
                if (withContext(Dispatchers.IO) { client.claim() }) {
                    repo.finishPairing(client)
                    pending = null
                    close()
                    return@LaunchedEffect
                }
            } catch (
                e: Exception,
            ) {
                error = e.message.orEmpty()
                return@LaunchedEffect
            }
        }
        error = "The approval request expired. Start registration again."
    }
    AlertDialog(onDismissRequest = { if (!busy) close() }, title = { Text("Connect to a server") }, text = {
        Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Button(onClick = { scanAgain() }, enabled = !busy) {
                Text(if (scanned || error.isNotEmpty() || code.isNotEmpty()) "Scan again" else "Scan QR code")
            }
            if (scanStatus.isNotEmpty()) Text(scanStatus)
            if (code.isNotEmpty()) {
                Text("Run this command on the host:")
                androidx.compose.foundation.text.selection.SelectionContainer {
                    Text("codex-start approve $code", style = MaterialTheme.typography.titleMedium)
                }
                Text("Server identity: $fingerprint", style = MaterialTheme.typography.bodySmall)
                Text("This request expires in ten minutes.")
            } else {
                OutlinedTextField(uri, {
                    uri = it
                    error = ""
                    scanStatus = ""
                }, enabled = !busy, label = { Text("Invitation") }, maxLines = 3)
                Button(onClick = { register(true) }, enabled = uri.isNotBlank() && !busy) { Text("Use invitation") }
                HorizontalDivider()
                OutlinedTextField(host, { host = it }, label = { Text("Host name or IP address") })
                OutlinedTextField(port, { port = it }, label = { Text("Port (optional)") })
                Button(onClick = {
                    scope.launch {
                        busy = true
                        runCatching {
                            if (port.isNotEmpty() && port.toIntOrNull() == null) error("Invalid port")
                            found = repo.scanHost(host, port.toIntOrNull())
                            if (found.isEmpty()) error("No daemon found on this host")
                        }.onFailure { error = it.message.orEmpty() }
                        busy = false
                    }
                }, enabled = host.isNotBlank() && !busy) { Text("Find servers") }
                found.forEach { item ->
                    val connection = item.getJSONObject("connection")
                    val discovery = item.getJSONObject("discovery")
                    OutlinedButton(onClick = {
                        register(false, connection.getString("host"), connection.getInt("port"))
                    }, enabled = !busy) { Text("${discovery.getString("name")} · ${connection.getInt("port")}") }
                }
            }
            if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
            if (error.isNotEmpty()) Text(error, color = MaterialTheme.colorScheme.error)
        }
    }, confirmButton = { TextButton(onClick = close, enabled = !busy) { Text("Close") } })
}
