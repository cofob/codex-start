package wtf.fob.cs.navigation

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.*
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.settings.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import kotlin.time.Duration.Companion.milliseconds

@Composable fun AppDrawer(
    drawerState: DrawerState,
    server: RemoteServer?,
    home: JSONObject?,
    page: Int,
    visible: Boolean,
    navigate: (Int) -> Unit,
    feature: (FeaturePage) -> Unit,
    settings: () -> Unit,
    switchServer: () -> Unit,
    open: (RecentChat) -> Unit,
) {
    ModalDrawerSheet(
        drawerState = drawerState,
        modifier = Modifier.fillMaxHeight().widthIn(max = 360.dp),
        drawerContainerColor = MaterialTheme.colorScheme.background,
    ) {
        Column(Modifier.fillMaxHeight().then(if (visible) Modifier else Modifier.clearAndSetSemantics {})) {
            Row(Modifier.fillMaxWidth().padding(20.dp), verticalAlignment = Alignment.CenterVertically) {
                Text("Codex", Modifier.weight(1f), style = MaterialTheme.typography.headlineLarge)
                FilledTonalIconButton(onClick = { navigate(8) }) { Icon(Icons.Default.Search, "Search all chats") }
            }
            LazyColumn(Modifier.weight(1f), contentPadding = PaddingValues(horizontal = 12.dp)) {
                item {
                    NavigationDrawerItem(label = {
                        Text("Projects")
                    }, selected = page == 0, onClick = { navigate(0) }, icon = { Icon(Icons.Default.FolderOpen, null) })
                    NavigationDrawerItem(
                        label = { Text("Remote") },
                        selected = false,
                        onClick = switchServer,
                        icon = { Icon(Icons.Default.Computer, null) },
                    )
                    NavigationDrawerItem(
                        label = { Text("Terminals") },
                        selected = page == 9,
                        onClick = { navigate(9) },
                        icon = { Icon(Icons.Default.Terminal, null) },
                    )
                    listOf(
                        FeaturePage.Plugins,
                        FeaturePage.Skills,
                        FeaturePage.Connections,
                        FeaturePage.Organization,
                    ).forEach { destination ->
                        NavigationDrawerItem(label = {
                            Text(destination.title)
                        }, selected = false, onClick = { feature(destination) }, icon = { Icon(destination.icon, null) })
                    }
                    Text(
                        "Recent chats",
                        Modifier.padding(start = 16.dp, top = 28.dp, bottom = 12.dp),
                        style = MaterialTheme.typography.labelLarge,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                val recent = home?.optJSONArray("recent")?.objects().orEmpty()
                if (recent.isEmpty()) {
                    item {
                        Text(
                            "Your chats will appear here.",
                            Modifier.padding(16.dp),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            style = MaterialTheme.typography.bodyMedium,
                        )
                    }
                }
                items(recent, key = { it.getJSONObject("session").text("profile") + "/" + it.getJSONObject("chat").text("id") }) { item ->
                    NavigationDrawerItem(label = {
                        Column {
                            Text(
                                chatTitle(item.getJSONObject("chat")),
                                maxLines = 1,
                                overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                            )
                            item.getJSONObject("session").text("profile").takeIf { it.isNotBlank() }?.let { profile ->
                                Text(
                                    profile,
                                    maxLines = 1,
                                    overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                                    style = MaterialTheme.typography.labelSmall,
                                )
                            }
                        }
                    }, selected = false, onClick = { open(RecentChat(item.getJSONObject("session"), item.getJSONObject("chat"))) })
                }
            }
            SettingsRow(server?.name ?: "Your workspace", "Settings", Icons.Default.AccountCircle, onClick = settings)
            Spacer(Modifier.height(12.dp))
        }
    }
}

@Composable fun SearchScreen(
    repo: RemoteRepository,
    server: String,
    home: JSONObject?,
    open: (RecentChat) -> Unit,
) {
    val searchFocus = remember { FocusRequester() }
    LaunchedEffect(Unit) { searchFocus.requestFocus() }
    KeyboardAction(KeyboardCommand.Search) { searchFocus.requestFocus() }
    var search by rememberSaveable { mutableStateOf("") }
    var matches by remember(server) { mutableStateOf<List<RecentChat>>(emptyList()) }
    var busy by remember { mutableStateOf(false) }
    var failure by remember { mutableStateOf("") }
    val sessions =
        home
            ?.optJSONArray("sessions")
            ?.objects()
            .orEmpty()
            .filter { it.supportsTasks() }
    LaunchedEffect(server, search, sessions.map { it.text("id") }) {
        if (search.isBlank()) {
            matches = emptyList()
            busy = false
            return@LaunchedEffect
        }
        delay(300.milliseconds)
        busy = true
        failure = ""
        try {
            matches =
                coroutineScope {
                    sessions
                        .take(8)
                        .map { session ->
                            async {
                                repo.data
                                    .read(
                                        RemoteQuery.of(
                                            server,
                                            session.text("id"),
                                            "thread/search",
                                            obj(
                                                "searchTerm" to search.trim(),
                                                "limit" to 50,
                                            ),
                                        ),
                                    ).optJSONArray("data")
                                    ?.objects()
                                    .orEmpty()
                                    .map {
                                        RecentChat(
                                            session,
                                            it.optJSONObject("thread") ?: it,
                                        )
                                    }
                            }
                        }.awaitAll()
                        .flatten()
                        .distinctBy { it.session.text("profile") to it.chat.text("id") }
                }
        } catch (
            cancelled: CancellationException,
        ) {
            throw cancelled
        } catch (error: Exception) {
            failure = error.message.orEmpty()
        } finally {
            busy = false
        }
    }
    Column {
        SearchField(search, { search = it }, "Search your chats", Modifier.padding(vertical = 16.dp).focusRequester(searchFocus))
        if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
        if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
        LazyColumn {
            if (search.isBlank()) {
                item { EmptyState("Find a conversation", "Search messages across projects on this computer.", Icons.Default.Search) }
            } else if (matches.isEmpty() &&
                !busy &&
                failure.isEmpty()
            ) {
                item { EmptyState("No matching chats", "Try another word or phrase.", Icons.Default.SearchOff) }
            }
            items(
                matches,
                key = { it.session.text("profile") + "/" + it.chat.text("id") },
            ) { chat ->
                ChatRow(
                    chat.chat,
                    listOf(projectTitle(chat.session.text("cwd")), chat.session.text("profile"))
                        .filter {
                            it.isNotBlank()
                        }.joinToString(" · "),
                ) { open(chat) }
            }
        }
    }
}
