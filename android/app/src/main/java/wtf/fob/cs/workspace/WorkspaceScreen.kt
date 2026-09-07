package wtf.fob.cs.workspace

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*

@Composable fun WorkspaceScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    var tab by remember { mutableIntStateOf(0) }
    Column {
        Row {
            listOf("Files", "Terminal").forEachIndexed {
                index,
                title,
                ->
                FilterChip(tab == index, { tab = index }, label = { Text(title) }, modifier = Modifier.padding(end = 8.dp))
            }
        }
        when (tab) {
            0 -> FilesScreen(repo, server, session)
            1 -> key(server, session.text("id")) { TerminalScreen(repo, server, session) }
        }
    }
}
