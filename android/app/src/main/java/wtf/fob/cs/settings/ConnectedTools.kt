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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.*
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import java.util.UUID
import kotlin.time.Duration.Companion.milliseconds

@Composable fun ConnectedAppsScreen(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
) {
    var added by remember { mutableStateOf(true) }
    var search by remember { mutableStateOf("") }
    var selected by remember { mutableStateOf<JSONObject?>(null) }
    val (query, state) =
        rememberRemoteData(
            repo,
            server,
            session,
            if (added) "app/installed" else "app/list",
            JSONObject().apply {
                if (thread.isNotEmpty()) put("threadId", thread)
            },
        )
    val apps =
        state.value
            ?.optJSONArray("data")
            ?.objects()
            .orEmpty()
            .filter { it.text("name").contains(search, true) }
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var busy by remember { mutableStateOf(false) }
    RefreshPage(repo, query, state) {
        SearchField(search, { search = it }, "Search apps", Modifier.padding(vertical = 12.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(added, { added = true }, label = { Text("Connected") })
            FilterChip(!added, {
                added =
                    false
            }, label = { Text("Discover") })
        }
        LazyColumn {
            if (apps.isEmpty() &&
                !state.loading
            ) {
                item {
                    EmptyState("Connect your apps", "Browse apps to bring your tools into Codex.", Icons.Default.Apps) {
                        if (added) {
                            FilledTonalButton(onClick = {
                                added =
                                    false
                            }) { Text("Discover apps") }
                        }
                    }
                }
            }
            items(apps, key = { it.text("id") }) { app ->
                SettingsGroup {
                    SettingsRow(app.text("name"), app.text("description"), Icons.Default.Apps, onClick = {
                        selected =
                            app
                    })
                }
            }
        }
    }
    selected?.let { app ->
        val (detailQuery, detail) =
            rememberRemoteData(
                repo,
                server,
                session,
                "app/read",
                obj(
                    "appIds" to JSONArray().put(app.text("id")),
                    "includeTools" to true,
                ).apply { if (thread.isNotEmpty()) put("threadId", thread) },
            )
        NativeSheet(app.text("name"), { selected = null }) {
            DataStatus(detail) { repo.data.refresh(detailQuery, true) }
            Text(app.text("description"), Modifier.padding(bottom = 20.dp))
            if (app.optBoolean("isAccessible")) {
                SettingsGroup {
                    SettingsRow("Use in Codex", "Allow this app in your workspace", trailing = {
                        Switch(app.optBoolean("isEnabled", true), enabled = !busy, onCheckedChange = { enabled ->
                            scope.launch {
                                busy = true
                                runCatching {
                                    repo.rpc(
                                        server,
                                        session,
                                        "config/value/write",
                                        obj(
                                            "keyPath" to "apps.${app.text("id")}.enabled",
                                            "value" to enabled,
                                            "mergeStrategy" to "replace",
                                        ),
                                    )
                                }.onSuccess {
                                    selected =
                                        JSONObject(app.toString()).put("isEnabled", enabled)
                                    repo.data.refresh(query, true)
                                }.onFailure(repo::report)
                                busy = false
                            }
                        })
                    })
                }
            } else {
                Button(enabled = app.text("installUrl").isNotEmpty(), modifier = Modifier.fillMaxWidth(), onClick = {
                    runCatching { openWebLink(context, app.text("installUrl")) }.onFailure(repo::report)
                }) { Text("Connect app") }
            }
        }
    }
}

/** External tools supply their own argument schema. Only that tool's inputs use a schema form. */
@Composable fun ConnectedToolSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    connection: String,
    tool: JSONObject,
    close: () -> Unit,
) {
    var arguments by remember { mutableStateOf<Any?>(JSONObject()) }
    var output by remember { mutableStateOf<JSONObject?>(null) }
    var busy by remember { mutableStateOf(false) }
    var failure by remember { mutableStateOf("") }
    var stream by remember { mutableStateOf(false) }
    val subscription = remember { UUID.randomUUID().toString() }
    val scope = rememberCoroutineScope()
    LaunchedEffect(stream) {
        if (!stream) return@LaunchedEffect
        val receive =
            launch(start = CoroutineStart.UNDISPATCHED) {
                repo.events.collect { event ->
                    val params = event.message.optJSONObject("params")
                    if (event.server == server &&
                        event.session == session &&
                        params?.text("subscriptionId") == subscription
                    ) {
                        output = params
                    }
                }
            }
        try {
            repo.rpc(
                server,
                session,
                "mcpServer/event/stream/start",
                obj(
                    "threadId" to thread,
                    "server" to connection,
                    "name" to tool.text("name"),
                    "arguments" to arguments,
                    "subscriptionId" to subscription,
                ),
            )
            awaitCancellation()
        } catch (
            cancelled: CancellationException,
        ) {
            throw cancelled
        } catch (
            error: Exception,
        ) {
            failure = error.message.orEmpty()
            stream = false
        } finally {
            receive.cancel()
            withContext(NonCancellable) {
                withTimeoutOrNull(3000.milliseconds) {
                    runCatching {
                        repo.rpc(
                            server,
                            session,
                            "mcpServer/event/stream/stop",
                            obj(
                                "subscriptionId" to subscription,
                            ),
                        )
                    }
                }
            }
        }
    }
    NativeSheet(tool.text("title", humanize(tool.text("name"))), close) {
        Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            Text(tool.text("description"))
            if (thread.isEmpty()) Text("Open a chat to use this tool.", color = MaterialTheme.colorScheme.onSurfaceVariant)
            tool.optJSONObject("inputSchema")?.let {
                SchemaField(repo.catalog, it, "Tool inputs", arguments, { value -> arguments = value })
            }
            if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
            output?.let { ValueView(it) }
        }
        Button(enabled = thread.isNotEmpty() && !busy && !stream, modifier = Modifier.fillMaxWidth(), onClick = {
            scope.launch {
                busy = true
                failure = ""
                try {
                    tool.optJSONObject("inputSchema")?.let { validateForm(repo.catalog, it, arguments) }
                    output =
                        repo.rpc(
                            server,
                            session,
                            "mcpServer/tool/call",
                            obj(
                                "threadId" to thread,
                                "server" to connection,
                                "tool" to tool.text("name"),
                                "arguments" to arguments,
                            ),
                        )
                } catch (
                    error: Exception,
                ) {
                    if (error is CancellationException) throw error
                    failure = error.message.orEmpty()
                } finally {
                    busy = false
                }
            }
        }) { Text(if (busy) "Running…" else "Use tool") }
        TextButton(enabled = thread.isNotEmpty() && !busy, onClick = {
            stream = !stream
        }) { Text(if (stream) "Stop live updates" else "Listen for live updates") }
    }
}

@Composable fun ResourceSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    connection: String,
    resource: JSONObject,
    close: () -> Unit,
) {
    val (query, state) =
        rememberRemoteData(
            repo,
            server,
            session,
            "mcpServer/resource/read",
            obj(
                "server" to connection,
                "uri" to resource.text("uri"),
            ).apply { if (thread.isNotEmpty()) put("threadId", thread) },
        )
    NativeSheet(resource.text("name", "Resource"), close) {
        DataStatus(state) { repo.data.refresh(query, true) }
        Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState())) {
            val contents =
                state.value?.optJSONArray("contents")?.objects()
                    ?: state.value
                        ?.optJSONObject("result")
                        ?.optJSONArray("contents")
                        ?.objects()
                        .orEmpty()
            contents.forEach { content ->
                if (content.has(
                        "text",
                    )
                ) {
                    Markdown(content.text("text"))
                } else {
                    Text("${content.text("mimeType", "File")} · ${content.text("uri")}")
                }
            }
        }
    }
}
