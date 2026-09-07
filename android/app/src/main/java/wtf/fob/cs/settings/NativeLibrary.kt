package wtf.fob.cs.settings

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import java.util.UUID

@Composable fun SectionsScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    var tab by remember { mutableIntStateOf(0) }
    Column {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            listOf("Sections", "Projects", "Shared plugins").forEachIndexed {
                index,
                title,
                ->
                FilterChip(tab == index, { tab = index }, label = { Text(title) })
            }
        }
        when (tab) {
            0 -> ThreadSectionsScreen(repo, server, session)
            1 -> ProjectLibraryScreen(repo, server, session)
            else -> SharedPluginsScreen(repo, server, session)
        }
    }
}

@Composable fun ProjectLibraryScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    val (query, state) = rememberRemoteData(repo, server, session.text("id"), "project/list", obj("limit" to 100))
    val projects =
        state.value
            ?.optJSONArray("data")
            ?.objects()
            .orEmpty()
    var selected by remember { mutableStateOf<JSONObject?>(null) }
    val scope = rememberCoroutineScope()
    RefreshPage(repo, query, state) {
        LazyColumn(contentPadding = PaddingValues(vertical = 16.dp)) {
            item {
                FilledTonalButton(onClick = { selected = JSONObject() }) {
                    Icon(Icons.Default.Add, null)
                    Text("New project")
                }
            }
            if (projects.isEmpty() &&
                !state.loading
            ) {
                item {
                    EmptyState(
                        "Organize work into projects",
                        "Group folders and chats in your Codex library.",
                        Icons.Default.FolderOpen,
                    )
                }
            }
            items(projects, key = { it.text("id") }) { project ->
                SettingsGroup {
                    SettingsRow(
                        project.text("name"),
                        project
                            .optJSONArray("roots")
                            ?.objects()
                            ?.joinToString("\n") {
                                it.text("path")
                            }.orEmpty(),
                        Icons.Default.FolderOpen,
                        onClick = {
                            selected =
                                project
                        },
                    )
                    if (projects.indexOf(project) >
                        0
                    ) {
                        TextButton(onClick = {
                            scope.launch {
                                runCatching {
                                    repo.rpc(
                                        server,
                                        session.text("id"),
                                        "project/move",
                                        obj(
                                            "projectId" to project.text("id"),
                                            "beforeProjectId" to projects[projects.indexOf(project) - 1].text("id"),
                                        ),
                                    )
                                }.onSuccess { repo.data.refresh(query, true) }.onFailure(repo::report)
                            }
                        }) { Text("Move up") }
                    }
                }
            }
        }
    }
    selected?.let { project ->
        ProjectEditorSheet(repo, server, session, project, { selected = null }) {
            selected = null
            repo.data.refresh(query, true)
        }
    }
}

@Composable private fun ProjectEditorSheet(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
    project: JSONObject,
    close: () -> Unit,
    saved: () -> Unit,
) {
    var name by remember { mutableStateOf(project.text("name")) }
    var roots by remember {
        mutableStateOf(
            project.optJSONArray("roots")?.objects()?.joinToString("\n") { it.text("path") } ?: session.text("executionCwd"),
        )
    }
    var importing by remember { mutableStateOf(false) }
    var selectedChats by remember { mutableStateOf<Set<String>>(emptySet()) }
    var busy by remember { mutableStateOf(false) }
    var failure by remember { mutableStateOf("") }
    var delete by remember { mutableStateOf(false) }
    val key = remember { UUID.randomUUID().toString() }
    val scope = rememberCoroutineScope()
    val id = project.text("id")
    var current by remember { mutableStateOf(project) }
    LaunchedEffect(id) {
        if (id.isNotEmpty()) {
            runCatching {
                repo
                    .rpc(
                        server,
                        session.text("id"),
                        "project/read",
                        obj("projectId" to id),
                    ).getJSONObject("project")
            }.onSuccess {
                current =
                    it
                name = it.text("name")
                roots = it.getJSONArray("roots").objects().joinToString("\n") { root -> root.text("path") }
            }.onFailure(repo::report)
        }
    }

    fun submit(remove: Boolean) {
        scope.launch {
            busy = true
            failure = ""
            try {
                if (remove) {
                    repo.rpc(server, session.text("id"), "project/delete", obj("projectId" to id))
                } else {
                    val paths =
                        roots
                            .lineSequence()
                            .map { it.trim() }
                            .filter { it.isNotEmpty() }
                            .distinct()
                            .toList()
                    require(
                        name.isNotBlank() && paths.isNotEmpty() && paths.all { it.startsWith('/') },
                    ) { "Enter a name and absolute folder paths" }
                    val params =
                        obj(
                            "name" to name.trim(),
                            "roots" to JSONArray(paths.map { obj("path" to it) }),
                            "metadata" to (current.optJSONObject("metadata") ?: JSONObject()),
                        )
                    if (id.isNotEmpty()) params.put("projectId", id) else params.put("idempotencyKey", key)
                    if (importing) params.put("threads", JSONArray(selectedChats.toList()))
                    repo.rpc(
                        server,
                        session.text("id"),
                        if (id.isNotEmpty()) {
                            "project/update"
                        } else if (importing) {
                            "project/import"
                        } else {
                            "project/create"
                        },
                        params,
                    )
                }
                saved()
            } catch (
                error: Exception,
            ) {
                if (error is kotlinx.coroutines.CancellationException) throw error
                failure = error.message.orEmpty()
            } finally {
                busy = false
            }
        }
    }
    NativeSheet(if (id.isEmpty()) "New project" else "Edit project", close) {
        Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            OutlinedTextField(name, { name = it }, label = { Text("Project name") }, singleLine = true, modifier = Modifier.fillMaxWidth())
            OutlinedTextField(
                roots,
                { roots = it },
                label = { Text("Folder paths, one per line") },
                modifier = Modifier.fillMaxWidth(),
                maxLines = 5,
            )
            if (id.isEmpty()) {
                SettingsRow("Include existing chats", trailing = { Switch(importing, { importing = it }) })
                if (importing) {
                    val (_, chats) = rememberRemoteData(repo, server, session.text("id"), "thread/list", obj("limit" to 100))
                    chats.value?.optJSONArray("data")?.objects()?.forEach { chat ->
                        SettingsRow(chatTitle(chat), trailing = {
                            Checkbox(
                                chat.text("id") in selectedChats,
                                {
                                    selectedChats =
                                        if (chat.text("id") in
                                            selectedChats
                                        ) {
                                            selectedChats - chat.text("id")
                                        } else {
                                            selectedChats + chat.text("id")
                                        }
                                },
                            )
                        })
                    }
                }
            } else {
                TextButton(onClick = { delete = true }, enabled = !busy) { Text("Delete project") }
            }
            if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
        }
        Button(enabled = !busy && name.isNotBlank(), modifier = Modifier.fillMaxWidth().padding(top = 16.dp), onClick = {
            submit(false)
        }) { Text(if (busy) "Saving…" else "Save project") }
    }
    if (delete) {
        AlertDialog(onDismissRequest = {
            delete = false
        }, title = {
            Text("Delete project?")
        }, text = {
            Text("Remove this project from your Codex library.")
        }, confirmButton = { TextButton(enabled = !busy, onClick = { submit(true) }) { Text("Delete") } }, dismissButton = {
            TextButton(onClick = {
                delete =
                    false
            }) { Text("Cancel") }
        })
    }
}

@Composable fun SharedPluginsScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    val (query, state) = rememberRemoteData(repo, server, session.text("id"), "plugin/share/list")
    var selected by remember { mutableStateOf<JSONObject?>(null) }
    val scope = rememberCoroutineScope()
    RefreshPage(repo, query, state) {
        LazyColumn(contentPadding = PaddingValues(vertical = 16.dp)) {
            item {
                FilledTonalButton(onClick = { selected = JSONObject() }) {
                    Icon(Icons.Default.Add, null)
                    Text("Share a plugin")
                }
            }
            val plugins =
                state.value
                    ?.optJSONArray("data")
                    ?.objects()
                    .orEmpty()
            if (plugins.isEmpty() &&
                !state.loading
            ) {
                item { EmptyState("Share your tools", "Manage plugins that you share with other people.", Icons.Default.Extension) }
            }
            items(plugins) { item ->
                val plugin = item.getJSONObject("plugin")
                SettingsGroup {
                    SettingsRow(
                        plugin.text(
                            "name",
                        ),
                        plugin.optJSONObject("shareContext")?.text("creatorName").orEmpty(),
                        Icons.Default.Extension,
                        onClick = {
                            selected =
                                item
                        },
                    )
                    if (item.text("localPluginPath").isEmpty()) {
                        TextButton(onClick = {
                            scope.launch {
                                runCatching {
                                    repo.rpc(
                                        server,
                                        session.text("id"),
                                        "plugin/share/checkout",
                                        obj(
                                            "remotePluginId" to plugin.text("remotePluginId"),
                                        ),
                                    )
                                }.onSuccess { repo.data.refresh(query, true) }.onFailure(repo::report)
                            }
                        }) { Text("Get local copy") }
                    }
                }
            }
        }
    }
    selected?.let { item ->
        SharePluginSheet(repo, server, session.text("id"), item, { selected = null }) {
            selected = null
            repo.data.refresh(query, true)
        }
    }
}

@Composable private fun SharePluginSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    item: JSONObject,
    close: () -> Unit,
    saved: () -> Unit,
) {
    val plugin = item.optJSONObject("plugin")
    val id = plugin?.text("remotePluginId").orEmpty()
    var path by remember { mutableStateOf(item.text("localPluginPath")) }
    var visibility by remember { mutableStateOf(plugin?.optJSONObject("shareContext")?.text("discoverability", "PRIVATE") ?: "PRIVATE") }
    var recipient by remember { mutableStateOf("") }
    var recipients by remember {
        mutableStateOf(
            plugin
                ?.optJSONObject("shareContext")
                ?.optJSONArray("sharePrincipals")
                ?.objects()
                .orEmpty(),
        )
    }
    var kind by remember { mutableIntStateOf(0) }
    var editor by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    var delete by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    fun submit(remove: Boolean) {
        scope.launch {
            busy = true
            runCatching {
                val targets =
                    recipients
                        .filter { it.text("role") != "owner" }
                        .map {
                            obj(
                                "principalId" to it.text("principalId"),
                                "principalType" to it.text("principalType"),
                                "role" to it.text("role"),
                            )
                        }.toMutableList()
                if (recipient.isNotBlank()) {
                    targets.removeAll {
                        it.text("principalId") == recipient.trim() &&
                            it.text("principalType") == listOf("user", "group", "workspace")[kind]
                    }
                    targets.add(
                        obj(
                            "principalId" to recipient.trim(),
                            "principalType" to listOf("user", "group", "workspace")[kind],
                            "role" to if (editor) "editor" else "reader",
                        ),
                    )
                }
                val params =
                    if (remove) {
                        obj("remotePluginId" to id)
                    } else {
                        obj(
                            "discoverability" to visibility,
                            "shareTargets" to JSONArray(targets),
                        )
                    }
                if (!remove) {
                    if (id.isEmpty()) params.put("pluginPath", path.trim()) else params.put("remotePluginId", id)
                }
                repo.rpc(
                    server,
                    session,
                    if (remove) {
                        "plugin/share/delete"
                    } else if (id.isEmpty()) {
                        "plugin/share/save"
                    } else {
                        "plugin/share/updateTargets"
                    },
                    params,
                )
            }.onSuccess { saved() }.onFailure(repo::report)
            busy = false
        }
    }
    NativeSheet(plugin?.text("name") ?: "Share a plugin", close) {
        Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            if (id.isEmpty()) {
                OutlinedTextField(
                    path,
                    { path = it },
                    label = { Text("Plugin folder on the host") },
                    modifier = Modifier.fillMaxWidth(),
                )
            }
            val visibilityOptions = listOf("PRIVATE", "UNLISTED", "LISTED")
            ChoiceMenu(
                "Visibility",
                listOf("Private", "Unlisted", "Listed in workspace"),
                visibilityOptions.indexOf(visibility).coerceAtLeast(0),
            ) {
                visibility =
                    visibilityOptions[it]
            }
            if (recipients.isNotEmpty()) {
                SettingsGroup("People with access") {
                    recipients.forEach { person ->
                        SettingsRow(person.text("name", person.text("principalId")), humanize(person.text("role")), trailing = {
                            if (person.text("role") !=
                                "owner"
                            ) {
                                IconButton(onClick = {
                                    recipients = recipients - person
                                }) { Icon(Icons.Default.Close, "Remove ${person.text("name", person.text("principalId"))}") }
                            }
                        })
                    }
                }
            }
            ChoiceMenu("Share with", listOf("Person", "Group", "Workspace"), kind) { kind = it }
            OutlinedTextField(
                recipient,
                { recipient = it },
                label = { Text("Recipient ID (optional)") },
                modifier = Modifier.fillMaxWidth(),
            )
            SettingsRow("Allow editing", trailing = { Switch(editor, { editor = it }) })
            if (id.isNotEmpty()) TextButton(onClick = { delete = true }) { Text("Stop sharing") }
        }
        Button(
            enabled =
                !busy &&
                    (
                        id.isNotEmpty() ||
                            path.startsWith(
                                '/',
                            )
                    ),
            modifier = Modifier.fillMaxWidth().padding(top = 16.dp),
            onClick = {
                submit(false)
            },
        ) { Text("Save sharing settings") }
    }
    if (delete) {
        AlertDialog(onDismissRequest = {
            delete = false
        }, title = {
            Text("Stop sharing this plugin?")
        }, text = {
            Text("Other people will lose access to the shared plugin.")
        }, confirmButton = { TextButton(enabled = !busy, onClick = { submit(true) }) { Text("Stop sharing") } }, dismissButton = {
            TextButton(onClick = {
                delete =
                    false
            }) { Text("Cancel") }
        })
    }
}
