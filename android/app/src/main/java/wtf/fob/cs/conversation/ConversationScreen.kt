package wtf.fob.cs.conversation

import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipboardManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.interaction.collectIsDraggedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.input.key.*
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import java.util.UUID
import kotlin.time.Duration.Companion.milliseconds

data class ConversationItem(
    val id: String,
    val type: String,
    val text: String,
    val details: JSONObject? = null,
    val detailsSize: Int =
        details?.toString()?.length
            ?: 0,
)

fun boundedConversation(
    items: List<ConversationItem>,
    keepLatest: Boolean = true,
): List<ConversationItem> {
    var remaining = 16 * 1024 * 1024
    val result = mutableListOf<ConversationItem>()
    val candidates = if (keepLatest) items.asReversed() else items
    for (item in candidates) {
        val size = item.text.length + item.detailsSize
        if (result.size == 1000 || size > remaining) break
        result += item
        remaining -= size
    }
    return if (keepLatest) result.asReversed() else result
}

fun conversationItem(item: JSONObject): ConversationItem {
    val type = item.text("type")
    val content =
        item.optJSONArray("content")?.objects()?.joinToString("\n") { part ->
            when (part.text("type")) {
                "text", "inputText" -> part.text("text")
                "image", "localImage" -> "Image: ${part.text("path", part.text("url"))}"
                else -> part.text("text", part.text("path"))
            }
        }
    val text =
        if (type == "reasoning") {
            listOf("summary", "content")
                .mapNotNull { item.optJSONArray(it) }
                .joinToString("\n\n") { parts ->
                    (0 until parts.length()).filterNot(parts::isNull).joinToString("\n") { index ->
                        parts.optJSONObject(index)?.text("text") ?: parts.optString(index)
                    }
                }.ifBlank { item.text("text") }
        } else {
            item.text("text", content ?: item.text("command", item.text("aggregatedOutput", item.text("summary"))))
        }
    return ConversationItem(item.text("id", UUID.randomUUID().toString()), type, text, item)
}

@SuppressLint("UnusedBoxWithConstraintsScope")
@Composable
@OptIn(ExperimentalLayoutApi::class)
fun ConversationScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
    thread: String,
    setThread: (String) -> Unit,
    action: (String) -> Unit,
) {
    val savedHistory = session.text("kind") == "history"
    var messages by remember(server, session, thread) { mutableStateOf<List<ConversationItem>>(emptyList()) }
    var approvals by remember(server, session) { mutableStateOf(repo.pendingApprovals(server, session.getString("id"))) }
    var input by rememberSaveable(server, session.text("id"), thread, stateSaver = TextFieldValue.Saver) {
        mutableStateOf(TextFieldValue(repo.draft(server, session.text("id"), thread)))
    }
    SideEffect { repo.keepDraft(server, session.text("id"), thread, input.text) }
    var activeTurn by remember(thread) { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var historyLoading by remember(thread) { mutableStateOf(thread.isNotEmpty()) }
    var historyCursor by remember(thread) { mutableStateOf("") }
    var attachments by remember(thread) { mutableStateOf<List<JSONObject>>(emptyList()) }
    var visible by remember(thread) { mutableIntStateOf(100) }
    var options by remember(server, session.text("id"), thread) { mutableStateOf(MessageOptions()) }
    var configure by remember { mutableStateOf(false) }
    var queueFollowUp by rememberSaveable { mutableStateOf(true) }
    var showQueue by rememberSaveable(server, thread) { mutableStateOf(false) }
    var showAgents by remember { mutableStateOf(false) }
    var parentTask by remember { mutableStateOf("") }
    var canWrite by remember(server, thread) { mutableStateOf(thread.isEmpty()) }
    var recovered by remember(server, thread) { mutableStateOf(false) }
    var historyError by remember(server, thread) { mutableStateOf("") }
    var accessMessage by remember(server, thread) { mutableStateOf("") }
    var writerSession by remember(server, thread) { mutableStateOf(session.getString("id")) }
    var loading by remember(server, thread) { mutableStateOf(false) }
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val scroll = rememberLazyListState()
    var followLatest by remember(server, session.text("id"), thread) { mutableStateOf(true) }
    var selectedAnswerId by remember(server, session.text("id"), thread) { mutableStateOf<String?>(null) }
    val selectingAnswer = selectedAnswerId != null
    var renderRevision by remember { mutableIntStateOf(0) }
    val userDragging by scroll.interactionSource.collectIsDraggedAsState()
    val showScrollToLatest by remember {
        derivedStateOf { !followLatest && scroll.canScrollForward }
    }
    val catalog = repo.catalog

    suspend fun scrollToLatest() {
        val lastItem = scroll.layoutInfo.totalItemsCount - 1
        if (lastItem < 0) return
        scroll.scrollToItem(lastItem)
        followLatest = true
    }

    suspend fun recover(force: Boolean = false) {
        suspend fun read(
            method: String,
            params: JSONObject,
        ) = repo.data.read(RemoteQuery.of(server, session.getString("id"), method, params), force)
        if (thread.isEmpty() || loading) return
        loading = true
        runCatching {
            val snapshot = read("thread/snapshot", obj("threadId" to thread))
            writerSession = snapshot.text("sessionId", session.getString("id"))
            recovered = snapshot.optBoolean("recovered")
            if (recovered) {
                canWrite = false
                accessMessage = snapshot.text("warning")
            }
            val turns =
                snapshot
                    .getJSONObject("turns")
                    .optJSONArray("data")
                    ?.objects()
                    .orEmpty()
            val result = snapshot.getJSONObject("items")
            historyCursor = result.text("nextCursor")
            messages =
                boundedConversation(
                    result
                        .getJSONArray("data")
                        .objects()
                        .reversed()
                        .map { conversationItem(it.getJSONObject("item")) },
                )
            activeTurn = turns.firstOrNull { it.text("status") == "inProgress" }?.text("id").orEmpty()
            historyError = ""
        }.onFailure {
            historyError = it.message ?: "Cannot load history"
            canWrite = false
        }
        approvals = repo.pendingApprovals(server, writerSession)
        loading = false
        historyLoading = false
    }

    suspend fun enableWriting() {
        writerSession =
            repo
                .rpc(
                    server,
                    session.getString("id"),
                    "thread/writeAccess",
                    obj("threadId" to thread),
                ).getString("sessionId")
        canWrite = true
        accessMessage = ""
        recover()
    }

    fun reportWriteAccessFailure(failure: Throwable) {
        accessMessage =
            if (failure.message.orEmpty().contains("active writer")) {
                "Another app is writing to this chat. Close it there, then enable writing here."
            } else {
                failure.message ?: "Cannot enable writing"
            }
    }
    val export =
        rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("text/markdown")) { uri ->
            if (uri !=
                null
            ) {
                scope.launch {
                    busy = true
                    runCatching { exportConversation(context, repo, server, session.getString("id"), thread, uri) }.onFailure(repo::report)
                    busy =
                        false
                }
            }
        }
    val exportRaw =
        rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("application/x-ndjson")) { uri ->
            if (uri !=
                null
            ) {
                scope.launch {
                    busy = true
                    runCatching {
                        exportConversationJson(
                            context,
                            repo,
                            server,
                            session.getString("id"),
                            thread,
                            uri,
                        )
                    }.onFailure(repo::report)
                    busy =
                        false
                }
            }
        }
    val attach =
        rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
            scope.launch {
                busy = true
                runCatching {
                    for (uri in uris.take(8 - attachments.size)) {
                        attachments =
                            attachments + uploadAttachment(context, repo, server, session.getString("id"), uri)
                    }
                }.onFailure(repo::report)
                busy = false
            }
        }
    LaunchedEffect(server, session.text("id"), thread) {
        recover()
        if (thread.isNotEmpty() && !recovered && historyError.isEmpty()) {
            busy = true
            runCatching { enableWriting() }.onFailure(::reportWriteAccessFailure)
            busy = false
        }
        while (true) {
            delay(3000.milliseconds)
            if (!canWrite) recover()
        }
    }
    LaunchedEffect(server, session.text("id"), thread, scroll) {
        snapshotFlow { scroll.canScrollForward }
            .distinctUntilChanged()
            .collect { canScrollForward ->
                if (!canScrollForward && selectedAnswerId == null && !scroll.isScrollInProgress) followLatest = true
            }
    }
    LaunchedEffect(server, session.text("id"), thread, userDragging) {
        if (userDragging) {
            snapshotFlow { scroll.canScrollForward }
                .distinctUntilChanged()
                .collect { canScrollForward ->
                    followLatest = !canScrollForward
                }
        }
    }
    val latestMessage = messages.lastOrNull()
    LaunchedEffect(
        historyLoading,
        messages.size,
        latestMessage?.id,
        latestMessage?.text?.length,
        latestMessage?.detailsSize,
        approvals.size,
        accessMessage.length,
        historyError.length,
        renderRevision,
    ) {
        if (!historyLoading && followLatest && !selectingAnswer) {
            withFrameNanos { }
            // A touch or native selection can start while waiting for the Markdown layout.
            if (followLatest && selectedAnswerId == null && !scroll.isScrollInProgress) scrollToLatest()
        }
    }
    LaunchedEffect(server, session, thread) {
        val deltas = mutableListOf<RemoteEvent>()
        var deltaSize = 0
        var publish: Job? = null

        fun flushDeltas() {
            publish?.cancel()
            publish = null
            if (deltas.isNotEmpty()) messages = applyConversationDeltas(messages, deltas)
            deltas.clear()
            deltaSize = 0
        }
        repo.events.collect { event ->
            if (event.server != server) return@collect
            if (event.message.text("method") == "connection/reset") {
                flushDeltas()
                recover(true)
                return@collect
            }
            if (event.session != writerSession) return@collect
            approvals = repo.pendingApprovals(server, event.session)
            val message = event.message
            val params = message.optJSONObject("params") ?: return@collect
            if (params.text("threadId").isNotEmpty() && params.text("threadId") != thread) return@collect
            if (message.text("method") in CONVERSATION_DELTAS) {
                deltas.add(event)
                deltaSize += params.text("delta").length
                if (deltaSize >= 32 * 1024 || deltas.size >= 256) {
                    flushDeltas()
                } else if (publish == null) {
                    publish =
                        launch {
                            delay(32)
                            flushDeltas()
                        }
                }
                return@collect
            }
            // A final item or recovery snapshot must never be followed by stale buffered text.
            flushDeltas()
            when (message.text("method")) {
                "turn/started" -> activeTurn = params.optJSONObject("turn")?.text("id").orEmpty()
                "turn/completed" -> {
                    activeTurn = ""
                    recover(true)
                }
                "item/started", "item/completed" ->
                    params.optJSONObject("item")?.let { item ->
                        val entry = conversationItem(item)
                        val existing = messages.indexOfFirst { it.id == entry.id }
                        messages =
                            boundedConversation(
                                if (existing <
                                    0
                                ) {
                                    messages + entry
                                } else {
                                    messages.toMutableList().apply { set(existing, entry) }
                                },
                            )
                    }
                "turn/plan/updated" -> {
                    val id = "plan/${params.text("turnId")}"
                    messages = boundedConversation(messages.filterNot { it.id == id } + ConversationItem(id, "plan", "", params))
                }
            }
        }
    }
    val messageFocus = remember { FocusRequester() }
    val focusManager = LocalFocusManager.current
    val canSend = canWrite && (input.text.isNotBlank() || attachments.isNotEmpty()) && !busy && !historyLoading

    fun sendMessage() {
        if (!canSend || busy) return
        // Set the guard before launching, so repeated input cannot submit twice.
        busy = true
        scope.launch {
            val text = input.text
            runCatching {
                val id =
                    thread.ifEmpty {
                        repo
                            .rpc(
                                server,
                                session.getString("id"),
                                "thread/start",
                                obj(
                                    "cwd" to session.getString("executionCwd"),
                                    "historyMode" to "paginated",
                                ),
                            ).getJSONObject("thread")
                            .getString("id")
                    }
                if (id != thread) setThread(id)
                val params =
                    obj(
                        "threadId" to id,
                        "input" to
                            JSONArray(
                                attachments.map {
                                    JSONObject(it.toString()).apply {
                                        if (text("type") in
                                            setOf("localImage", "localAudio")
                                        ) {
                                            remove("name")
                                        }
                                    }
                                },
                            ).apply {
                                if (text.isNotBlank()) {
                                    put(
                                        obj(
                                            "type" to "text",
                                            "text" to text,
                                        ),
                                    )
                                }
                            },
                        "clientUserMessageId" to UUID.randomUUID().toString(),
                    )
                if (activeTurn.isEmpty()) {
                    options.parameters().let { settings -> settings.keys().forEach { key -> params.put(key, settings.get(key)) } }
                } else if (!queueFollowUp) {
                    params.put("expectedTurnId", activeTurn)
                }
                repo.rpc(
                    server,
                    session.getString("id"),
                    if (activeTurn.isEmpty()) {
                        "turn/start"
                    } else if (queueFollowUp) {
                        "thread/queue/add"
                    } else {
                        "turn/steer"
                    },
                    params,
                )
                input = TextFieldValue()
                attachments = emptyList()
            }.onFailure(repo::report)
            busy = false
        }
    }
    KeyboardAction(KeyboardCommand.FocusMessage) { if (canWrite && !configure && !showAgents) messageFocus.requestFocus() }
    BoxWithConstraints(Modifier.fillMaxSize()) {
        val compact = maxHeight < 420.dp
        Column(Modifier.fillMaxSize()) {
            Row(
                Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(Modifier.weight(1f).then(if (historyLoading) Modifier else Modifier.testTag("conversation-ready"))) {
                    val needsInput = approvals.any { it.message.optJSONObject("params")?.text("threadId") == thread }
                    TaskStatus(
                        if (historyError.isNotEmpty()) {
                            "History unavailable"
                        } else if (needsInput) {
                            "Needs your input"
                        } else if (!canWrite) {
                            "Read only"
                        } else if (activeTurn.isEmpty()) {
                            "Ready"
                        } else {
                            "Working"
                        },
                        attention = historyError.isNotEmpty() || needsInput,
                        active = activeTurn.isNotEmpty() && !needsInput && historyError.isEmpty(),
                    )
                }
                if (thread.isNotEmpty() && !savedHistory) {
                    if (canWrite) {
                        TextButton(onClick = { canWrite = false }) { Text("Read only") }
                    } else {
                        TextButton(enabled = !busy && !historyLoading && !recovered && historyError.isEmpty(), onClick = {
                            scope.launch {
                                busy = true
                                runCatching { enableWriting() }.onFailure(::reportWriteAccessFailure)
                                busy = false
                            }
                        }) { Text("Enable writing") }
                    }
                }
                if (thread.isNotEmpty()) {
                    var menu by remember { mutableStateOf(false) }
                    Box {
                        IconButton(onClick = { menu = true }) { Icon(Icons.Default.MoreHoriz, "Task actions") }
                        DropdownMenu(menu, { menu = false }) {
                            taskActionLabels.filter { !savedHistory }.forEach { (method, label) ->
                                DropdownMenuItem(
                                    enabled =
                                        canWrite ||
                                            method in
                                            setOf(
                                                "thread/searchOccurrences",
                                                "thread/timeline/list",
                                                "thread/queue/list",
                                            ),
                                    text = {
                                        Text(label)
                                    },
                                    onClick = {
                                        menu =
                                            false
                                        if (method == "thread/queue/list") showQueue = true else action(method)
                                    },
                                )
                            }
                            DropdownMenuItem(text = { Text("Export conversation") }, onClick = {
                                menu = false
                                export.launch("codex-$thread.md")
                            })
                            DropdownMenuItem(text = { Text("Export protocol history") }, onClick = {
                                menu = false
                                exportRaw.launch("codex-$thread.jsonl")
                            })
                            DropdownMenuItem(enabled = !savedHistory, text = { Text("Agents and subagents") }, onClick = {
                                menu = false
                                showAgents = true
                            })
                            DropdownMenuItem(enabled = canWrite, text = { Text("Start a side conversation") }, onClick = {
                                menu = false
                                scope.launch {
                                    runCatching {
                                        val fork =
                                            repo.rpc(
                                                server,
                                                session.getString("id"),
                                                "thread/fork",
                                                obj(
                                                    "threadId" to thread,
                                                    "ephemeral" to true,
                                                    "excludeTurns" to true,
                                                ),
                                            )
                                        parentTask = thread
                                        setThread(fork.getJSONObject("thread").getString("id"))
                                    }.onFailure(repo::report)
                                }
                            })
                            if (parentTask.isNotEmpty()) {
                                DropdownMenuItem(text = { Text("Return to parent task") }, onClick = {
                                    menu = false
                                    setThread(parentTask)
                                    parentTask =
                                        ""
                                })
                            }
                            DropdownMenuItem(text = { Text("Prepare repository instructions") }, onClick = {
                                menu = false
                                input =
                                    TextFieldValue(
                                        "Inspect this repository and create or update AGENTS.md with concise instructions for future coding tasks. Describe the project, build and test commands, code rules, and relevant constraints.",
                                    )
                            })
                            DropdownMenuItem(text = { Text("Prepare a conversation recap") }, onClick = {
                                menu = false
                                input =
                                    TextFieldValue(
                                        "Summarize this conversation, including decisions, changes, test results, and unfinished work.",
                                    )
                            })
                            DropdownMenuItem(text = { Text("Copy host transcript path") }, onClick = {
                                menu = false
                                scope.launch {
                                    runCatching {
                                        val path =
                                            repo
                                                .rpc(
                                                    server,
                                                    session.getString("id"),
                                                    "thread/read",
                                                    obj(
                                                        "threadId" to thread,
                                                        "includeTurns" to false,
                                                    ),
                                                ).getJSONObject("thread")
                                                .text("path")
                                        require(path.isNotEmpty()) { "This task has no persisted host transcript" }
                                        context
                                            .getSystemService(
                                                ClipboardManager::class.java,
                                            ).setPrimaryClip(ClipData.newPlainText("Host transcript", path))
                                    }.onFailure(repo::report)
                                }
                            })
                        }
                    }
                    IconButton(
                        onClick = { scope.launch { recover() } },
                    ) { Icon(Icons.Default.Refresh, "Reload conversation", Modifier.size(20.dp)) }
                }
            }
            if (loading && messages.isEmpty()) LinearProgressIndicator(Modifier.fillMaxWidth())
            Box(Modifier.weight(1f)) {
                LazyColumn(
                    state = scroll,
                    modifier = Modifier.fillMaxSize().testTag("conversation-messages"),
                    verticalArrangement = Arrangement.spacedBy(10.dp),
                    contentPadding = PaddingValues(vertical = if (compact) 4.dp else 12.dp),
                ) {
                    if (accessMessage.isNotEmpty()) {
                        item {
                            Text(
                                accessMessage,
                                Modifier.padding(8.dp),
                                style = MaterialTheme.typography.bodySmall,
                            )
                        }
                    }
                    if (historyError.isNotEmpty()) {
                        item {
                            Text(
                                historyError,
                                Modifier.padding(8.dp),
                                color = MaterialTheme.colorScheme.error,
                            )
                        }
                    }
                    if (historyCursor.isNotEmpty()) {
                        item {
                            TextButton(onClick = {
                                scope.launch {
                                    runCatching {
                                        val page =
                                            repo.rpc(
                                                server,
                                                session.getString("id"),
                                                "thread/items/list",
                                                obj(
                                                    "threadId" to thread,
                                                    "limit" to 100,
                                                    "sortDirection" to "desc",
                                                    "cursor" to historyCursor,
                                                ),
                                            )
                                        val earlier =
                                            page
                                                .getJSONArray("data")
                                                .objects()
                                                .reversed()
                                                .map { conversationItem(it.getJSONObject("item")) }
                                        followLatest = false
                                        messages = boundedConversation(earlier + messages, keepLatest = false)
                                        visible += earlier.size
                                        historyCursor =
                                            page.text("nextCursor")
                                    }.onFailure(repo::report)
                                }
                            }) { Text("Load earlier history") }
                        }
                    }
                    if (messages.size > visible) {
                        item {
                            TextButton(onClick = {
                                followLatest = false
                                visible += 100
                            }) { Text("Load earlier messages") }
                        }
                    }
                    if (messages.isEmpty() && activeTurn.isEmpty() && historyError.isEmpty() && !loading) {
                        item {
                            Column(
                                Modifier.fillMaxWidth().padding(vertical = if (compact) 12.dp else 48.dp),
                                horizontalAlignment = Alignment.CenterHorizontally,
                            ) {
                                if (!compact) {
                                    Icon(
                                        Icons.Default.AutoAwesome,
                                        null,
                                        Modifier.size(28.dp),
                                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                                    )
                                }
                                Text(
                                    if (savedHistory) "This saved chat has no messages." else "What would you like to work on?",
                                    Modifier.padding(top = if (compact) 0.dp else 16.dp),
                                    style = if (compact) MaterialTheme.typography.titleMedium else MaterialTheme.typography.titleLarge,
                                )
                                Text(
                                    projectTitle(session.getString("cwd")),
                                    Modifier.padding(top = 8.dp),
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                    style = MaterialTheme.typography.bodyMedium,
                                )
                            }
                        }
                    }
                    val blocks = conversationBlocks(messages.takeLast(visible))
                    items(blocks, key = { it.id }) { block ->
                        if (block.activity) {
                            ConversationActivity(block, activeTurn.isNotEmpty() && block.id == blocks.lastOrNull()?.id) { text ->
                                context
                                    .getSystemService(
                                        ClipboardManager::class.java,
                                    ).setPrimaryClip(ClipData.newPlainText("Codex activity", text))
                            }
                        } else {
                            val item = block.items.first()
                            ConversationMessage(
                                item,
                                repo,
                                server,
                                session.getString("id"),
                                streaming = activeTurn.isNotEmpty() && item.id == messages.lastOrNull()?.id,
                                selectionChanged = { selecting ->
                                    if (selecting) {
                                        selectedAnswerId = item.id
                                    } else if (selectedAnswerId == item.id) {
                                        selectedAnswerId = null
                                    }
                                    if (selecting) followLatest = false
                                },
                                rendered = { renderRevision++ },
                            ) {
                                context
                                    .getSystemService(
                                        ClipboardManager::class.java,
                                    ).setPrimaryClip(ClipData.newPlainText("Codex", item.text))
                            }
                        }
                    }
                    if (canWrite) {
                        items(
                            approvals.filter {
                                it.message.optJSONObject("params")?.text("threadId", thread) == thread
                            },
                            key = { "approval/${it.message.get("id")}" },
                        ) { event ->
                            ApprovalCard(repo, event, catalog) {
                                approvals =
                                    repo.pendingApprovals(server, session.getString("id"))
                            }
                        }
                    }
                    item(key = "conversation-tail") { Spacer(Modifier.height(1.dp)) }
                }
                if (showScrollToLatest) {
                    SmallFloatingActionButton(
                        onClick = { scope.launch { scrollToLatest() } },
                        modifier = Modifier.align(Alignment.BottomEnd).padding(12.dp),
                    ) {
                        Icon(Icons.Default.KeyboardArrowDown, "Scroll to latest message")
                    }
                }
            }
            if (busy || historyLoading) LinearProgressIndicator(Modifier.fillMaxWidth())
            if (!savedHistory) {
                androidx.compose.foundation.lazy.LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    items(attachments) { attachment ->
                        InputChip(selected = true, onClick = {
                            attachments =
                                attachments.filterNot { it === attachment }
                        }, label = {
                            Text(attachment.text("name", attachment.text("path").substringAfterLast('/')), maxLines = 1)
                        }, trailingIcon = { Icon(Icons.Default.Close, "Remove attachment", Modifier.size(18.dp)) })
                    }
                }
                if (thread.isNotEmpty()) {
                    ConversationQueue(repo, server, writerSession, thread, canWrite, showQueue, { showQueue = true }, { showQueue = false })
                }
                ConversationComposer(
                    input,
                    { input = it },
                    enabled = canWrite,
                    compact = compact,
                    placeholder =
                        if (!canWrite) {
                            "Enable writing to reply"
                        } else if (activeTurn.isEmpty()) {
                            "Message Codex…"
                        } else if (queueFollowUp) {
                            "Queue your next instruction…"
                        } else {
                            "Steer this turn…"
                        },
                    modifier =
                        Modifier.focusRequester(messageFocus).keyboardTabNavigation(focusManager).onPreviewKeyEvent { event ->
                            if ((event.key == Key.Enter || event.key == Key.NumPadEnter) &&
                                event.isShiftPressed &&
                                !event.isCtrlPressed &&
                                !event.isAltPressed &&
                                !event.isMetaPressed
                            ) {
                                event.runShortcut {
                                    val start = input.selection.min
                                    input = TextFieldValue(input.text.replaceRange(start, input.selection.max, "\n"), TextRange(start + 1))
                                }
                            } else {
                                when (event.command()) {
                                    KeyboardCommand.Send -> event.runShortcut { sendMessage() }
                                    KeyboardCommand.Attach ->
                                        event.runShortcut {
                                            if (canWrite &&
                                                !busy &&
                                                attachments.size < 8
                                            ) {
                                                attach.launch(arrayOf("*/*"))
                                            }
                                        }
                                    else -> false
                                }
                            }
                        },
                    actions = {
                        if (canWrite &&
                            activeTurn.isNotEmpty()
                        ) {
                            FilledTonalIconButton(onClick = {
                                scope.launch {
                                    runCatching {
                                        repo.rpc(
                                            server,
                                            session.getString("id"),
                                            "turn/interrupt",
                                            obj(
                                                "threadId" to thread,
                                                "turnId" to activeTurn,
                                            ),
                                        )
                                    }.onFailure(repo::report)
                                }
                            }) { Icon(Icons.Default.Stop, "Interrupt task", Modifier.size(20.dp)) }
                        }
                        FilledIconButton(enabled = canSend, onClick = ::sendMessage) {
                            Icon(
                                Icons.Default.ArrowUpward,
                                if (activeTurn.isEmpty()) {
                                    "Send message"
                                } else if (queueFollowUp) {
                                    "Queue message"
                                } else {
                                    "Steer task"
                                },
                            )
                        }
                    },
                ) {
                    IconButton(onClick = {
                        attach.launch(arrayOf("*/*"))
                    }, enabled = canWrite && !busy && attachments.size < 8) { Icon(Icons.Default.Add, "Add attachment") }
                    IconButton(
                        enabled = canWrite,
                        onClick = { configure = true },
                    ) { Icon(Icons.Default.Tune, "Message options", Modifier.size(20.dp)) }
                    if (activeTurn.isNotEmpty()) {
                        TextButton(onClick = {
                            queueFollowUp = !queueFollowUp
                        }) { Text(if (queueFollowUp) "Queue" else "Steer", style = MaterialTheme.typography.labelLarge) }
                    } else if (options.effort.isNotEmpty()) {
                        Text(humanize(options.effort), style = MaterialTheme.typography.labelMedium)
                    }
                    if (canWrite && thread.isNotEmpty()) AudioButton(repo, server, session.getString("id"), thread)
                }
            }
            Spacer(Modifier.height(if (compact) 4.dp else 8.dp))
        }
    }
    if (configure) ModelOptionsSheet(repo, server, session.text("id"), thread, options, { options = it }, { configure = false }, activeTurn)
    if (showAgents) AgentTasksDialog(repo, server, session.getString("id"), thread, setThread, { showAgents = false })
}

@Composable fun Markdown(
    text: String,
    size: Float = 16f,
    onLongClick: (() -> Unit)? = null,
    selectable: Boolean = false,
    selectionChanged: (Boolean) -> Unit = {},
    rendered: () -> Unit = {},
) {
    val color = MaterialTheme.colorScheme.onSurface
    val sizePx = with(LocalDensity.current) { size.sp.toPx() }
    AndroidView(factory = {
        ChatMarkdownView(it).apply {
            setTextIsSelectable(selectable)
            setTextSize(android.util.TypedValue.COMPLEX_UNIT_PX, sizePx)
        }
    }, onRelease = { it.close() }, update = {
        it.setTextSize(android.util.TypedValue.COMPLEX_UNIT_PX, sizePx)
        it.selectionChanged = selectionChanged
        it.renderChanged = rendered
        it.setOnLongClickListener(
            if (onLongClick == null) {
                null
            } else {
                android.view.View.OnLongClickListener {
                    onLongClick()
                    true
                }
            },
        )
        it.isLongClickable = selectable || onLongClick != null
        it.setTextColor(
            android.graphics.Color.argb(
                (color.alpha * 255).toInt(),
                (color.red * 255).toInt(),
                (color.green * 255).toInt(),
                (
                    color.blue *
                        255
                ).toInt(),
            ),
        )
        it.submit(text.take(512 * 1024))
    }, modifier = Modifier.fillMaxWidth().clipToBounds())
}
