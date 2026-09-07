package wtf.fob.cs.settings

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ShortText
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

@Composable fun PreferencesScreen(
    page: FeaturePage,
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
    thread: String,
) {
    val sid = session.text("id")
    val method =
        when (page) {
            FeaturePage.Features -> "experimentalFeature/list"
            FeaturePage.Permissions -> "permissionProfile/list"
            else -> "config/read"
        }
    val (query, state) =
        rememberRemoteData(
            repo,
            server,
            sid,
            method,
            if (method ==
                "config/read"
            ) {
                obj("cwd" to session.text("executionCwd"), "includeLayers" to true)
            } else {
                JSONObject()
            },
        )
    val config = state.value?.optJSONObject("config") ?: JSONObject()
    val (requirementsQuery, requirements) =
        rememberRemoteData(
            repo,
            server,
            sid,
            "configRequirements/read",
            enabled =
                page == FeaturePage.Permissions,
        )
    val scope = rememberCoroutineScope()
    var busy by remember { mutableStateOf(false) }
    var confirm by remember { mutableStateOf(false) }
    var editing by remember { mutableStateOf<Pair<String, Any>?>(null) }

    fun run(
        method: String,
        params: JSONObject,
    ) {
        scope.launch {
            busy = true
            runCatching { repo.rpc(server, sid, method, params) }
                .onSuccess {
                    repo.data.invalidate(server)
                    repo.data.refresh(query, true)
                    editing =
                        null
                }.onFailure(repo::report)
            busy = false
        }
    }

    fun write(
        key: String,
        value: Any,
    ) = run("config/value/write", obj("keyPath" to key, "value" to value, "mergeStrategy" to "replace"))
    RefreshPage(repo, query, state) {
        LazyColumn(contentPadding = PaddingValues(vertical = 16.dp)) {
            when (page) {
                FeaturePage.Personalization ->
                    item {
                        SettingsGroup("Response style") {
                            listOf("friendly" to "Friendly", "pragmatic" to "Practical", "none" to "Neutral").forEach { (value, title) ->
                                SettingsRow(title, trailing = {
                                    RadioButton(
                                        config.text("personality") == value,
                                        enabled =
                                            !busy && state.value != null,
                                        onClick = { write("personality", value) },
                                    )
                                }, onClick = {
                                    if (!busy &&
                                        state.value != null
                                    ) {
                                        write("personality", value)
                                    }
                                })
                            }
                        }
                        SettingsGroup("Instructions") {
                            SettingsRow(
                                "Custom instructions",
                                config.text("developer_instructions").ifEmpty { "Add preferences for future tasks" },
                                Icons.Default.EditNote,
                                enabled =
                                    state.value != null,
                                onClick = { editing = "developer_instructions" to config.text("developer_instructions") },
                            )
                            SettingsRow(
                                "Response detail",
                                config.text("model_verbosity", "Host default"),
                                Icons.AutoMirrored.Filled.ShortText,
                                enabled =
                                    state.value != null,
                                onClick = { editing = "model_verbosity" to config.text("model_verbosity", "medium") },
                            )
                        }
                    }
                FeaturePage.Permissions ->
                    item {
                        SettingsGroup("Approval requests") {
                            Text(
                                "Choose who reviews actions that require approval.",
                                Modifier.padding(16.dp),
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                            SettingsRow(
                                "Ask me",
                                "You review each approval request.",
                                enabled = !busy,
                                onClick = { write("approvals_reviewer", "user") },
                            )
                            SettingsRow("Allow low-risk actions", "Automatic review checks each request.", enabled = !busy, onClick = {
                                write("approvals_reviewer", "auto_review")
                            })
                        }
                        SettingsGroup("Permission profiles") {
                            Text(
                                if (thread.isEmpty()) "Open a chat to set its permissions." else "Apply a profile to this chat.",
                                Modifier.padding(16.dp),
                                style = MaterialTheme.typography.bodySmall,
                            )
                            state.value?.optJSONArray("data")?.objects()?.forEach { profile ->
                                SettingsRow(
                                    humanize(profile.text("id")),
                                    profile.text("description"),
                                    Icons.Default.Shield,
                                    enabled = profile.optBoolean("allowed") && thread.isNotEmpty() && !busy,
                                    onClick = {
                                        run(
                                            "thread/settings/update",
                                            obj("threadId" to thread, "permissions" to profile.text("id")),
                                        )
                                    },
                                )
                            }
                        }
                        DataStatus(requirements) { repo.data.refresh(requirementsQuery, true) }
                        requirements.value?.optJSONObject("requirements")?.let { rules ->
                            SettingsGroup("Managed settings") {
                                Text(
                                    "These requirements are set by the workspace administrator.",
                                    Modifier.padding(16.dp),
                                    style = MaterialTheme.typography.bodySmall,
                                )
                                ValueView(rules)
                            }
                        }
                    }
                FeaturePage.Memory ->
                    item {
                        SettingsGroup("Learning from work") {
                            SettingsRow("Generate memories", "Use completed tasks to improve future work.", trailing = {
                                Switch(
                                    config.optJSONObject("memories")?.optBoolean("generate_memories", true) ?: true,
                                    enabled =
                                        !busy && state.value != null,
                                    onCheckedChange = { write("memories.generate_memories", it) },
                                )
                            })
                            SettingsRow("Use saved memories", "Include relevant memories in new tasks.", trailing = {
                                Switch(
                                    config.optJSONObject("memories")?.optBoolean("use_memories", true) ?: true,
                                    enabled =
                                        !busy && state.value != null,
                                    onCheckedChange = { write("memories.use_memories", it) },
                                )
                            })
                        }
                        if (thread.isNotEmpty()) {
                            SettingsGroup("This chat") {
                                SettingsRow(
                                    "Exclude chat from memory",
                                    "Stop this chat from contributing to memory.",
                                    enabled = !busy,
                                    onClick = {
                                        run(
                                            "thread/memoryMode/set",
                                            obj(
                                                "threadId" to thread,
                                                "mode" to "disabled",
                                            ),
                                        )
                                    },
                                )
                                SettingsRow("Include chat in memory", enabled = !busy, onClick = {
                                    run(
                                        "thread/memoryMode/set",
                                        obj(
                                            "threadId" to thread,
                                            "mode" to "enabled",
                                        ),
                                    )
                                })
                            }
                        }
                        SettingsGroup {
                            SettingsRow(
                                "Clear all memories",
                                "Permanently remove stored memories on this computer.",
                                Icons.Default.DeleteOutline,
                                onClick = { confirm = true },
                            )
                        }
                    }
                FeaturePage.Features -> {
                    val features =
                        state.value?.optJSONArray("data")?.objects().orEmpty().filter {
                            it.text("stage") !in
                                setOf("removed", "deprecated")
                        }
                    items(features, key = { it.text("name") }) { feature ->
                        SettingsGroup {
                            SettingsRow(
                                feature.text(
                                    "displayName",
                                    humanize(feature.text("name")),
                                ),
                                feature.text("description"),
                                trailing = {
                                    Switch(feature.optBoolean("enabled"), enabled = !busy, onCheckedChange = {
                                        run(
                                            "experimentalFeature/enablement/set",
                                            obj(
                                                "enablement" to obj(feature.text("name") to it),
                                            ),
                                        )
                                    })
                                },
                            )
                        }
                    }
                }
                FeaturePage.Configuration ->
                    item {
                        Text(
                            "Settings for this computer and project",
                            Modifier.padding(bottom = 20.dp),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        ConfigurationRows(config, "") { key, value -> editing = key to value }
                    }
                else -> Unit
            }
        }
    }
    editing?.let { (key, original) -> ConfigValueSheet(key, original, busy, { editing = null }) { value -> write(key, value) } }
    if (confirm) {
        AlertDialog(onDismissRequest = {
            confirm = false
        }, title = {
            Text("Clear all memories?")
        }, text = { Text("This deletes the stored memories on this computer. This cannot be undone.") }, confirmButton = {
            TextButton(onClick = {
                run("memory/reset", JSONObject())
                confirm =
                    false
            }) { Text("Clear memories") }
        }, dismissButton = { TextButton(onClick = { confirm = false }) { Text("Cancel") } })
    }
}

@Composable private fun ConfigurationRows(
    value: JSONObject,
    prefix: String,
    edit: (String, Any) -> Unit,
) {
    value.keys().asSequence().sorted().forEach { key ->
        val item = value.opt(key)
        val path = if (prefix.isEmpty()) key else "$prefix.$key"
        if (item is JSONObject) {
            var expanded by remember(path) { mutableStateOf(false) }
            SettingsGroup {
                SettingsRow(humanize(key), if (expanded) "Hide settings" else "Show settings", Icons.Default.Tune, onClick = {
                    expanded =
                        !expanded
                })
                if (expanded) ConfigurationRows(item, path, edit)
            }
        } else if (item !is JSONArray && item != null && item != JSONObject.NULL) {
            SettingsRow(
                humanize(
                    key,
                ),
                if (Regex(
                        "token|secret|password|api_key",
                        RegexOption.IGNORE_CASE,
                    ).containsMatchIn(key)
                ) {
                    "••••••"
                } else {
                    item.toString().take(160)
                },
                onClick = {
                    edit(path, item)
                },
            )
        }
    }
}

@Composable private fun ConfigValueSheet(
    key: String,
    original: Any,
    busy: Boolean,
    close: () -> Unit,
    save: (Any) -> Unit,
) {
    var value by remember { mutableStateOf(original.toString()) }
    var failure by remember { mutableStateOf("") }
    val choices =
        when (key) {
            "model_verbosity" -> listOf("low", "medium", "high")
            "personality" -> listOf("friendly", "pragmatic", "none")
            "approval_policy" -> listOf("untrusted", "on-request", "never")
            "sandbox_mode" -> listOf("read-only", "workspace-write", "danger-full-access")
            else -> emptyList()
        }
    NativeSheet(humanize(key.substringAfterLast('.')), close, scrollContent = true) {
        if (original is Boolean) {
            SettingsRow("Enabled", trailing = { Switch(value.toBoolean(), { value = it.toString() }) })
        } else if (choices.isNotEmpty()) {
            SettingsGroup {
                choices.forEach { choice ->
                    SettingsRow(humanize(choice), trailing = {
                        RadioButton(
                            value == choice,
                            { value = choice },
                        )
                    }, onClick = { value = choice })
                }
            }
        } else {
            OutlinedTextField(
                value,
                { value = it },
                label = { Text(humanize(key.substringAfterLast('.'))) },
                modifier = Modifier.fillMaxWidth(),
                maxLines = 8,
                visualTransformation =
                    if (Regex(
                            "token|secret|password|api_key",
                            RegexOption.IGNORE_CASE,
                        ).containsMatchIn(
                            key,
                        )
                    ) {
                        androidx.compose.ui.text.input
                            .PasswordVisualTransformation()
                    } else {
                        androidx.compose.ui.text.input.VisualTransformation.None
                    },
            )
        }
        if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
        Button(enabled = !busy, onClick = {
            runCatching {
                save(
                    when (original) {
                        is Boolean -> value.toBooleanStrict()
                        is Int, is Long ->
                            value.toLongOrNull()
                                ?: error("Enter a whole number")
                        is Number -> value.toDoubleOrNull()?.takeIf { it.isFinite() } ?: error("Enter a number")
                        else -> value
                    },
                )
            }.onFailure {
                failure =
                    it.message.orEmpty()
            }
        }, modifier = Modifier.fillMaxWidth().padding(top = 20.dp)) { Text(if (busy) "Saving…" else "Save") }
    }
}

@Composable fun ThreadSectionsScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    val (query, state) = rememberRemoteData(repo, server, session.text("id"), "threadSection/list")
    var edit by remember { mutableStateOf<JSONObject?>(null) }
    var name by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var delete by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    fun save(remove: Boolean) {
        scope.launch {
            busy = true
            val id = edit?.text("id").orEmpty()
            val method =
                if (remove) {
                    "threadSection/delete"
                } else if (id.isEmpty()) {
                    "threadSection/create"
                } else {
                    "threadSection/update"
                }
            val params =
                if (remove) {
                    obj(
                        "sectionId" to id,
                    )
                } else {
                    obj("name" to name.trim()).apply { if (id.isNotEmpty()) put("sectionId", id) }
                }
            runCatching { repo.rpc(server, session.text("id"), method, params) }
                .onSuccess {
                    edit = null
                    delete = false
                    repo.data.refresh(query, true)
                }.onFailure(repo::report)
            busy = false
        }
    }
    RefreshPage(repo, query, state) {
        LazyColumn(contentPadding = PaddingValues(vertical = 16.dp)) {
            item {
                FilledTonalButton(onClick = {
                    edit = JSONObject()
                    name = ""
                }) {
                    Icon(Icons.Default.Add, null)
                    Text("New section")
                }
            }
            val sections =
                state.value
                    ?.optJSONArray("data")
                    ?.objects()
                    .orEmpty()
            if (sections.isEmpty() &&
                !state.loading
            ) {
                item {
                    EmptyState(
                        "Organize your chats",
                        "Create sections, then move chats from their action menu.",
                        Icons.Default.FolderSpecial,
                    )
                }
            }
            items(sections, key = { it.text("id") }) { section ->
                SettingsRow(section.text("name"), icon = Icons.Default.FolderSpecial, onClick = {
                    edit =
                        section
                    name = section.text("name")
                })
            }
        }
    }
    edit?.let { section ->
        NativeSheet(if (section.has("id")) "Edit section" else "New section", { edit = null }, scrollContent = true) {
            OutlinedTextField(name, { name = it }, label = { Text("Section name") }, modifier = Modifier.fillMaxWidth())
            Button(onClick = {
                save(false)
            }, enabled = !busy && name.isNotBlank(), modifier = Modifier.fillMaxWidth().padding(top = 16.dp)) { Text("Save") }
            if (section.has("id")) TextButton(onClick = { delete = true }, enabled = !busy) { Text("Delete section") }
        }
    }
    if (delete) {
        AlertDialog(onDismissRequest = {
            delete = false
        }, title = {
            Text("Delete section?")
        }, text = {
            Text("The chats in this section remain available.")
        }, confirmButton = { TextButton(onClick = { save(true) }, enabled = !busy) { Text("Delete") } }, dismissButton = {
            TextButton(onClick = {
                delete =
                    false
            }) { Text("Cancel") }
        })
    }
}

@Composable fun DiagnosticsScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
    thread: String,
) {
    val (query, state) = rememberRemoteData(repo, server, session.text("id"), "server/diagnostics")
    var feedback by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf("") }
    var logs by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    RefreshPage(repo, query, state) {
        LazyColumn(contentPadding = PaddingValues(vertical = 16.dp)) {
            item {
                SettingsGroup {
                    SettingsRow("Report a problem", "Send feedback and optional logs", Icons.Default.BugReport, onClick = {
                        feedback =
                            true
                    })
                }
            }
            item { SettingsGroup("Diagnostics") { state.value?.let { ValueView(it) } } }
        }
    }
    if (feedback) {
        NativeSheet("Report a problem", { feedback = false }, scrollContent = true) {
            OutlinedTextField(
                message,
                { message = it },
                label = { Text("What went wrong?") },
                minLines = 4,
                modifier = Modifier.fillMaxWidth(),
            )
            SettingsRow("Include logs", "Logs can contain details from your workspace.", trailing = { Switch(logs, { logs = it }) })
            Button(enabled = message.isNotBlank() && !busy, modifier = Modifier.fillMaxWidth(), onClick = {
                scope.launch {
                    busy = true
                    runCatching {
                        repo.rpc(
                            server,
                            session.text("id"),
                            "feedback/upload",
                            obj(
                                "classification" to "bug",
                                "reason" to message,
                                "includeLogs" to logs,
                            ).apply { if (thread.isNotEmpty()) put("threadId", thread) },
                        )
                    }.onSuccess { feedback = false }.onFailure(repo::report)
                    busy = false
                }
            }) { Text("Send feedback") }
        }
    }
}

@Composable fun ImportSettingsScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    val (query, state) =
        rememberRemoteData(
            repo,
            server,
            session.text("id"),
            "externalAgentConfig/detect",
            obj(
                "cwds" to JSONArray().put(session.text("executionCwd")),
                "includeHome" to true,
            ),
        )
    var selected by remember { mutableStateOf<Set<Int>>(emptySet()) }
    val entries =
        state.value?.optJSONArray("items")?.objects() ?: state.value
            ?.optJSONArray("migrationItems")
            ?.objects()
            .orEmpty()
    val scope = rememberCoroutineScope()
    var busy by remember { mutableStateOf(false) }
    var history by remember { mutableStateOf(false) }
    RefreshPage(repo, query, state) {
        LazyColumn(contentPadding = PaddingValues(vertical = 16.dp)) {
            item { Text("Select the settings to import from other coding tools.", Modifier.padding(bottom = 20.dp)) }
            item { TextButton(onClick = { history = true }) { Text("Import history") } }
            if (entries.isEmpty() &&
                !state.loading
            ) {
                item {
                    EmptyState(
                        "No settings found",
                        "Compatible settings from other tools will appear here.",
                        Icons.Default.FileDownload,
                    )
                }
            }
            items(entries.size) { index ->
                val entry = entries[index]
                SettingsRow(
                    entry.text(
                        "name",
                        humanize(entry.text("itemType")),
                    ),
                    entry.text("path", entry.text("description")),
                    trailing = {
                        Checkbox(
                            index in selected,
                            { selected = if (index in selected) selected - index else selected + index },
                        )
                    },
                )
            }
            item {
                Button(enabled = selected.isNotEmpty() && !busy, modifier = Modifier.fillMaxWidth(), onClick = {
                    scope.launch {
                        busy = true
                        runCatching {
                            repo.rpc(
                                server,
                                session.text("id"),
                                "externalAgentConfig/import",
                                obj(
                                    "migrationItems" to JSONArray(selected.sorted().map { entries[it] }),
                                ),
                            )
                        }.onSuccess {
                            selected = emptySet()
                            repo.data.refresh(query, true)
                        }.onFailure(repo::report)
                        busy = false
                    }
                }) { Text("Import selected settings") }
            }
        }
    }
    if (history) ImportHistorySheet(repo, server, session.text("id")) { history = false }
}

@Composable private fun ImportHistorySheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    close: () -> Unit,
) {
    val (query, state) = rememberRemoteData(repo, server, session, "externalAgentConfig/import/readHistories")
    NativeSheet("Import history", close) {
        DataStatus(state) { repo.data.refresh(query, true) }
        LazyColumn(Modifier.weight(1f, fill = false)) {
            val entries =
                state.value
                    ?.optJSONArray("data")
                    ?.objects()
                    .orEmpty()
            if (entries.isEmpty() &&
                !state.loading
            ) {
                item { EmptyState("No imports yet", "Completed imports will appear here.", Icons.Default.History) }
            }
            items(entries) { entry ->
                SettingsGroup {
                    SettingsRow(entry.text("providerId", "Import"), humanize(entry.text("status")), Icons.Default.History)
                    ValueView(entry)
                }
            }
        }
    }
}
