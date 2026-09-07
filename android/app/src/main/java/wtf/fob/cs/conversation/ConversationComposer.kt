package wtf.fob.cs.conversation

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Close
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*

/** One continuous surface, including when a saved draft is read only. */
@Composable
internal fun ConversationComposer(
    value: TextFieldValue,
    changed: (TextFieldValue) -> Unit,
    enabled: Boolean,
    compact: Boolean,
    placeholder: String,
    modifier: Modifier = Modifier,
    actions: @Composable RowScope.() -> Unit,
    tools: @Composable RowScope.() -> Unit,
) {
    var toolsExpanded by rememberSaveable { mutableStateOf(false) }
    Surface(
        Modifier.fillMaxWidth().testTag("message-composer"),
        shape = RoundedCornerShape(28.dp),
        color = MaterialTheme.colorScheme.surfaceContainerLow,
        border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
    ) {
        Column(Modifier.padding(6.dp)) {
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                if (compact) {
                    IconButton(enabled = enabled, onClick = { toolsExpanded = !toolsExpanded }) {
                        Icon(
                            if (toolsExpanded) Icons.Default.Close else Icons.Default.Add,
                            if (toolsExpanded) "Hide message tools" else "Message tools",
                        )
                    }
                }
                BasicTextField(
                    value,
                    changed,
                    enabled = enabled,
                    modifier =
                        Modifier
                            .weight(1f)
                            .then(modifier)
                            .testTag("message-input")
                            .semantics { contentDescription = "Message" }
                            .heightIn(min = 48.dp)
                            .padding(horizontal = 12.dp, vertical = 12.dp),
                    textStyle =
                        MaterialTheme.typography.bodyLarge.copy(
                            color = if (enabled) MaterialTheme.colorScheme.onSurface else MaterialTheme.colorScheme.onSurfaceVariant,
                        ),
                    cursorBrush = SolidColor(MaterialTheme.colorScheme.primary),
                    minLines = 1,
                    maxLines = if (compact) 3 else 6,
                    decorationBox = { field ->
                        Box {
                            if (value.text.isEmpty()) {
                                Text(
                                    placeholder,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                            }
                            field()
                        }
                    },
                )
                actions()
            }
            // Keep voice controls alive when the keyboard changes the available height.
            Row(
                Modifier
                    .fillMaxWidth()
                    .then(
                        if (!compact ||
                            toolsExpanded
                        ) {
                            Modifier
                        } else {
                            Modifier.height(0.dp).focusProperties { canFocus = false }.clearAndSetSemantics {}
                        },
                    ).clipToBounds()
                    .horizontalScroll(rememberScrollState())
                    .testTag("message-tools"),
                verticalAlignment = Alignment.CenterVertically,
                content = tools,
            )
        }
    }
}
