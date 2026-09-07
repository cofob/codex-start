package wtf.fob.cs.data

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File
import java.util.UUID

suspend fun readRemotePreview(
    context: Context,
    repo: RemoteRepository,
    server: String,
    session: String,
    source: String,
    maximumBytes: Long,
): ByteArray =
    withContext(Dispatchers.IO) {
        val file = File(context.cacheDir, "preview-${UUID.randomUUID()}")
        try {
            repo.download(server, session, source, file, maximumBytes)
            require(file.length() <= maximumBytes) { "File exceeds the preview limit" }
            file.readBytes()
        } finally {
            file.delete()
        }
    }
