package wtf.fob.cs.app

import android.Manifest
import android.annotation.SuppressLint
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.view.KeyboardShortcutGroup
import android.view.Menu
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.focusable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.*
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.Chat
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.Saver
import androidx.compose.runtime.saveable.listSaver
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.key.onKeyEvent
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.core.content.edit
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation.compose.currentBackStackEntryAsState
import androidx.navigation.compose.rememberNavController
import kotlinx.coroutines.launch
import org.json.JSONObject
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.pairing.*
import wtf.fob.cs.settings.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*

class MainActivity : ComponentActivity() {
    private var incoming by mutableStateOf(Intent())
    private val notifications = registerForActivityResult(ActivityResultContracts.RequestPermission()) { startMonitoring() }

    private fun startMonitoring() {
        ContextCompat.startForegroundService(this, Intent(this, MonitorService::class.java))
    }

    fun monitor() {
        if (Build.VERSION.SDK_INT >= 33) notifications.launch(Manifest.permission.POST_NOTIFICATIONS) else startMonitoring()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        incoming = intent
        enableEdgeToEdge()
        if (getSharedPreferences("monitoring", MODE_PRIVATE).getBoolean("enabled", false)) startMonitoring()
        setContent { CodexTheme { CodexApp((application as CodexApplication).repository, incoming, ::monitor) } }
    }

    override fun onProvideKeyboardShortcuts(
        data: MutableList<KeyboardShortcutGroup>?,
        menu: Menu?,
        deviceId: Int,
    ) {
        super.onProvideKeyboardShortcuts(data, menu, deviceId)
        data?.addAll(keyboardShortcutGroups())
    }

    override fun onStart() {
        super.onStart()
        (application as CodexApplication).repository.setForeground(true)
    }

    override fun onStop() {
        (application as CodexApplication).repository.setForeground(false)
        super.onStop()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        incoming = intent
    }
}

@Composable fun CodexTheme(content: @Composable () -> Unit) {
    val context = LocalContext.current
    val preferences = remember { context.getSharedPreferences("appearance", 0) }
    var theme by remember { mutableStateOf(preferences.getString("theme", "system")) }
    var dynamic by remember { mutableStateOf(preferences.getBoolean("dynamic", false)) }
    DisposableEffect(preferences) {
        val listener =
            android.content.SharedPreferences.OnSharedPreferenceChangeListener { _, _ ->
                theme =
                    preferences.getString("theme", "system")
                dynamic = preferences.getBoolean("dynamic", false)
            }
        preferences.registerOnSharedPreferenceChangeListener(listener)
        onDispose { preferences.unregisterOnSharedPreferenceChangeListener(listener) }
    }
    val dark =
        when (theme) {
            "dark" -> true
            "light" -> false
            else -> isSystemInDarkTheme()
        }
    val activity = androidx.activity.compose.LocalActivity.current
    val view = LocalView.current
    SideEffect {
        activity?.window?.let { window ->
            androidx.core.view.WindowCompat.getInsetsController(window, view).apply {
                isAppearanceLightStatusBars = !dark
                isAppearanceLightNavigationBars = !dark
            }
        }
    }
    val colors =
        if (dynamic &&
            Build.VERSION.SDK_INT >= 31
        ) {
            if (dark) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        } else if (dark) {
            darkColorScheme(
                primary = Color(0xFFA9C7FF),
                onPrimary = Color(0xFF002F66),
                primaryContainer = Color(0xFF17467D),
                secondaryContainer = Color(0xFF303944),
            )
        } else {
            lightColorScheme(
                primary = Color(0xFF2867CD),
                onPrimary = Color.White,
                primaryContainer = Color(0xFFD8E6FF),
                secondaryContainer = Color(0xFFE3EAF3),
            )
        }
    val neutral =
        colors.copy(
            background = Color(if (dark) 0xFF08090B else 0xFFFCFCFE),
            surface = Color(if (dark) 0xFF08090B else 0xFFFCFCFE),
            onSurface = Color(if (dark) 0xFFF3F3F5 else 0xFF1A1B1F),
            onSurfaceVariant = Color(if (dark) 0xFFB5B7BF else 0xFF5D616A),
            surfaceContainerLow = Color(if (dark) 0xFF1B1C20 else 0xFFF1F2F6),
            surfaceContainer = Color(if (dark) 0xFF222328 else 0xFFECEEF3),
            surfaceContainerHigh = Color(if (dark) 0xFF2D2F34 else 0xFFE6E8EF),
            outlineVariant = Color(if (dark) 0xFF36383E else 0xFFDADDE5),
        )
    MaterialTheme(colorScheme = neutral) { Surface(Modifier.fillMaxSize(), content = content) }
}

private val PendingTaskSaver =
    listSaver<MutableState<Triple<String, String, String>?>, String>(
        save = { it.value?.let { target -> listOf(target.first, target.second, target.third) }.orEmpty() },
        restore = { mutableStateOf(if (it.size == 3) Triple(it[0], it[1], it[2]) else null) },
    )

private val JsonStateSaver =
    Saver<MutableState<JSONObject?>, String>(
        save = { it.value?.toString().orEmpty() },
        restore = { mutableStateOf(it.takeIf(String::isNotEmpty)?.let(::JSONObject)) },
    )

@OptIn(ExperimentalMaterial3Api::class)
@SuppressLint("UnusedBoxWithConstraintsScope")
@Composable
fun CodexApp(
    repo: RemoteRepository,
    incoming: Intent,
    monitor: () -> Unit,
) {
    val context = LocalContext.current
    val preferences = remember { context.getSharedPreferences("onboarding", android.content.Context.MODE_PRIVATE) }
    var guide by remember { mutableStateOf(!preferences.getBoolean("complete", false)) }
    if (guide) {
        GuideScreen {
            preferences.edit { putBoolean("complete", true) }
            guide = false
        }
        return
    }
    val servers by repo.servers.collectAsStateWithLifecycle()
    val error by repo.error.collectAsStateWithLifecycle()
    var server by rememberSaveable { mutableStateOf("") }
    var project by rememberSaveable(saver = JsonStateSaver) { mutableStateOf(null) }
    var session by rememberSaveable(saver = JsonStateSaver) { mutableStateOf(null) }
    var thread by rememberSaveable { mutableStateOf("") }
    var title by rememberSaveable { mutableStateOf("New chat") }
    // Projects, chats, conversation, changes, files, settings.
    val navigation = rememberNavController()
    val entry by navigation.currentBackStackEntryAsState()
    val page = entry?.arguments?.getInt("page") ?: 0
    var feature by rememberSaveable { mutableStateOf(FeaturePage.Plugins) }
    val scope = rememberCoroutineScope()

    fun navigate(destination: Int) {
        val projectKey = "$server/${session?.text("id")}/${project?.text("path")}"
        scope.launch(kotlinx.coroutines.Dispatchers.Main.immediate) {
            navigation.openScreen(destination, projectKey, thread, feature.name)
        }
    }
    var switcher by remember { mutableStateOf(false) }
    var addServer by rememberSaveable { mutableStateOf(false) }
    var invitation by rememberSaveable { mutableStateOf("") }
    val drawer = rememberDrawerState(DrawerValue.Closed)
    var action by remember { mutableStateOf<String?>(null) }
    var pendingTask by rememberSaveable(saver = PendingTaskSaver) { mutableStateOf(null) }
    val snackbar = remember { SnackbarHostState() }
    LaunchedEffect(error) {
        error?.let {
            snackbar.showSnackbar(it, duration = SnackbarDuration.Long)
            repo.clearError()
        }
    }
    LaunchedEffect(servers, pendingTask) {
        val target = pendingTask
        if (target != null && servers.any { it.id == target.first && it.status == "Connected" }) {
            runCatching {
                val found =
                    (repo.request(target.first, "session/list") as JSONObject).getJSONArray("data").objects().firstOrNull {
                        it.text("id") ==
                            target.second
                    }
                if (found != null) {
                    session = found
                    project =
                        obj("name" to projectTitle(found.getString("cwd")), "path" to found.getString("cwd"), "id" to "")
                    server = target.first
                    thread = target.third
                    navigate(2)
                }
                pendingTask = null
            }.onFailure(repo::report)
        } else if (target == null && servers.none { it.id == server }) {
            server = servers.firstOrNull()?.id.orEmpty()
            session = null
            project = null
            thread = ""
            if (page != 5) navigate(0)
        }
    }
    var handledIncoming by rememberSaveable { mutableStateOf("") }
    LaunchedEffect(incoming) {
        val key = incoming.toUri(Intent.URI_INTENT_SCHEME)
        if (handledIncoming == key) return@LaunchedEffect
        handledIncoming = key
        incoming.dataString?.takeIf { isInvitationLink(it) }?.let {
            invitation = it
            addServer = true
        }
        incoming.getStringExtra("server")?.takeIf { it.isNotEmpty() }?.let { id ->
            server = id
            session = null
            pendingTask = Triple(id, incoming.getStringExtra("session").orEmpty(), incoming.getStringExtra("thread").orEmpty())
        }
    }
    LaunchedEffect(server, thread, session?.text("id")) {
        if (thread.isNotEmpty() && session != null) {
            runCatching {
                title =
                    chatTitle(
                        repo
                            .rpc(
                                server,
                                session!!.getString("id"),
                                "thread/read",
                                obj("threadId" to thread, "includeTurns" to false),
                            ).getJSONObject("thread"),
                    )
            }
        }
    }
    LaunchedEffect(server, thread) {
        repo.events.collect { event ->
            if (event.server == server &&
                event.message.text("method") == "thread/name/updated" &&
                event.message.optJSONObject("params")?.text("threadId") == thread
            ) {
                event.message
                    .optJSONObject("params")
                    ?.text("threadName")
                    ?.takeIf { it.isNotBlank() }
                    ?.let { title = it }
            }
        }
    }
    val (_, home) = rememberRemoteData(repo, server, "", "ui/home", enabled = servers.any { it.id == server && it.status == "Connected" })
    val effectiveSession =
        session?.takeIf { it.supportsTasks() } ?: home.value
            ?.optJSONArray("sessions")
            ?.objects()
            ?.firstOrNull { it.supportsTasks() }
    LaunchedEffect(server, effectiveSession?.text("id")) { effectiveSession?.let { repo.prefetchSession(server, it) } }
    val selected = servers.firstOrNull { it.id == server }
    var keyboardHelp by remember { mutableStateOf(false) }
    val keyboardFocus = remember { FocusRequester() }
    val view = LocalView.current
    val keyboardActions = remember { KeyboardActions() }
    LaunchedEffect(Unit) { keyboardFocus.requestFocus() }
    Box(
        Modifier
            .fillMaxSize()
            .onKeyEvent { event ->
                // The terminal owns all of its chords. Dialogs use a separate window.
                if (view.rootView.findFocus() is TerminalView || switcher || addServer || action != null || keyboardHelp) {
                    false
                } else if (keyboardActions.handle(event)) {
                    true
                } else {
                    when (event.command()) {
                        KeyboardCommand.Search ->
                            event.runShortcut {
                                navigate(8)
                                scope.launch { drawer.close() }
                            }
                        KeyboardCommand.SwitchServer ->
                            event.runShortcut {
                                switcher = true
                                scope.launch { drawer.close() }
                            }
                        KeyboardCommand.Settings ->
                            event.runShortcut {
                                navigate(5)
                                scope.launch { drawer.close() }
                            }
                        KeyboardCommand.Projects ->
                            event.runShortcut {
                                navigate(0)
                                scope.launch { drawer.close() }
                            }
                        KeyboardCommand.Back, KeyboardCommand.Escape ->
                            event.runShortcut {
                                if (drawer.isOpen) scope.launch { drawer.close() } else navigation.popBackStack()
                            }
                        KeyboardCommand.Chat, KeyboardCommand.Changes, KeyboardCommand.Files -> {
                            if (page !in 2..4 || session == null) {
                                false
                            } else {
                                event.runShortcut {
                                    navigate(
                                        when (event.command()) {
                                            KeyboardCommand.Chat -> 2
                                            KeyboardCommand.Changes -> 3
                                            else -> 4
                                        },
                                    )
                                }
                            }
                        }
                        KeyboardCommand.Help -> event.runShortcut { keyboardHelp = true }
                        else -> false
                    }
                }
            }.focusRequester(keyboardFocus)
            .focusable(),
    ) {
        AdaptiveNavigation(drawerState = drawer, navigation = { visible ->
            AppDrawer(drawer, selected, home.value, page, visible, { destination ->
                navigate(destination)
                scope.launch { drawer.close() }
            }, { target ->
                feature = target
                navigate(7)
                scope.launch { drawer.close() }
            }, {
                navigate(5)
                scope.launch { drawer.close() }
            }, {
                switcher = true
                scope.launch { drawer.close() }
            }, { recent ->
                session = recent.session
                project =
                    obj("name" to projectTitle(recent.session.text("cwd")), "path" to recent.session.text("cwd"), "id" to "")
                thread = recent.chat.text("id")
                title = chatTitle(recent.chat)
                navigate(2)
                scope.launch { drawer.close() }
            })
        }) { persistentNavigation ->
            BoxWithConstraints(Modifier.fillMaxSize()) {
                val wide = maxWidth >= 600.dp
                val compactHeight = maxHeight < 480.dp
                val keyboardVisible = WindowInsets.ime.getBottom(androidx.compose.ui.platform.LocalDensity.current) > 0
                AppScreenHost(navigation) { screenEntry ->
                    val level = screenEntry.arguments?.getInt("page") ?: 0
                    val chatTab by screenEntry.savedStateHandle.getStateFlow(CHAT_TAB, 2).collectAsState()
                    val screenPage = if (level == 2) chatTab else level
                    val screenFeature = FeaturePage.valueOf(screenEntry.arguments?.getString("feature") ?: FeaturePage.Plugins.name)
                    val tabStates =
                        androidx.compose.runtime.saveable
                            .rememberSaveableStateHolder()
                    val inChat = screenPage in 2..4 && session != null
                    val chatLabels = listOf("Chat", "Changes", "Files")
                    val chatIcons = listOf(Icons.AutoMirrored.Filled.Chat, Icons.Default.Difference, Icons.Default.Folder)
                    val heading =
                        when {
                            screenPage == 7 -> screenFeature.title
                            screenPage == 8 -> "Search"
                            screenPage == 9 -> "Terminals"
                            screenPage == 5 -> "Settings"
                            screenPage == 6 -> "Sessions"
                            inChat -> title
                            screenPage ==
                                1 &&
                                project != null -> project!!.getString("name")
                            else -> "Codex"
                        }
                    Scaffold(
                        topBar = {
                            TopAppBar(title = {
                                Column {
                                    Text(
                                        heading,
                                        style =
                                            if (screenPage ==
                                                0
                                            ) {
                                                MaterialTheme.typography.headlineLarge
                                            } else {
                                                MaterialTheme.typography.titleLarge
                                            },
                                        maxLines = 1,
                                        overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                                    )
                                    if (!compactHeight &&
                                        selected != null &&
                                        screenPage != 5 &&
                                        screenPage != 0
                                    ) {
                                        Text(
                                            listOf(selected.name, session?.text("profile").orEmpty())
                                                .filter {
                                                    it.isNotBlank()
                                                }.joinToString(" · "),
                                            style = MaterialTheme.typography.labelMedium,
                                            maxLines = 1,
                                            overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                                        )
                                    }
                                }
                            }, navigationIcon = {
                                if (screenPage != 0 ||
                                    !persistentNavigation
                                ) {
                                    FilledTonalIconButton(onClick = {
                                        if (screenPage !=
                                            0
                                        ) {
                                            navigation.popBackStack()
                                        } else if (navigation.currentBackStackEntry ==
                                            screenEntry
                                        ) {
                                            scope.launch { drawer.open() }
                                        }
                                    }) {
                                        Icon(
                                            if (screenPage ==
                                                0
                                            ) {
                                                Icons.Default.Menu
                                            } else {
                                                Icons.AutoMirrored.Filled.ArrowBack
                                            },
                                            if (screenPage ==
                                                0
                                            ) {
                                                "Open navigation"
                                            } else {
                                                "Back"
                                            },
                                        )
                                    }
                                }
                            }, actions = {
                                IconButton(onClick = { keyboardHelp = true }) { Icon(Icons.Default.Keyboard, "Keyboard shortcuts") }
                                if (servers.isNotEmpty()) {
                                    IconButton(
                                        onClick = { switcher = true },
                                    ) { Icon(Icons.Default.SwapHoriz, "Switch server or active task") }
                                }
                                if (screenPage != 5) IconButton(onClick = { navigate(5) }) { Icon(Icons.Default.Settings, "Settings") }
                            }, expandedHeight = if (compactHeight) 48.dp else 64.dp)
                        },
                        snackbarHost = { SnackbarHost(snackbar) },
                        bottomBar = {
                            if (inChat &&
                                !keyboardVisible &&
                                compactHeight
                            ) {
                                SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp)) {
                                    chatLabels.forEachIndexed { index, label ->
                                        SegmentedButton(
                                            selected = screenPage == index + 2,
                                            onClick = { screenEntry.savedStateHandle[CHAT_TAB] = index + 2 },
                                            shape = SegmentedButtonDefaults.itemShape(index, chatLabels.size),
                                            icon = {},
                                            label = { Text(label, maxLines = 1) },
                                        )
                                    }
                                }
                            } else if (inChat && !wide && !keyboardVisible) {
                                NavigationBar {
                                    chatLabels.forEachIndexed { index, label ->
                                        NavigationBarItem(selected = screenPage == index + 2, onClick = {
                                            screenEntry.savedStateHandle[CHAT_TAB] =
                                                index + 2
                                        }, icon = { Icon(chatIcons[index], label) }, label = { Text(label) })
                                    }
                                }
                            }
                        },
                    ) { padding ->
                        Row(Modifier.padding(padding).fillMaxSize()) {
                            if (inChat && wide && !compactHeight) {
                                NavigationRail {
                                    chatLabels.forEachIndexed { index, label ->
                                        NavigationRailItem(selected = screenPage == index + 2, onClick = {
                                            screenEntry.savedStateHandle[CHAT_TAB] =
                                                index + 2
                                        }, icon = { Icon(chatIcons[index], label) }, label = { Text(label) })
                                    }
                                }
                            }
                            Column(Modifier.weight(1f).fillMaxHeight().padding(horizontal = 16.dp)) {
                                if (selected != null &&
                                    selected.status != "Connected"
                                ) {
                                    Text(
                                        if (selected.status ==
                                            "Connecting"
                                        ) {
                                            "Connecting to ${selected.name}…"
                                        } else {
                                            "${selected.name}: ${selected.status}. Retrying…"
                                        },
                                        Modifier.padding(vertical = 8.dp),
                                        style = MaterialTheme.typography.bodySmall,
                                    )
                                }
                                if (pendingTask != null) LinearProgressIndicator(Modifier.fillMaxWidth())
                                if (screenPage == 0 &&
                                    servers.isNotEmpty()
                                ) {
                                    androidx.compose.foundation.lazy.LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                        item {
                                            AssistChip(onClick = {
                                                switcher = true
                                            }, label = {
                                                Text(
                                                    "All servers",
                                                )
                                            }, leadingIcon = { Icon(Icons.Default.Dns, null, Modifier.size(16.dp)) })
                                        }
                                        items(servers.size) { index ->
                                            val host = servers[index]
                                            FilterChip(
                                                selected = host.id == server,
                                                onClick = {
                                                    pendingTask = null
                                                    server = host.id
                                                    project = null
                                                    session =
                                                        null
                                                    thread = ""
                                                },
                                                label = { Text(host.name, maxLines = 1) },
                                                leadingIcon = {
                                                    Icon(
                                                        Icons.Default.Circle,
                                                        if (host.status ==
                                                            "Connected"
                                                        ) {
                                                            "Connected"
                                                        } else {
                                                            "Connecting"
                                                        },
                                                        Modifier.size(8.dp),
                                                        tint =
                                                            if (host.status ==
                                                                "Connected"
                                                            ) {
                                                                MaterialTheme.colorScheme.primary
                                                            } else {
                                                                MaterialTheme.colorScheme.outline
                                                            },
                                                    )
                                                },
                                            )
                                        }
                                    }
                                }
                                CompositionLocalProvider(
                                    LocalKeyboardActions provides keyboardActions.takeIf { entry?.id == screenEntry.id },
                                ) {
                                    when {
                                        screenPage == 7 ->
                                            FeatureScreen(
                                                screenFeature,
                                                repo,
                                                server,
                                                effectiveSession,
                                                thread,
                                                { navigate(0) },
                                            )
                                        screenPage == 8 ->
                                            SearchScreen(repo, server, home.value) { recent ->
                                                session = recent.session
                                                project =
                                                    obj(
                                                        "name" to projectTitle(recent.session.text("cwd")),
                                                        "path" to recent.session.text("cwd"),
                                                        "id" to "",
                                                    )
                                                thread = recent.chat.text("id")
                                                title = chatTitle(recent.chat)
                                                navigate(2)
                                            }
                                        screenPage == 5 ->
                                            SettingsScreen(repo, server, monitor, { guide = true }, { navigate(6) }) {
                                                feature = it
                                                navigate(7)
                                            }
                                        server.isEmpty() ->
                                            EmptyServersScreen({
                                                invitation = ""
                                                addServer = true
                                            }, { navigate(5) })
                                        screenPage == 9 -> TerminalScreen(repo, server, session ?: JSONObject())
                                        screenPage == 0 ->
                                            ProjectsScreen(repo, server, selected?.status == "Connected", { recent ->
                                                session = recent.session
                                                project =
                                                    obj(
                                                        "name" to projectTitle(recent.session.getString("cwd")),
                                                        "path" to recent.session.getString("cwd"),
                                                        "id" to "",
                                                    )
                                                thread = recent.chat.getString("id")
                                                title = chatTitle(recent.chat)
                                                navigate(2)
                                            }) { chosen, running ->
                                                project = chosen
                                                session = running
                                                thread = ""
                                                navigate(1)
                                            }
                                        screenPage == 1 && project != null && session != null ->
                                            ProjectChatsScreen(repo, server, session!!) { chat ->
                                                thread =
                                                    chat.getString("id")
                                                title = chatTitle(chat)
                                                navigate(2)
                                            }
                                        inChat ->
                                            tabStates.SaveableStateProvider(screenPage) {
                                                when (screenPage) {
                                                    2 ->
                                                        ConversationScreen(
                                                            repo,
                                                            server,
                                                            session!!,
                                                            thread,
                                                            { thread = it },
                                                            { action = it },
                                                        )
                                                    3 -> DiffScreen(repo, server, session!!, thread)
                                                    4 -> WorkspaceScreen(repo, server, session!!)
                                                }
                                            }
                                        screenPage == 6 ->
                                            TasksScreen(repo, server, session, { chosen ->
                                                session = chosen
                                                project =
                                                    obj(
                                                        "name" to projectTitle(chosen.getString("cwd")),
                                                        "path" to chosen.getString("cwd"),
                                                        "id" to "",
                                                    )
                                            }) {
                                                thread = it
                                                navigate(2)
                                            }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if (keyboardHelp) KeyboardShortcutsDialog { keyboardHelp = false }
    if (switcher) {
        ServerSwitcher(repo, servers, server, { switcher = false }, { selectedServer ->
            pendingTask = null
            server = selectedServer
            project = null
            session = null
            thread = ""
            navigate(0)
            switcher = false
        }, { task ->
            pendingTask = null
            server = task.server.id
            session = task.session
            project =
                obj(
                    "name" to projectTitle(task.session.getString("cwd")),
                    "path" to task.session.getString("cwd"),
                    "id" to task.chat.text("projectId"),
                )
            thread = task.chat.getString("id")
            title = chatTitle(task.chat)
            navigate(if (thread.isEmpty()) 6 else 2)
            switcher = false
        }, {
            switcher = false
            invitation = ""
            addServer = true
        })
    }
    if (addServer) {
        AddServerDialog(repo, invitation) {
            addServer = false
            invitation = ""
        }
    }
    action?.let { method ->
        session?.let { chosen ->
            TaskControlSheet(method, repo, server, chosen, thread, {
                thread = it
                navigate(2)
            }, {
                navigate(1)
                thread =
                    ""
            }, { action = null })
        }
    }
}
