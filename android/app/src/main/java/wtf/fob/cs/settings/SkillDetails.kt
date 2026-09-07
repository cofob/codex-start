package wtf.fob.cs.settings

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.core.content.edit
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*

@Composable fun SkillContentsSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    skill: JSONObject,
    marketplace: String = "",
    plugin: String = "",
    close: () -> Unit,
) {
    val context = LocalContext.current
    var text by remember { mutableStateOf("") }
    var failure by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(true) }
    LaunchedEffect(skill) {
        try {
            text =
                if (marketplace.isNotEmpty() &&
                    plugin.isNotEmpty()
                ) {
                    repo
                        .rpc(
                            server,
                            session,
                            "plugin/skill/read",
                            obj(
                                "remoteMarketplaceName" to marketplace,
                                "remotePluginId" to plugin,
                                "skillName" to skill.text("name"),
                            ),
                        ).text("contents")
                } else {
                    readRemotePreview(context, repo, server, session, skill.text("path"), 1024 * 1024).toString(Charsets.UTF_8)
                }
        } catch (
            error: Exception,
        ) {
            if (error is kotlinx.coroutines.CancellationException) throw error
            failure = error.message.orEmpty()
        } finally {
            busy = false
        }
    }
    NativeSheet(skill.text("name", "Skill"), close) {
        if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
        if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
        Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState())) { Markdown(text) }
    }
}

@Composable fun SkillFoldersSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    close: () -> Unit,
) {
    val preferences = LocalContext.current.getSharedPreferences("skill-folders", 0)
    var folders by remember { mutableStateOf(preferences.getString("$server/$session", "").orEmpty()) }
    var busy by remember { mutableStateOf(false) }
    var failure by remember { mutableStateOf("") }
    val scope = rememberCoroutineScope()
    NativeSheet("Additional skill folders", close, scrollContent = true) {
        Text("Load skills from these folders in this Codex session. Enter one absolute path per line.", Modifier.padding(bottom = 16.dp))
        OutlinedTextField(folders, {
            folders = it
        }, label = { Text("Folders on this computer") }, minLines = 3, maxLines = 8, modifier = Modifier.fillMaxWidth())
        if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
        Button(enabled = !busy, modifier = Modifier.fillMaxWidth().padding(top = 16.dp), onClick = {
            scope.launch {
                busy = true
                failure = ""
                try {
                    val paths =
                        folders
                            .lines()
                            .map(String::trim)
                            .filter(String::isNotEmpty)
                            .distinct()
                    require(paths.all { it.startsWith('/') }) { "Use absolute paths" }
                    repo.rpc(server, session, "skills/extraRoots/set", obj("extraRoots" to JSONArray(paths)))
                    preferences.edit { putString("$server/$session", paths.joinToString("\n")) }
                    repo.data.invalidate(server) { it.session == session && it.method == "skills/list" }
                    repo.data.refreshObserved(server)
                    close()
                } catch (error: Exception) {
                    if (error is kotlinx.coroutines.CancellationException) throw error
                    failure =
                        error.message.orEmpty()
                } finally {
                    busy = false
                }
            }
        }) { Text("Apply folders") }
    }
}
