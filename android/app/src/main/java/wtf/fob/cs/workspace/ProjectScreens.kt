package wtf.fob.cs.workspace

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import kotlinx.coroutines.launch
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*
import kotlin.time.Duration.Companion.milliseconds

@Composable fun GuideScreen(done: () -> Unit) {
    Column(
        Modifier
            .fillMaxSize()
            .safeDrawingPadding()
            .verticalScroll(rememberScrollState())
            .padding(24.dp),
        verticalArrangement = Arrangement.Center,
    ) {
        Icon(Icons.Default.Terminal, null, Modifier.size(48.dp), tint = MaterialTheme.colorScheme.primary)
        Spacer(Modifier.height(20.dp))
        Text("Your Codex, wherever you are", style = MaterialTheme.typography.headlineLarge)
        Text("Work with Codex running on your own computers.", Modifier.padding(vertical = 16.dp))
        listOf(
            "1. Connect a computer" to "Run codex-start connect on the host, then scan its QR code. You can add several servers.",
            "2. Choose a project" to "Pick a folder on the host. Open an existing chat or start a new one.",
            "3. Keep work moving" to
                "Send messages, review changes, and answer approval requests. " +
                "Start task monitoring for notifications.",
        ).forEach { (title, detail) ->
            Text(title, style = MaterialTheme.typography.titleMedium)
            Text(detail, Modifier.padding(top = 4.dp, bottom = 20.dp), style = MaterialTheme.typography.bodyMedium)
        }
        Text("Model access and credentials stay on the host.", style = MaterialTheme.typography.bodySmall)
        Button(onClick = done, Modifier.fillMaxWidth().padding(top = 24.dp)) { Text("Get started") }
    }
}

@Composable fun EmptyServersScreen(
    add: () -> Unit,
    settings: () -> Unit,
) {
    Column(
        Modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Icon(Icons.Default.Dns, null, Modifier.size(64.dp), tint = MaterialTheme.colorScheme.primary)
        Text("Connect your first server", Modifier.padding(top = 24.dp), style = MaterialTheme.typography.headlineSmall)
        Text("Add a computer to see its projects and chats.", Modifier.padding(vertical = 16.dp))
        Button(onClick = add, Modifier.fillMaxWidth()) {
            Icon(Icons.Default.Add, null)
            Spacer(Modifier.width(8.dp))
            Text("Add server")
        }
        OutlinedButton(onClick = settings, Modifier.fillMaxWidth().padding(top = 8.dp)) { Text("Settings") }
    }
}

data class RecentChat(
    val session: JSONObject,
    val chat: JSONObject,
)

data class ProjectChatGroup(
    val project: JSONObject,
    val chats: List<RecentChat>,
)

fun projectChatGroups(home: JSONObject?): List<ProjectChatGroup> {
    val chatsByPath =
        home
            ?.optJSONArray("projectChats")
            ?.objects()
            .orEmpty()
            .map { RecentChat(it.getJSONObject("session"), it.getJSONObject("chat")) }
            .groupBy { it.session.text("cwd") }
    return home
        ?.optJSONArray("projects")
        ?.objects()
        .orEmpty()
        .map { project -> ProjectChatGroup(project, chatsByPath[project.text("path")].orEmpty()) }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProjectsScreen(
    repo: RemoteRepository,
    server: String,
    connected: Boolean,
    openChat: (RecentChat) -> Unit,
    open: (JSONObject, JSONObject) -> Unit,
) {
    val (query, state) = rememberRemoteData(repo, server, "", "ui/home", enabled = connected)
    val projects = projectChatGroups(state.value)
    val loading = state.loading
    var search by remember(server) { mutableStateOf("") }
    var opening by remember(server) { mutableStateOf("") }
    var picker by remember { mutableStateOf(false) }
    var profileProject by remember(server) { mutableStateOf<JSONObject?>(null) }
    var profiles by remember(server) { mutableStateOf<List<String>>(emptyList()) }
    val scope = rememberCoroutineScope()

    fun openProject(
        project: JSONObject,
        profile: String? = null,
    ) {
        scope.launch {
            profileProject = null
            opening = project.text("name")
            runCatching {
                repo.request(server, "project/open", obj("projectId" to project.getString("id"), "profile" to profile)) as JSONObject
            }.onSuccess {
                repo.prefetchSession(server, it.getJSONObject("session"))
                open(it.getJSONObject("project"), it.getJSONObject("session"))
            }.onFailure(repo::report)
            opening = ""
        }
    }

    suspend fun refresh() {
        repo.data.invalidate(server)
        runCatching { repo.data.read(query, true) }.onFailure(repo::report)
    }
    val visibleProjects =
        projects.mapNotNull { group ->
            val projectMatches =
                group.project.text("name").contains(search, true) ||
                    group.project.text("path").contains(search, true)
            val chats =
                if (search.isBlank() || projectMatches) {
                    group.chats
                } else {
                    group.chats.filter { chatTitle(it.chat).contains(search, true) }
                }
            group.takeIf { projectMatches || chats.isNotEmpty() }?.copy(chats = chats)
        }
    PullToRefreshBox(isRefreshing = state.refreshing && state.value != null, onRefresh = {
        scope.launch { refresh() }
    }, modifier = Modifier.fillMaxSize()) {
        Column(Modifier.fillMaxSize()) {
            DataStatus(state) { repo.data.refresh(query, true) }
            SearchField(search, { search = it }, "Search projects and chats", Modifier.padding(top = 8.dp, bottom = 12.dp))
            if (opening.isNotEmpty()) LinearProgressIndicator(Modifier.fillMaxWidth())
            if (opening.isNotEmpty()) {
                Text(
                    "Opening $opening…",
                    Modifier.padding(vertical = 12.dp),
                    style = MaterialTheme.typography.bodySmall,
                )
            }
            LazyColumn(Modifier.weight(1f), contentPadding = PaddingValues(bottom = 24.dp)) {
                item {
                    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                        Text("Projects", Modifier.weight(1f), style = MaterialTheme.typography.titleMedium)
                        IconButton(onClick = {
                            scope.launch { refresh() }
                        }, enabled = connected && !loading) { Icon(Icons.Default.Refresh, "Refresh projects", Modifier.size(20.dp)) }
                        TextButton(onClick = { picker = true }, enabled = connected && opening.isEmpty()) {
                            Icon(Icons.Default.Add, null, Modifier.size(18.dp))
                            Spacer(Modifier.width(4.dp))
                            Text("Add project")
                        }
                    }
                }
                if (!loading && state.error == null && projects.isEmpty()) {
                    item {
                        Text(
                            if (connected) {
                                "Choose a folder on this server to start working."
                            } else {
                                "Projects will appear when the server connects."
                            },
                            Modifier.padding(vertical = 24.dp),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
                if (projects.isNotEmpty() && visibleProjects.isEmpty()) {
                    item {
                        EmptyState(
                            "No matching projects or chats",
                            "Try another name or open a project to search older chats.",
                            Icons.Default.SearchOff,
                        )
                    }
                }
                visibleProjects.forEach { group ->
                    val project = group.project
                    item(key = "project/${project.getString("id")}") {
                        Column {
                            ProjectRow(project.getString("name"), project.getString("path"), connected && opening.isEmpty()) {
                                if (repo.servers.value
                                        .firstOrNull { it.id == server }
                                        ?.capabilities
                                        ?.contains("launcherSettings") ==
                                    true
                                ) {
                                    scope.launch {
                                        opening = project.text("name")
                                        runCatching {
                                            val names = repo.data.read(RemoteQuery(server, "", "launcher/list")).getJSONArray("profiles")
                                            profiles = (0 until names.length()).map { names.getString(it) }
                                            if (profiles.isEmpty()) openProject(project) else profileProject = project
                                        }.onFailure(repo::report)
                                        opening = ""
                                    }
                                } else {
                                    openProject(project)
                                }
                            }
                            if (group.chats.isEmpty() && search.isBlank() && !state.refreshing) {
                                Text(
                                    "No chats in this project.",
                                    Modifier.padding(start = 52.dp, bottom = 12.dp),
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        }
                    }
                    items(
                        group.chats,
                        key = { "project/${project.text("id")}/${it.session.text("profile")}/chat/${it.chat.text("id")}" },
                    ) { item ->
                        Box(Modifier.padding(start = 40.dp)) {
                            ChatRow(item.chat, item.session.text("profile")) { openChat(item) }
                        }
                    }
                    item(key = "project/${project.getString("id")}/divider") {
                        HorizontalDivider(Modifier.padding(bottom = 8.dp))
                    }
                }
                if (projects.isNotEmpty()) {
                    item {
                        Text(
                            "Recent chats are shown here. Open a project to see all its chats.",
                            Modifier.padding(vertical = 12.dp),
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                }
            }
        }
    }
    profileProject?.let { project ->
        NativeSheet("Open ${project.text("name")}", { profileProject = null }, scrollContent = true) {
            Text("Choose a profile for this project. Each profile keeps its own session.")
            SettingsRow("Use project defaults", icon = Icons.Default.Folder, onClick = { openProject(project) })
            profiles.forEach { profile ->
                SettingsRow(profile, icon = Icons.Default.Tune, onClick = { openProject(project, profile) })
            }
        }
    }
    if (picker) {
        DirectoryPicker(repo, server, { picker = false }) { path ->
            scope.launch {
                runCatching { repo.request(server, "project/add", obj("path" to path)) }
                    .onSuccess {
                        picker = false
                        refresh()
                    }.onFailure(repo::report)
            }
        }
    }
}

@Composable private fun DirectoryPicker(
    repo: RemoteRepository,
    server: String,
    close: () -> Unit,
    choose: (String) -> Unit,
) {
    var listing by remember { mutableStateOf<JSONObject?>(null) }
    val path = listing?.text("path").orEmpty()
    val parent = listing?.text("parent").orEmpty()
    val home = listing?.text("home").orEmpty()
    val cursor = listing?.text("nextCursor").orEmpty()
    var hidden by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    val entries = listing?.optJSONArray("data")?.objects().orEmpty()
    var failure by remember { mutableStateOf("") }
    val scope = rememberCoroutineScope()

    suspend fun load(
        target: String,
        append: Boolean = false,
    ) {
        busy = true
        failure = ""
        runCatching {
            val result =
                repo.request(
                    server,
                    "directory/list",
                    obj(
                        "path" to target,
                        "hidden" to hidden,
                        "cursor" to cursor.takeIf { append },
                    ),
                ) as JSONObject
            listing =
                result.put(
                    "data",
                    org.json.JSONArray(
                        (
                            (if (append) entries else emptyList()) +
                                result.getJSONArray("data").objects()
                        ).takeLast(500),
                    ),
                )
        }.onFailure { failure = it.message ?: "Cannot open this folder" }
        busy = false
    }
    LaunchedEffect(hidden) { load(path) }
    Dialog(onDismissRequest = close, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Surface(Modifier.fillMaxWidth(0.96f).fillMaxHeight(0.86f), shape = MaterialTheme.shapes.extraLarge) {
            Column(Modifier.padding(20.dp)) {
                Text("Choose a project folder", style = MaterialTheme.typography.headlineSmall)
                Text(path.ifEmpty { "Host home directory" }, Modifier.padding(vertical = 12.dp), style = MaterialTheme.typography.bodySmall)
                Row(verticalAlignment = Alignment.CenterVertically) {
                    IconButton(onClick = {
                        scope.launch { load(parent) }
                    }, enabled = parent.isNotEmpty() && !busy) { Icon(Icons.AutoMirrored.Filled.ArrowBack, "Parent folder") }
                    TextButton(onClick = { scope.launch { load(home) } }, enabled = !busy) { Text("Home") }
                    Spacer(Modifier.weight(1f))
                    FilterChip(selected = hidden, onClick = { hidden = !hidden }, label = { Text("Hidden files") })
                }
                if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
                if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
                LazyColumn(Modifier.weight(1f)) {
                    if (!busy && entries.isEmpty()) item { Text("This folder is empty.", Modifier.padding(16.dp)) }
                    items(entries, key = { it.getString("path") }) { entry ->
                        TextButton(
                            onClick = { scope.launch { load(entry.getString("path")) } },
                            enabled =
                                entry.getBoolean("directory") && !busy,
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Icon(if (entry.getBoolean("directory")) Icons.Default.Folder else Icons.Default.Description, null)
                            Text(
                                entry.getString("name"),
                                Modifier.weight(1f).padding(start = 12.dp),
                                maxLines = 2,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                    if (cursor.isNotEmpty()) {
                        item {
                            TextButton(
                                onClick = { scope.launch { load(path, true) } },
                                enabled = !busy,
                            ) { Text("Load more") }
                        }
                    }
                }
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                    TextButton(onClick = close) { Text("Cancel") }
                    Button(onClick = { choose(path) }, enabled = path.isNotEmpty() && !busy) { Text("Use this folder") }
                }
            }
        }
    }
}

@Composable fun ProjectChatsScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
    open: (JSONObject) -> Unit,
) {
    var busy by remember { mutableStateOf(false) }
    var search by remember { mutableStateOf("") }
    var submittedSearch by remember { mutableStateOf("") }
    var archived by remember { mutableStateOf(false) }
    val parameters =
        obj("limit" to 50, "sourceKinds" to chatSources(), "cwd" to session.getString("executionCwd")).apply {
            if (archived) put("archived", true)
            if (submittedSearch.isNotBlank()) put("searchTerm", submittedSearch)
        }
    val (query, state) = rememberRemoteData(repo, server, session.text("id"), "ui/chats", parameters)
    var extra by remember(query) { mutableStateOf<List<JSONObject>>(emptyList()) }
    var nextCursor by remember(query) { mutableStateOf<String?>(null) }
    val chats =
        (
            state.value
                ?.optJSONArray("data")
                ?.objects()
                .orEmpty() + extra
        ).distinctBy { it.text("id") }
    val cursor = nextCursor ?: state.value?.text("nextCursor").orEmpty()
    val scope = rememberCoroutineScope()
    LaunchedEffect(search) {
        kotlinx.coroutines.delay(300.milliseconds)
        submittedSearch = search.trim()
    }
    LaunchedEffect(state.value) {
        extra = emptyList()
        nextCursor = null
    }

    suspend fun refresh(append: Boolean = false) {
        busy = true
        runCatching {
            if (append) {
                val result = repo.listProjectChats(server, session, JSONObject(parameters.toString()).put("cursor", cursor))
                extra = (extra + result.getJSONArray("data").objects()).take(500)
                nextCursor = result.text("nextCursor")
            } else {
                repo.data.read(query, true)
            }
        }.onFailure(repo::report)
        busy = false
    }

    fun newChat() {
        if (busy) return
        busy = true
        scope.launch {
            runCatching {
                repo.rpc(
                    server,
                    session.getString("id"),
                    "thread/start",
                    obj(
                        "cwd" to session.getString("executionCwd"),
                        "historyMode" to "paginated",
                    ),
                )
            }.onSuccess { open(it.getJSONObject("thread")) }
                .onFailure(repo::report)
            busy = false
        }
    }
    KeyboardAction(KeyboardCommand.NewChat, ::newChat)
    Column {
        Row(Modifier.fillMaxWidth().padding(vertical = 12.dp), verticalAlignment = Alignment.CenterVertically) {
            Text("Chats", Modifier.weight(1f), style = MaterialTheme.typography.titleLarge)
            IconButton(onClick = { scope.launch { refresh() } }, enabled = !busy) { Icon(Icons.Default.Refresh, "Refresh chats") }
            FilledTonalButton(onClick = ::newChat, enabled = !busy) {
                Icon(Icons.Default.Add, null)
                Text("New chat")
            }
        }
        SearchField(search, { search = it }, "Search chats", search = { if (!busy) scope.launch { refresh() } })
        FilterChip(archived, { archived = !archived }, label = { Text("Archived") })
        DataStatus(state) { repo.data.refresh(query, true) }
        if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
        LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            if (!busy &&
                !state.loading &&
                chats.isEmpty()
            ) {
                item {
                    Text(
                        if (archived) "No archived chats." else "No chats yet. Start a new chat in this project.",
                        Modifier.padding(vertical = 20.dp),
                    )
                }
            }
            items(chats, key = { it.getString("id") }) { chat -> ChatRow(chat) { open(chat) } }
            if (cursor.isNotEmpty()) {
                item {
                    TextButton(
                        onClick = { scope.launch { refresh(true) } },
                        enabled = !busy,
                    ) { Text("Load more chats") }
                }
            }
        }
    }
}
