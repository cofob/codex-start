package wtf.fob.cs.ui

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import wtf.fob.cs.data.RemoteServer

@Composable
fun ConnectionNotice(
    server: RemoteServer,
    retry: () -> Unit,
    manage: () -> Unit,
) {
    if (server.status == "Connected") return
    val connecting = server.status == "Connecting"
    val needsPairing = server.status in setOf("Host identity changed. Pair again", "Device access was rejected")
    Surface(
        Modifier.fillMaxWidth().padding(vertical = 8.dp),
        shape = RoundedCornerShape(16.dp),
        color = MaterialTheme.colorScheme.surfaceContainerLow,
    ) {
        Column(Modifier.padding(horizontal = 12.dp, vertical = 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (connecting) {
                    CircularProgressIndicator(Modifier.size(14.dp), strokeWidth = 1.5.dp)
                } else {
                    Icon(Icons.Default.CloudOff, null, Modifier.size(18.dp))
                }
                Text(
                    if (connecting) "Connecting to ${server.name}…" else server.status,
                    style = MaterialTheme.typography.labelLarge,
                )
            }
            if (!connecting) {
                Text(
                    if (needsPairing) {
                        "Check this computer and its device access before you pair again."
                    } else {
                        "Check that ${server.name} is awake and codex-start is running. We will keep trying."
                    },
                    Modifier.padding(top = 4.dp),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Row {
                    if (!needsPairing) TextButton(onClick = retry) { Text("Try now") }
                    TextButton(onClick = manage) { Text("Manage connection") }
                }
            }
        }
    }
}
