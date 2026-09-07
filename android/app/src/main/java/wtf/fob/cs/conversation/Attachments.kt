package wtf.fob.cs.conversation

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.provider.OpenableColumns
import android.util.Base64
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import java.io.File
import java.util.UUID

suspend fun uploadAttachment(
    context: Context,
    repo: RemoteRepository,
    server: String,
    session: String,
    uri: Uri,
): JSONObject =
    withContext(Dispatchers.IO) {
        val name =
            context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) cursor.getString(0) else null
            }
                ?: "attachment"
        val safeName =
            name
                .substringAfterLast('/')
                .replace(Regex("[^\\p{L}\\p{N}._-]"), "_")
                .take(128)
                .ifEmpty { "attachment" }
        val mime = context.contentResolver.getType(uri).orEmpty()
        val folder = "/home/codex/.local/state/codex-start/attachments/${UUID.randomUUID()}"
        val path = "$folder/$safeName"
        val local = File.createTempFile("attachment-", ".data", context.cacheDir)
        try {
            context.contentResolver.openInputStream(uri)!!.use { input ->
                local.outputStream().use { output ->
                    val buffer = ByteArray(256 * 1024)
                    var length = 0L
                    while (true) {
                        val n = input.read(buffer)
                        if (n <
                            0
                        ) {
                            break
                        }
                        length += n
                        require(length <= 1024L * 1024 * 1024) { "Attachment exceeds 1 GiB" }
                        output.write(buffer, 0, n)
                    }
                }
            }
            repo.rpc(server, session, "fs/createDirectory", obj("path" to folder))
            repo.upload(server, session, local, path)
            obj(
                "type" to
                    when {
                        mime.startsWith("image/") -> "localImage"
                        mime.startsWith("audio/") -> "localAudio"
                        else -> "mention"
                    },
                "path" to path,
                "name" to safeName,
            )
        } finally {
            local.delete()
        }
    }

private fun imageSources(item: JSONObject): List<String> {
    val sources = mutableListOf<String>()
    when (item.text("type")) {
        "imageView" -> sources += item.text("path")
        "imageGeneration" ->
            sources +=
                item.text("savedPath").ifEmpty {
                    item
                        .text("result")
                        .takeIf { it.isNotEmpty() }
                        ?.let { "data:image/png;base64,$it" }
                        .orEmpty()
                }
    }
    val content = item.optJSONArray("content") ?: item.optJSONArray("contentItems") ?: item.optJSONObject("result")?.optJSONArray("content")
    content?.objects()?.forEach { part ->
        when (part.text("type")) {
            "localImage" -> sources += part.text("path")
            "image", "inputImage" ->
                sources +=
                    part.text("url", part.text("imageUrl", part.text("image_url"))).ifEmpty {
                        part
                            .text("data")
                            .takeIf { it.isNotEmpty() }
                            ?.let { "data:image/png;base64,$it" }
                            .orEmpty()
                    }
        }
    }
    return sources.filter(String::isNotEmpty).take(8)
}

@Composable fun ConversationImages(
    repo: RemoteRepository,
    server: String,
    session: String,
    item: JSONObject,
) {
    val context = LocalContext.current
    val sources = remember(item.toString()) { imageSources(item) }
    sources.forEach { source ->
        var bitmap by remember(source) { mutableStateOf<Bitmap?>(null) }
        var error by remember(source) { mutableStateOf("") }
        LaunchedEffect(source) {
            runCatching {
                val bytes =
                    when {
                        source.startsWith("data:image/") -> {
                            val encoded = source.substringAfter("base64,", "")
                            require(encoded.length <= 8 * 1024 * 1024) { "Image is too large for an inline preview" }
                            withContext(Dispatchers.IO) { Base64.decode(encoded, Base64.DEFAULT) }
                        }
                        source.startsWith('/') -> readRemotePreview(context, repo, server, session, source, 8L * 1024 * 1024)
                        else -> error("This image URL requires a host integration. Open the tool details to inspect it.")
                    }
                bitmap =
                    withContext(Dispatchers.IO) {
                        val options = BitmapFactory.Options().apply { inJustDecodeBounds = true }
                        BitmapFactory.decodeByteArray(bytes, 0, bytes.size, options)
                        require(options.outWidth > 0 && options.outHeight > 0) { "Unsupported image format" }
                        var sample = 1
                        while (maxOf(options.outWidth, options.outHeight) / sample > 1600) sample *= 2
                        BitmapFactory.decodeByteArray(bytes, 0, bytes.size, BitmapFactory.Options().apply { inSampleSize = sample })
                    }
            }.onFailure { error = it.message.orEmpty() }
        }
        bitmap?.let { Image(it.asImageBitmap(), "Conversation image", Modifier.fillMaxWidth().heightIn(max = 400.dp)) }
        if (bitmap == null && error.isEmpty()) LinearProgressIndicator(Modifier.fillMaxWidth())
        if (error.isNotEmpty()) Text(error, style = MaterialTheme.typography.bodySmall)
    }
}

suspend fun exportConversation(
    context: Context,
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    destination: Uri,
) = withContext(Dispatchers.IO) {
    context.contentResolver.openOutputStream(destination)!!.bufferedWriter().use { output ->
        var cursor: String? = null
        do {
            val page =
                repo.rpc(
                    server,
                    session,
                    "thread/items/list",
                    obj(
                        "threadId" to thread,
                        "limit" to 50,
                        "sortDirection" to "asc",
                        "cursor" to cursor,
                    ),
                )
            for (entry in page.getJSONArray("data").objects()) {
                val item = conversationItem(entry.getJSONObject("item"))
                output
                    .append("## ")
                    .append(humanize(item.type))
                    .append("\n\n")
                    .append(item.text)
                    .append("\n\n")
            }
            cursor = page.text("nextCursor").takeIf(String::isNotEmpty)
        } while (cursor != null)
    }
}
