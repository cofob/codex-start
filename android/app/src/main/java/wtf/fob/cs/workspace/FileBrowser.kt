package wtf.fob.cs.workspace

import android.util.Base64
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.*
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.terminal.*
import wtf.fob.cs.ui.*
import java.util.UUID
import kotlin.time.Duration.Companion.milliseconds

fun childPath(
    directory: String,
    name: String,
): String {
    require(name.isNotBlank() && name !in setOf(".", "..") && '/' !in name && '\u0000' !in name) { "Enter a file name without a slash" }
    return directory.trimEnd('/') + "/" + name
}

@Composable fun FilesScreen(
    repo: RemoteRepository,
    server: String,
    session: JSONObject,
) {
    val sid = session.text("id")
    var directory by rememberSaveable(server, sid) { mutableStateOf(session.text("executionCwd")) }
    val (query, state) = rememberRemoteData(repo, server, sid, "fs/readDirectory", obj("path" to directory))
    val entries =
        state.value?.optJSONArray("entries")?.objects().orEmpty().sortedWith(
            compareByDescending<JSONObject> {
                it.optBoolean("isDirectory")
            }.thenBy { it.text("fileName", it.text("name")).lowercase() },
        )
    var selected by remember { mutableStateOf<JSONObject?>(null) }
    var file by remember(server, sid) { mutableStateOf("") }
    var content by remember(server, sid) { mutableStateOf("") }
    var original by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var folder by remember { mutableStateOf(false) }
    var folderName by remember { mutableStateOf("") }
    var search by rememberSaveable { mutableStateOf("") }
    var findFiles by remember { mutableStateOf(false) }
    var discard by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val context = LocalContext.current

    fun refresh() {
        repo.data.refresh(query, true)
    }

    fun open(path: String) {
        scope.launch {
            busy = true
            runCatching {
                val bytes = readRemotePreview(context, repo, server, sid, path, 1024 * 1024)
                require(bytes.none { it == 0.toByte() }) { "This is a binary file. Use Download to open it in another app." }
                original = bytes.toString(Charsets.UTF_8)
                content = original
                file = path
                selected = null
            }.onFailure(repo::report)
            busy = false
        }
    }
    val upload =
        rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
            if (uri != null) {
                scope.launch {
                    busy = true
                    runCatching {
                        withContext(Dispatchers.IO) {
                            val name =
                                context.contentResolver
                                    .query(
                                        uri,
                                        arrayOf(android.provider.OpenableColumns.DISPLAY_NAME),
                                        null,
                                        null,
                                        null,
                                    )?.use { cursor ->
                                        if (cursor.moveToFirst()) cursor.getString(0) else null
                                    }
                                    ?: "upload"
                            val destination = childPath(directory, name)
                            val temporary = java.io.File.createTempFile("upload-", ".data", context.cacheDir)
                            try {
                                context.contentResolver.openInputStream(uri)!!.use { input ->
                                    temporary.outputStream().use { output ->
                                        val buffer = ByteArray(256 * 1024)
                                        var size = 0L
                                        while (true) {
                                            val n = input.read(buffer)
                                            if (n <
                                                0
                                            ) {
                                                break
                                            }
                                            size += n
                                            require(size <= 1024L * 1024 * 1024) { "File exceeds 1 GiB" }
                                            output.write(buffer, 0, n)
                                        }
                                    }
                                }
                                repo.upload(server, sid, temporary, destination)
                            } finally {
                                temporary.delete()
                            }
                        }
                    }.onSuccess { refresh() }.onFailure(repo::report)
                    busy = false
                }
            }
        }
    var downloadPath by remember { mutableStateOf("") }
    val download =
        rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("application/octet-stream")) { uri ->
            if (uri !=
                null
            ) {
                scope.launch {
                    busy = true
                    runCatching {
                        val temporary = context.cacheDir.resolve("download-${UUID.randomUUID()}")
                        try {
                            repo.download(server, sid, downloadPath, temporary)
                            withContext(Dispatchers.IO) {
                                temporary.inputStream().use { input ->
                                    context.contentResolver.openOutputStream(uri)!!.use {
                                        input.copyTo(
                                            it,
                                            256 * 1024,
                                        )
                                    }
                                }
                            }
                        } finally {
                            temporary.delete()
                        }
                    }.onFailure(repo::report)
                    busy = false
                }
            }
        }
    LaunchedEffect(server, sid, directory) {
        val watch = UUID.randomUUID().toString()
        var watching = false
        try {
            runCatching {
                repo.rpc(server, sid, "fs/watch", obj("path" to directory, "watchId" to watch))
                watching = true
            }
            awaitCancellation()
        } finally {
            if (watching) {
                withContext(NonCancellable) {
                    withTimeoutOrNull(3000.milliseconds) {
                        runCatching {
                            repo.rpc(
                                server,
                                sid,
                                "fs/unwatch",
                                obj(
                                    "watchId" to watch,
                                ),
                            )
                        }
                    }
                }
            }
        }
    }
    androidx.activity.compose.BackHandler(file.isNotEmpty()) { if (content != original) discard = true else file = "" }
    if (file.isNotEmpty()) {
        Column(Modifier.fillMaxSize()) {
            Row(Modifier.fillMaxWidth(), verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
                IconButton(onClick = { if (content != original) discard = true else file = "" }) { Icon(Icons.Default.Close, "Close file") }
                Text(file.substringAfterLast('/'), Modifier.weight(1f), style = MaterialTheme.typography.titleMedium)
                TextButton(enabled = !busy && content != original, onClick = {
                    scope.launch {
                        busy = true
                        runCatching {
                            repo.rpc(
                                server,
                                sid,
                                "fs/writeFile",
                                obj(
                                    "path" to file,
                                    "dataBase64" to Base64.encodeToString(content.toByteArray(), Base64.NO_WRAP),
                                ),
                            )
                        }.onSuccess {
                            original = content
                            refresh()
                        }.onFailure(repo::report)
                        busy = false
                    }
                }) { Text("Save") }
            }
            OutlinedTextField(
                content,
                {
                    content = it
                },
                modifier =
                    Modifier
                        .weight(
                            1f,
                        ).fillMaxWidth()
                        .imePadding(),
                textStyle = LocalTextStyle.current.copy(fontFamily = FontFamily.Monospace),
                label = {
                    Text("File contents")
                },
            )
        }
    } else {
        RefreshPage(repo, query, state) {
            Row(verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
                IconButton(enabled = directory != "/", onClick = {
                    directory = directory.trimEnd('/').substringBeforeLast('/').ifEmpty { "/" }
                }) { Icon(Icons.Default.DriveFolderUpload, "Parent folder") }
                Text(projectTitle(directory), Modifier.weight(1f), style = MaterialTheme.typography.titleMedium, maxLines = 1)
                IconButton(onClick = { findFiles = true }) { Icon(Icons.Default.Search, "Search workspace files") }
                IconButton(enabled = !busy, onClick = { folder = true }) { Icon(Icons.Default.CreateNewFolder, "New folder") }
                IconButton(enabled = !busy, onClick = { upload.launch(arrayOf("*/*")) }) { Icon(Icons.Default.UploadFile, "Upload file") }
            }
            Text(directory, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 2)
            SearchField(search, { search = it }, "Find in this folder", Modifier.padding(vertical = 12.dp))
            if (busy) LinearProgressIndicator(Modifier.fillMaxWidth())
            LazyColumn {
                val visible = entries.filter { it.text("fileName", it.text("name")).contains(search, true) }
                if (visible.isEmpty() &&
                    !state.loading
                ) {
                    item { EmptyState("No files here", "Add a file or choose another folder.", Icons.Default.FolderOpen) }
                }
                items(visible, key = { it.text("fileName", it.text("name")) }) { entry ->
                    val name = entry.text("fileName", entry.text("name"))
                    val path = childPath(directory, name)
                    SettingsRow(
                        name,
                        icon = if (entry.optBoolean("isDirectory")) Icons.Default.Folder else Icons.Default.Description,
                        trailing = {
                            IconButton(
                                onClick = { selected = JSONObject(entry.toString()).put("path", path) },
                            ) { Icon(Icons.Default.MoreVert, "Options for $name") }
                        },
                        onClick = {
                            if (entry.optBoolean("isDirectory")) {
                                directory = path
                                search = ""
                            } else {
                                open(path)
                            }
                        },
                    )
                }
            }
        }
    }
    selected?.let { entry ->
        FileActionsSheet(repo, server, sid, entry, { selected = null }, { open(entry.text("path")) }, {
            downloadPath = entry.text("path")
            download.launch(downloadPath.substringAfterLast('/'))
        }, {
            selected = null
            refresh()
        })
    }
    if (findFiles) {
        WorkspaceFileSearchSheet(repo, server, sid, session.text("executionCwd"), { findFiles = false }) { path, isDirectory ->
            findFiles = false
            if (isDirectory) {
                directory = path
                search = ""
            } else {
                open(path)
            }
        }
    }
    if (folder) {
        NativeSheet("New folder", { folder = false }, scrollContent = true) {
            OutlinedTextField(folderName, { folderName = it }, label = { Text("Folder name") }, modifier = Modifier.fillMaxWidth())
            Button(enabled = folderName.isNotBlank() && !busy, modifier = Modifier.fillMaxWidth().padding(top = 16.dp), onClick = {
                scope.launch {
                    busy = true
                    runCatching {
                        repo.rpc(
                            server,
                            sid,
                            "fs/createDirectory",
                            obj(
                                "path" to childPath(directory, folderName.trim()),
                                "recursive" to false,
                            ),
                        )
                    }.onSuccess {
                        folder = false
                        folderName = ""
                        refresh()
                    }.onFailure(repo::report)
                    busy = false
                }
            }) { Text("Create folder") }
        }
    }
    if (discard) {
        AlertDialog(onDismissRequest = {
            discard = false
        }, title = { Text("Discard file changes?") }, text = { Text("The edits in this file have not been saved.") }, confirmButton = {
            TextButton(onClick = {
                discard =
                    false
                file = ""
            }) { Text("Discard") }
        }, dismissButton = { TextButton(onClick = { discard = false }) { Text("Keep editing") } })
    }
}

@Composable private fun WorkspaceFileSearchSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    root: String,
    close: () -> Unit,
    open: (String, Boolean) -> Unit,
) {
    var search by rememberSaveable { mutableStateOf("") }
    var term by remember { mutableStateOf("") }
    LaunchedEffect(search) {
        delay(300.milliseconds)
        term = search.trim()
    }
    val (query, state) =
        rememberRemoteData(
            repo,
            server,
            session,
            "fuzzyFileSearch",
            obj(
                "query" to term,
                "roots" to org.json.JSONArray().put(root),
            ),
            enabled = term.isNotEmpty(),
        )
    NativeSheet("Find a file", close) {
        SearchField(search, { search = it }, "Search the workspace", Modifier.padding(bottom = 12.dp))
        DataStatus(state) { repo.data.refresh(query, true) }
        LazyColumn(Modifier.weight(1f, fill = false)) {
            val files =
                state.value
                    ?.optJSONArray("files")
                    ?.objects()
                    .orEmpty()
            if (files.isEmpty() &&
                !state.loading
            ) {
                item { EmptyState("Search by file name", "Find files and folders in this workspace.", Icons.Default.Search) }
            }
            items(files) { file ->
                val path = file.text("path").let { if (it.startsWith('/')) it else file.text("root", root).trimEnd('/') + "/" + it }
                SettingsRow(
                    file.text("file_name"),
                    file.text("path"),
                    if (file.text("match_type") ==
                        "directory"
                    ) {
                        Icons.Default.Folder
                    } else {
                        Icons.Default.Description
                    },
                    onClick = {
                        open(
                            path,
                            file.text("match_type") == "directory",
                        )
                    },
                )
            }
        }
    }
}

@Composable private fun FileActionsSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    entry: JSONObject,
    close: () -> Unit,
    open: () -> Unit,
    download: () -> Unit,
    changed: () -> Unit,
) {
    val path = entry.text("path")
    val directory = entry.optBoolean("isDirectory")
    val (query, state) = rememberRemoteData(repo, server, session, "fs/getMetadata", obj("path" to path))
    var copy by remember { mutableStateOf(false) }
    var destination by remember { mutableStateOf("") }
    var delete by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    fun run(
        method: String,
        params: JSONObject,
    ) {
        scope.launch {
            busy = true
            runCatching { repo.rpc(server, session, method, params) }.onSuccess { changed() }.onFailure(repo::report)
            busy =
                false
        }
    }
    NativeSheet(path.substringAfterLast('/'), close) {
        DataStatus(state) { repo.data.refresh(query, true) }
        SettingsGroup {
            if (!directory) {
                SettingsRow("Open file", icon = Icons.Default.Edit, onClick = open)
                SettingsRow("Download", icon = Icons.Default.Download, onClick = download)
            }
            SettingsRow("Copy to…", icon = Icons.Default.ContentCopy, onClick = { copy = true })
            SettingsRow("Delete", icon = Icons.Default.DeleteOutline, onClick = { delete = true })
        }
        if (copy) {
            OutlinedTextField(destination, { destination = it }, label = { Text("Destination path") }, modifier = Modifier.fillMaxWidth())
            Button(enabled = !busy && destination.startsWith('/'), onClick = {
                run(
                    "fs/copy",
                    obj(
                        "sourcePath" to path,
                        "destinationPath" to destination,
                        "recursive" to directory,
                    ),
                )
            }) { Text("Copy") }
        }
    }
    if (delete) {
        AlertDialog(onDismissRequest = {
            delete = false
        }, title = {
            Text("Delete ${path.substringAfterLast('/')}?")
        }, text = {
            Text(if (directory) "This deletes the folder and all its contents." else "This permanently deletes the file.")
        }, confirmButton = {
            TextButton(enabled = !busy, onClick = {
                run(
                    "fs/remove",
                    obj(
                        "path" to path,
                        "recursive" to directory,
                        "force" to false,
                    ),
                )
            }) { Text("Delete") }
        }, dismissButton = {
            TextButton(onClick = {
                delete =
                    false
            }) { Text("Cancel") }
        })
    }
}
