package wtf.fob.cs.settings

import android.content.Intent
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Login
import androidx.compose.material.icons.automirrored.filled.Logout
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.core.net.toUri
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import java.util.UUID

@Composable fun FeatureScreen(
    page: FeaturePage,
    repo: RemoteRepository,
    server: String,
    session: JSONObject?,
    thread: String,
    chooseProject: () -> Unit,
) {
    if (page == FeaturePage.Launcher) {
        LauncherSettingsScreen(repo, server)
        return
    }
    if (session == null) {
        EmptyState("Choose a project", "Open a project on this computer to manage ${page.title.lowercase()}.", page.icon) {
            Button(onClick = chooseProject) { Text("Choose project") }
        }
        return
    }
    when (page) {
        FeaturePage.Launcher -> Unit
        FeaturePage.Plugins -> PluginsScreen(repo, server, session)
        FeaturePage.Skills -> SkillsScreen(repo, server, session)
        FeaturePage.Connections -> ConnectionsScreen(repo, server, session, thread)
        FeaturePage.Account -> AccountScreen(repo, server, session)
        FeaturePage.Personalization, FeaturePage.Permissions, FeaturePage.Memory, FeaturePage.Features, FeaturePage.Configuration ->
            PreferencesScreen(
                page,
                repo,
                server,
                session,
                thread,
            )
        FeaturePage.Organization -> SectionsScreen(repo, server, session)
        FeaturePage.Diagnostics -> DiagnosticsScreen(repo, server, session, thread)
        FeaturePage.Imports -> ImportSettingsScreen(repo, server, session)
    }
}

data class PluginEntry(
    val summary: JSONObject,
    val marketplace: String,
    val path: String,
) {
    val id get() = summary.text("id")
    val title get() = summary.optJSONObject("interface")?.text("displayName")?.takeIf { it.isNotEmpty() } ?: summary.text("name")

    fun parameters() =
        obj("pluginName" to summary.text("name")).apply {
            if (path.isEmpty()) put("remoteMarketplaceName", marketplace) else put("marketplacePath", path)
        }
}

fun pluginEntries(response: JSONObject?): List<PluginEntry> =
    response
        ?.optJSONArray("marketplaces")
        ?.objects()
        ?.flatMap { marketplace ->
            marketplace
                .optJSONArray("plugins")
                ?.objects()
                .orEmpty()
                .map { PluginEntry(it, marketplace.text("name"), marketplace.text("path")) }
        }.orEmpty()
        .distinctBy { it.id }

@Composable fun PluginsScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    var installed by remember { mutableStateOf(true) }
    var search by remember { mutableStateOf("") }
    var selected by remember { mutableStateOf<PluginEntry?>(null) }
    var marketplaces by remember { mutableStateOf(false) }
    var syncing by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val (query, state) =
        rememberRemoteData(
            repo,
            server,
            session.text("id"),
            if (installed) "plugin/installed" else "plugin/list",
            obj(
                "cwds" to JSONArray().put(session.text("executionCwd")),
            ),
        )
    val plugins =
        pluginEntries(state.value).filter {
            it.title.contains(search, true) ||
                it.summary
                    .optJSONObject("interface")
                    ?.text("shortDescription")
                    ?.contains(search, true) == true
        }
    RefreshPage(repo, query, state) {
        SearchField(search, { search = it }, "Search plugins", Modifier.padding(vertical = 12.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(installed, { installed = true }, label = { Text("Added") })
            FilterChip(!installed, { installed = false }, label = { Text("Discover") })
            Spacer(Modifier.weight(1f))
            IconButton(enabled = !syncing, onClick = {
                scope.launch {
                    syncing = true
                    try {
                        val result =
                            repo.rpc(
                                server,
                                session.text("id"),
                                "plugin/reconcile",
                                obj("reason" to "Refresh plugins from Android"),
                            )
                        repo.data.invalidate(server) { it.method.startsWith("plugin/") }
                        repo.data.refresh(query, true)
                        require(
                            (result.optJSONArray("failedRemotePluginIds")?.length() ?: 0) == 0,
                        ) { "Some plugins could not be updated. Refresh to try again." }
                    } catch (error: Exception) {
                        repo.report(error)
                    } finally {
                        syncing = false
                    }
                }
            }) { Icon(Icons.Default.Sync, "Update installed plugins") }
            IconButton(onClick = { marketplaces = true }) { Icon(Icons.Default.Storefront, "Manage plugin sources") }
        }
        LazyColumn(contentPadding = PaddingValues(bottom = 24.dp)) {
            if (plugins.isEmpty() &&
                !state.loading
            ) {
                item {
                    EmptyState(
                        if (search.isNotBlank()) {
                            "No matching plugins"
                        } else if (installed) {
                            "Add more to your workspace"
                        } else {
                            "No plugins available"
                        },
                        "Browse plugins to connect tools and add skills.",
                        Icons.Default.Extension,
                    ) {
                        if (installed) {
                            FilledTonalButton(onClick = {
                                installed =
                                    false
                            }) { Text("Discover plugins") }
                        }
                    }
                }
            }
            items(plugins, key = { it.id }) { plugin ->
                SettingsGroup {
                    SettingsRow(
                        plugin.title,
                        plugin.summary
                            .optJSONObject(
                                "interface",
                            )?.text("shortDescription")
                            .orEmpty(),
                        Icons.Default.Extension,
                        onClick = {
                            selected =
                                plugin
                        },
                    )
                }
            }
            state.value?.optJSONArray("marketplaceLoadErrors")?.objects()?.forEach { error ->
                item { Text(error.text("message"), color = MaterialTheme.colorScheme.error) }
            }
        }
    }
    selected?.let { entry -> PluginDetailSheet(repo, server, session.text("id"), entry) { selected = null } }
    if (marketplaces) {
        MarketplaceSheet(
            repo,
            server,
            session.text("id"),
            state.value
                ?.optJSONArray("marketplaces")
                ?.objects()
                .orEmpty(),
        ) {
            marketplaces =
                false
        }
    }
}

@Composable private fun PluginDetailSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    entry: PluginEntry,
    close: () -> Unit,
) {
    val (query, state) = rememberRemoteData(repo, server, session, "plugin/read", entry.parameters())
    val plugin = state.value?.optJSONObject("plugin")
    val summary = plugin?.optJSONObject("summary") ?: entry.summary
    var skillContents by remember { mutableStateOf<JSONObject?>(null) }
    var busy by remember { mutableStateOf(false) }
    var failure by remember { mutableStateOf("") }
    val scope = rememberCoroutineScope()
    NativeSheet(entry.title, close) {
        Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            DataStatus(state) { repo.data.refresh(query, true) }
            Text(
                summary
                    .optJSONObject(
                        "interface",
                    )?.text("longDescription", summary.optJSONObject("interface")?.text("shortDescription").orEmpty())
                    .orEmpty(),
            )
            if (plugin != null) {
                SettingsGroup("Included") {
                    plugin.optJSONArray("skills")?.objects()?.forEach { skill ->
                        SettingsRow(skill.text("name"), skill.text("description"), Icons.Default.AutoAwesome, onClick = {
                            skillContents =
                                skill
                        })
                    }
                    plugin.optJSONArray("mcpServers")?.let { servers ->
                        for (i in 0 until servers.length()) SettingsRow(servers.getString(i), "Tool connection", Icons.Default.Link)
                    }
                }
            }
            if (summary
                    .text(
                        "disabledReason",
                    ).isNotEmpty()
            ) {
                Text(humanize(summary.text("disabledReason")), color = MaterialTheme.colorScheme.error)
            }
            if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
        }
        val installed = summary.optBoolean("installed")
        Button(
            enabled =
                !busy &&
                    (
                        installed ||
                            summary.text(
                                "availability",
                                "AVAILABLE",
                            ) == "AVAILABLE"
                    ),
            modifier = Modifier.fillMaxWidth(),
            onClick = {
                scope.launch {
                    busy = true
                    failure = ""
                    try {
                        repo.rpc(
                            server,
                            session,
                            if (installed) "plugin/uninstall" else "plugin/install",
                            if (installed) {
                                obj(
                                    "pluginId" to entry.id,
                                )
                            } else {
                                entry.parameters().put("installAttemptId", UUID.randomUUID().toString())
                            },
                        )
                        close()
                    } catch (error: Exception) {
                        if (error is kotlinx.coroutines.CancellationException) throw error
                        failure =
                            error.message.orEmpty()
                    } finally {
                        busy = false
                    }
                }
            },
        ) {
            Text(
                if (busy) {
                    "Updating…"
                } else if (installed) {
                    "Remove plugin"
                } else {
                    "Add plugin"
                },
            )
        }
    }
    skillContents?.let { skill ->
        SkillContentsSheet(
            repo,
            server,
            session,
            skill,
            marketplace = if (entry.path.isEmpty()) entry.marketplace else "",
            plugin = summary.text("remotePluginId"),
        ) {
            skillContents =
                null
        }
    }
}

@Composable private fun MarketplaceSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    sources: List<JSONObject>,
    close: () -> Unit,
) {
    var source by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    fun run(
        method: String,
        params: JSONObject,
    ) {
        scope.launch {
            busy = true
            runCatching { repo.rpc(server, session, method, params) }.onSuccess { close() }.onFailure(repo::report)
            busy =
                false
        }
    }
    NativeSheet("Plugin sources", close) {
        Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState())) {
            sources.forEach { marketplace ->
                SettingsRow(marketplace.text("name"), trailing = {
                    IconButton(enabled = !busy, onClick = {
                        run(
                            "marketplace/remove",
                            obj(
                                "marketplaceName" to marketplace.text("name"),
                            ),
                        )
                    }) { Icon(Icons.Default.RemoveCircleOutline, "Remove plugin source") }
                })
            }
            OutlinedTextField(source, { source = it }, label = { Text("Repository URL or host path") }, modifier = Modifier.fillMaxWidth())
            Button(
                enabled = !busy && source.isNotBlank(),
                onClick = { run("marketplace/add", obj("source" to source.trim())) },
            ) { Text("Add source") }
            TextButton(enabled = !busy, onClick = { run("marketplace/upgrade", JSONObject()) }) { Text("Update all sources") }
        }
    }
}

@Composable fun SkillsScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    var hooks by remember { mutableStateOf(false) }
    var search by remember { mutableStateOf("") }
    val (query, state) =
        rememberRemoteData(
            repo,
            server,
            session.text("id"),
            if (hooks) "hooks/list" else "skills/list",
            obj(
                "cwds" to JSONArray().put(session.text("executionCwd")),
            ),
        )
    val groups =
        state.value
            ?.optJSONArray("data")
            ?.objects()
            .orEmpty()
    val entries =
        groups
            .flatMap {
                it.optJSONArray(if (hooks) "hooks" else "skills")?.objects().orEmpty()
            }.filter { it.text(if (hooks) "eventName" else "name").contains(search, true) }
    val scope = rememberCoroutineScope()
    var pending by remember { mutableStateOf("") }
    var folders by remember { mutableStateOf(false) }
    var selected by remember { mutableStateOf<JSONObject?>(null) }
    RefreshPage(repo, query, state) {
        SearchField(search, { search = it }, "Search skills and hooks", Modifier.padding(vertical = 12.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(!hooks, { hooks = false }, label = { Text("Skills") })
            FilterChip(hooks, {
                hooks =
                    true
            }, label = { Text("Hooks") })
            Spacer(Modifier.weight(1f))
            IconButton(onClick = { folders = true }) { Icon(Icons.Default.FolderOpen, "Additional skill folders") }
        }
        LazyColumn {
            if (entries.isEmpty() &&
                !state.loading
            ) {
                item { EmptyState("Nothing here yet", "Add a plugin or a workspace skill to get started.", Icons.Default.AutoAwesome) }
            }
            items(entries) { item ->
                SettingsGroup {
                    if (hooks) {
                        SettingsRow(
                            humanize(
                                item.text("eventName"),
                            ),
                            item.text("command", item.text("statusMessage", item.text("sourcePath"))),
                            Icons.Default.Bolt,
                            trailing = {
                                Text(
                                    if (item.optBoolean("enabled")) "Enabled" else "Disabled",
                                    style = MaterialTheme.typography.labelMedium,
                                )
                            },
                        )
                    } else {
                        SettingsRow(
                            item
                                .optJSONObject(
                                    "interface",
                                )?.text(
                                    "displayName",
                                    item.text("name"),
                                ) ?: item.text("name"),
                            item.text("description"),
                            Icons.Default.AutoAwesome,
                            trailing = {
                                Switch(item.optBoolean("enabled"), enabled = pending != item.text("path"), onCheckedChange = { enabled ->
                                    scope.launch {
                                        pending = item.text("path")
                                        runCatching {
                                            repo.rpc(
                                                server,
                                                session.text("id"),
                                                "skills/config/write",
                                                obj(
                                                    "path" to pending,
                                                    "enabled" to enabled,
                                                ),
                                            )
                                        }.onSuccess { repo.data.refresh(query, true) }.onFailure(repo::report)
                                        pending = ""
                                    }
                                })
                            },
                            onClick = { selected = item },
                        )
                    }
                }
            }
            groups.flatMap { it.optJSONArray("errors")?.objects().orEmpty() }.forEach { error ->
                item { Text(error.text("message"), color = MaterialTheme.colorScheme.error) }
            }
        }
    }
    selected?.let { skill -> SkillContentsSheet(repo, server, session.text("id"), skill) { selected = null } }
    if (folders) SkillFoldersSheet(repo, server, session.text("id")) { folders = false }
}

@Composable fun ConnectionsScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
    thread: String,
) {
    var apps by remember { mutableStateOf(true) }
    var tool by remember { mutableStateOf<Pair<String, JSONObject>?>(null) }
    var resource by remember { mutableStateOf<Pair<String, JSONObject>?>(null) }
    val (query, state) =
        rememberRemoteData(
            repo,
            server,
            session.text("id"),
            "mcpServerStatus/list",
            JSONObject().apply {
                if (thread.isNotEmpty()) put("threadId", thread)
            },
        )
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    var selected by remember { mutableStateOf<JSONObject?>(null) }
    Column {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(apps, { apps = true }, label = { Text("Apps") })
            FilterChip(!apps, { apps = false }, label = { Text("Tools") })
        }
        if (apps) {
            ConnectedAppsScreen(repo, server, session.text("id"), thread)
        } else {
            RefreshPage(repo, query, state) {
                LazyColumn(contentPadding = PaddingValues(vertical = 16.dp)) {
                    item {
                        Text(
                            "Tools connected to this computer",
                            Modifier.padding(bottom = 20.dp),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    val connections =
                        state.value
                            ?.optJSONArray("data")
                            ?.objects()
                            .orEmpty()
                    if (connections.isEmpty() &&
                        !state.loading
                    ) {
                        item { EmptyState("No connected tools", "Add plugins to connect your apps and services.", Icons.Default.Link) }
                    }
                    items(connections, key = { it.text("name") }) { item ->
                        SettingsGroup {
                            SettingsRow(item.text("name"), humanize(item.text("authStatus")), Icons.Default.Link, onClick = {
                                selected =
                                    item
                            })
                            if (item.text("authStatus") == "notLoggedIn") {
                                TextButton(onClick = {
                                    scope.launch {
                                        runCatching {
                                            repo.rpc(
                                                server,
                                                session.text("id"),
                                                "mcpServer/oauth/login",
                                                obj("name" to item.text("name")),
                                            )
                                        }.onSuccess { result ->
                                            openWebLink(context, result.text("authorizationUrl"))
                                        }.onFailure(repo::report)
                                    }
                                }) { Text("Connect") }
                            }
                        }
                    }
                    item {
                        TextButton(onClick = {
                            scope.launch {
                                runCatching {
                                    repo.rpc(server, session.text("id"), "config/mcpServer/reload")
                                }.onSuccess { repo.data.refresh(query, true) }.onFailure(repo::report)
                            }
                        }) { Text("Reconnect tools") }
                    }
                }
            }
        }
    }
    tool?.let { (name, item) -> ConnectedToolSheet(repo, server, session.text("id"), thread, name, item) { tool = null } }
    resource?.let { (name, item) -> ResourceSheet(repo, server, session.text("id"), thread, name, item) { resource = null } }
    selected?.let { connection ->
        NativeSheet(connection.text("name"), { selected = null }) {
            Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState())) {
                SettingsGroup("Tools") {
                    connection.optJSONObject("tools")?.let { tools ->
                        tools.keys().asSequence().forEach { name ->
                            SettingsRow(
                                humanize(
                                    name,
                                ),
                                tools.optJSONObject(name)?.text("description").orEmpty(),
                                Icons.Default.Build,
                                onClick = {
                                    tool =
                                        connection.text("name") to JSONObject(tools.getJSONObject(name).toString()).put("name", name)
                                    selected = null
                                },
                            )
                        }
                    }
                }
                SettingsGroup("Resources") {
                    connection.optJSONArray("resources")?.objects()?.forEach { item ->
                        SettingsRow(item.text("name"), item.text("description"), Icons.Default.Description, onClick = {
                            resource =
                                connection.text("name") to item
                            selected = null
                        })
                    }
                }
            }
        }
    }
}

fun openWebLink(
    context: android.content.Context,
    url: String,
) {
    val uri = url.toUri()
    require(uri.scheme in setOf("https", "http") && !uri.host.isNullOrBlank()) { "The host did not return a valid sign-in link" }
    context.startActivity(Intent(Intent.ACTION_VIEW, uri))
}

@Composable fun AccountScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    val (query, account) = rememberRemoteData(repo, server, session.text("id"), "account/read")
    val (limitQuery, limits) = rememberRemoteData(repo, server, session.text("id"), "account/rateLimits/read")
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var login by remember { mutableStateOf<JSONObject?>(null) }
    var confirm by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    val resetId = remember { UUID.randomUUID().toString() }
    var activity by remember { mutableStateOf(false) }
    var provider by remember { mutableStateOf(false) }
    RefreshPage(repo, query, account) {
        LazyColumn(contentPadding = PaddingValues(vertical = 16.dp)) {
            item {
                val profile = account.value?.optJSONObject("account")
                SettingsGroup("Account on this computer") {
                    SettingsRow(
                        profile?.text("email", "Signed in") ?: "Sign in to Codex",
                        profile?.text("planType").orEmpty(),
                        Icons.Default.AccountCircle,
                    )
                    if (profile ==
                        null
                    ) {
                        SettingsRow("Sign in with ChatGPT", icon = Icons.AutoMirrored.Filled.Login, enabled = !busy, onClick = {
                            scope.launch {
                                busy = true
                                runCatching {
                                    repo.rpc(
                                        server,
                                        session.text("id"),
                                        "account/login/start",
                                        obj("type" to "chatgptDeviceCode"),
                                    )
                                }.onSuccess {
                                    login =
                                        it
                                }.onFailure(repo::report)
                                busy = false
                            }
                        })
                    } else {
                        SettingsRow("Sign out", icon = Icons.AutoMirrored.Filled.Logout, onClick = { confirm = "logout" })
                    }
                }
                SettingsGroup {
                    SettingsRow(
                        "Account activity",
                        "Token use and workspace messages",
                        Icons.Default.Insights,
                        onClick = { activity = true },
                    )
                    SettingsRow("Model provider", "API key or Amazon Bedrock", Icons.Default.Cloud, onClick = { provider = true })
                }
                DataStatus(limits) { repo.data.refresh(limitQuery, true) }
                val buckets =
                    limits.value?.optJSONObject("rateLimitsByLimitId")?.let { obj ->
                        obj
                            .keys()
                            .asSequence()
                            .mapNotNull { obj.optJSONObject(it) }
                            .toList()
                    }
                        ?: listOfNotNull(limits.value?.optJSONObject("rateLimits"))
                buckets.forEach { bucket ->
                    SettingsGroup(bucket.text("limitName", "Usage limits")) {
                        listOf("primary" to "Current window", "secondary" to "Weekly window").forEach { (key, title) ->
                            bucket.optJSONObject(key)?.let { window ->
                                val used =
                                    if (window.isNull(
                                            "usedPercent",
                                        )
                                    ) {
                                        null
                                    } else {
                                        window.optDouble("usedPercent").takeIf { it.isFinite() }?.coerceIn(0.0, 100.0)
                                    }
                                Column(Modifier.padding(20.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                                    Text(title, style = MaterialTheme.typography.titleMedium)
                                    Text(if (used == null) "Usage unavailable" else "${(100 - used).toInt()}% remaining")
                                    if (used !=
                                        null
                                    ) {
                                        LinearProgressIndicator(progress = { (used / 100).toFloat() }, modifier = Modifier.fillMaxWidth())
                                    }
                                    if (!window.isNull("resetsAt")) {
                                        Text(
                                            "Resets ${java.text.DateFormat.getDateTimeInstance(
                                                java.text.DateFormat.SHORT,
                                                java.text.DateFormat.SHORT,
                                            ).format(
                                                java.util.Date(
                                                    window.optLong("resetsAt") * 1000,
                                                ),
                                            )}",
                                            style = MaterialTheme.typography.bodySmall,
                                        )
                                    }
                                }
                            }
                        }
                    }
                }
                if (limits.value?.optJSONObject("rateLimitResetCredits") !=
                    null
                ) {
                    TextButton(onClick = { confirm = "reset" }) { Text("Use a usage reset") }
                }
                TextButton(onClick = { confirm = "credits" }) { Text("Ask workspace admin for credits") }
            }
        }
    }
    if (activity) AccountActivitySheet(repo, server, session.text("id")) { activity = false }
    if (provider) ProviderSetupSheet(repo, server, session.text("id")) { provider = false }
    login?.let { result ->
        NativeSheet("Sign in with ChatGPT", { login = null }, scrollContent = true) {
            Text("Enter this code on the sign-in page:")
            androidx.compose.foundation.text.selection.SelectionContainer {
                Text(
                    result.text("userCode", result.text("user_code")),
                    Modifier.padding(vertical = 20.dp),
                    style = MaterialTheme.typography.headlineMedium,
                )
            }
            Button(onClick = {
                runCatching {
                    openWebLink(context, result.text("verificationUrl", result.text("verification_url", result.text("authUrl"))))
                }.onFailure(repo::report)
            }, modifier = Modifier.fillMaxWidth()) { Text("Open sign-in page") }
            TextButton(onClick = {
                scope.launch {
                    runCatching {
                        repo.rpc(
                            server,
                            session.text("id"),
                            "account/login/cancel",
                            obj(
                                "loginId" to result.text("loginId"),
                            ),
                        )
                    }.onFailure(repo::report)
                    login = null
                }
            }) { Text("Cancel sign-in") }
        }
    }
    if (confirm.isNotEmpty()) {
        AlertDialog(onDismissRequest = { confirm = "" }, title = {
            Text(
                when (confirm) {
                    "logout" -> "Sign out on this computer?"
                    "credits" -> "Request more credits?"
                    else -> "Use one usage reset?"
                },
            )
        }, text = {
            Text(
                when (confirm) {
                    "logout" -> "New tasks will need you to sign in again."
                    "credits" -> "Send an email request to the workspace administrator to add credits."
                    else -> "This consumes one available reset credit."
                },
            )
        }, confirmButton = {
            TextButton(enabled = !busy, onClick = {
                scope.launch {
                    busy = true
                    runCatching {
                        repo.rpc(
                            server,
                            session.text("id"),
                            when (confirm) {
                                "logout" -> "account/logout"
                                "credits" -> "account/sendAddCreditsNudgeEmail"
                                else -> "account/rateLimitResetCredit/consume"
                            },
                            when (confirm) {
                                "logout" -> JSONObject()
                                "credits" ->
                                    obj(
                                        "creditType" to "credits",
                                    )
                                else -> obj("idempotencyKey" to resetId)
                            },
                        )
                    }.onSuccess {
                        repo.data.invalidate(server)
                        repo.data.refreshObserved(server)
                        confirm =
                            ""
                    }.onFailure(repo::report)
                    busy = false
                }
            }) { Text("Confirm") }
        }, dismissButton = { TextButton(onClick = { confirm = "" }) { Text("Cancel") } })
    }
}
