package wtf.fob.cs.settings

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.lifecycle.ViewModel
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.ui.*

// Keep settings drafts across rotation, but never persist their possible secrets.
internal class LauncherSettingsState : ViewModel() {
    var editor by mutableStateOf<LauncherSettingsDraft?>(null)
}

internal class LauncherSettingsDraft(
    val target: JSONObject,
) {
    var snapshot by mutableStateOf<JSONObject?>(null)
    var text by mutableStateOf("")
}

@Composable fun LauncherSettingsScreen(
    repo: RemoteRepository,
    server: String,
) {
    val servers by repo.servers.collectAsStateWithLifecycle()
    val supported = servers.firstOrNull { it.id == server }?.capabilities?.contains("launcherSettings") == true
    if (!supported) {
        EmptyState("Update the host", "This server needs a codex-start version with launcher settings support.", Icons.Default.SystemUpdate)
        return
    }
    val (query, state) = rememberRemoteData(repo, server, "", "launcher/list")
    val (_, projects) = rememberRemoteData(repo, server, "", "project/list")
    val edits: LauncherSettingsState = viewModel(key = server)
    var name by remember(server) { mutableStateOf("") }
    val profiles = state.value?.optJSONArray("profiles")
    RefreshPage(repo, query, state) {
        LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(vertical = 16.dp)) {
            item {
                Text(
                    "Settings apply to new sessions. Open each profile from a project to keep several profiles active on this host.",
                    Modifier.padding(bottom = 16.dp),
                    style = MaterialTheme.typography.bodyMedium,
                )
                SettingsGroup("Global settings") {
                    SettingsRow(
                        "Edit global settings",
                        "Defaults and definitions for this host",
                        Icons.Default.Settings,
                        onClick = { edits.editor = LauncherSettingsDraft(obj("scope" to "global", "title" to "Global settings")) },
                    )
                }
                SettingsGroup("Profiles") {
                    if (profiles != null) {
                        for (index in 0 until profiles.length()) {
                            val profile = profiles.getString(index)
                            SettingsRow(
                                profile,
                                "Edit profile settings",
                                Icons.Default.Tune,
                                onClick = {
                                    edits.editor =
                                        LauncherSettingsDraft(
                                            obj("scope" to "profile", "profile" to profile, "title" to "Profile: $profile"),
                                        )
                                },
                            )
                        }
                    }
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedTextField(
                            name,
                            { name = it },
                            label = { Text("New profile name") },
                            singleLine = true,
                            modifier = Modifier.fillMaxWidth(),
                        )
                        Button(
                            onClick = {
                                edits.editor =
                                    LauncherSettingsDraft(
                                        obj(
                                            "scope" to "profile",
                                            "profile" to name.trim(),
                                            "title" to "Profile: ${name.trim()}",
                                        ),
                                    )
                            },
                            enabled =
                                name.isNotBlank() &&
                                    state.value != null &&
                                    (0 until (profiles?.length() ?: 0)).none { profiles?.getString(it) == name.trim() },
                        ) {
                            Text("Add profile")
                        }
                    }
                }
                SettingsGroup("Project settings") {
                    DataStatus(projects) { repo.data.refresh(RemoteQuery(server, "", "project/list"), true) }
                    val entries =
                        projects.value
                            ?.optJSONArray("data")
                            ?.objects()
                            .orEmpty()
                    if (entries.isEmpty() && projects.value != null) Text("Add a project to edit its settings.", Modifier.padding(16.dp))
                    entries.forEach { project ->
                        SettingsRow(
                            project.text("name"),
                            project.text("path"),
                            Icons.Default.Folder,
                            onClick = {
                                edits.editor =
                                    LauncherSettingsDraft(
                                        obj(
                                            "scope" to "project",
                                            "projectId" to project.text("id"),
                                            "title" to project.text("name"),
                                        ),
                                    )
                            },
                        )
                    }
                }
            }
        }
    }
    edits.editor?.let { draft ->
        key(server, draft) {
            LauncherSettingsEditor(repo, server, draft, {
                name = ""
                repo.data.refresh(query, true)
            }, { edits.editor = null })
        }
    }
}

@Composable private fun LauncherSettingsEditor(
    repo: RemoteRepository,
    server: String,
    draft: LauncherSettingsDraft,
    saved: () -> Unit,
    close: () -> Unit,
) {
    val target = draft.target
    val snapshot = draft.snapshot
    val text = draft.text
    var busy by remember { mutableStateOf(false) }
    var failure by remember { mutableStateOf<String?>(null) }
    var reload by remember { mutableIntStateOf(0) }
    var discard by remember { mutableStateOf(false) }
    var reloadAfterDiscard by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val dirty = snapshot != null && text != snapshot?.text("text")
    val dismiss = {
        if (!busy) {
            reloadAfterDiscard = false
            if (dirty) discard = true else close()
        }
    }
    LaunchedEffect(reload) {
        if (reload == 0 && draft.snapshot != null) return@LaunchedEffect
        busy = true
        failure = null
        try {
            val result = repo.request(server, "launcher/read", target) as JSONObject
            draft.snapshot = result
            draft.text = result.text("text")
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (error: Exception) {
            failure = error.message ?: "Cannot read settings"
        } finally {
            busy = false
        }
    }
    NativeSheet(target.text("title"), dismiss, scrollContent = true, canDismiss = {
        dismiss()
        false
    }) {
        BackHandler(onBack = dismiss)
        if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
        Text(
            "Edit the TOML settings below. The host checks the file before it saves. Existing sessions keep their settings.",
            style = MaterialTheme.typography.bodySmall,
        )
        snapshot?.let { Text(it.text("path"), style = MaterialTheme.typography.bodySmall, fontFamily = FontFamily.Monospace) }
        if (target.text("scope") == "profile") {
            Text(
                "Example: add environment = \"default\" under [settings]. To inherit a profile, add extends = \"name\" before [settings].",
                style = MaterialTheme.typography.bodySmall,
            )
        }
        OutlinedTextField(
            text,
            { draft.text = it },
            label = { Text("Settings (TOML)") },
            modifier = Modifier.fillMaxWidth(),
            minLines = 8,
            maxLines = 18,
            enabled = snapshot != null && !busy,
            textStyle = MaterialTheme.typography.bodyMedium.copy(fontFamily = FontFamily.Monospace),
        )
        failure?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(onClick = {
                reloadAfterDiscard = true
                if (dirty) discard = true else reload++
            }, enabled = !busy, modifier = Modifier.weight(1f)) { Text("Reload") }
            Button(onClick = {
                scope.launch {
                    busy = true
                    failure = null
                    try {
                        repo.request(
                            server,
                            "launcher/write",
                            JSONObject(target.toString()).put("text", text).put("version", snapshot!!.text("version")),
                        )
                        saved()
                        close()
                    } catch (cancelled: CancellationException) {
                        throw cancelled
                    } catch (error: Exception) {
                        failure = error.message ?: "Cannot save settings"
                    } finally {
                        busy = false
                    }
                }
            }, enabled = !busy && snapshot != null && (dirty || target.text("scope") == "profile"), modifier = Modifier.weight(1f)) {
                Text("Save settings")
            }
        }
    }
    if (discard) {
        AlertDialog(
            onDismissRequest = { discard = false },
            title = { Text("Discard edits?") },
            text = { Text("Your unsaved settings will be lost.") },
            confirmButton = {
                TextButton(onClick = {
                    discard = false
                    if (reloadAfterDiscard) reload++ else close()
                }) { Text("Discard") }
            },
            dismissButton = { TextButton(onClick = { discard = false }) { Text("Keep editing") } },
        )
    }
}
