package wtf.fob.cs.conversation

import android.content.ClipData
import android.content.ClipboardManager
import android.widget.ScrollView
import android.widget.TextView
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.viewinterop.AndroidView
import io.noties.markwon.Markwon
import wtf.fob.cs.ui.NativeSheet

/** Native selection stays outside the chat list so it cannot consume chat drags. */
@Composable
fun AnswerSelection(
    text: String,
    close: () -> Unit,
) {
    val context = LocalContext.current
    val color = MaterialTheme.colorScheme.onSurface.toArgb()
    NativeSheet("Select text", close) {
        Text("Touch and hold the answer, then move the selection handles.", style = MaterialTheme.typography.bodySmall)
        AndroidView(
            factory = { context ->
                ScrollView(context).apply {
                    addView(
                        TextView(context).apply {
                            textSize = 16f
                            setTextColor(color)
                            setTextIsSelectable(true)
                            Markwon.create(context).setMarkdown(this, text.take(512 * 1024))
                        },
                    )
                }
            },
            update = { (it.getChildAt(0) as TextView).setTextColor(color) },
            modifier =
                Modifier
                    .fillMaxWidth()
                    .weight(1f, fill = false)
                    .clipToBounds()
                    .testTag("answer-selection"),
        )
        TextButton(onClick = {
            context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("Agent answer", text))
        }) { Text("Copy full answer") }
    }
}
