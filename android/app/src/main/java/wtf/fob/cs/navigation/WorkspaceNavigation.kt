package wtf.fob.cs.navigation

import androidx.compose.foundation.layout.Box
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Chat
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*

/** Replaces the phone's bottom tabs without adding another row of controls. */
@Composable
internal fun WorkspaceViewMenu(
    page: Int,
    terminals: Boolean,
    navigate: (Int) -> Unit,
    keyboardHelp: () -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { expanded = true }) { Icon(Icons.Default.SpaceDashboard, "Workspace views") }
        DropdownMenu(expanded, onDismissRequest = { expanded = false }) {
            val destinations = listOf(2 to "Chat", 3 to "Changes", 4 to "Files") + if (terminals) listOf(9 to "Terminals") else emptyList()
            destinations.forEach { (destination, label) ->
                DropdownMenuItem(
                    text = { Text(label) },
                    leadingIcon = {
                        Icon(
                            when (destination) {
                                2 -> Icons.AutoMirrored.Filled.Chat
                                3 -> Icons.Default.Difference
                                4 -> Icons.Default.Folder
                                else -> Icons.Default.Terminal
                            },
                            null,
                        )
                    },
                    trailingIcon = { if (page == destination) Icon(Icons.Default.Check, "Current view") },
                    onClick = {
                        expanded = false
                        navigate(destination)
                    },
                )
            }
            HorizontalDivider()
            DropdownMenuItem(text = { Text("Keyboard shortcuts") }, leadingIcon = { Icon(Icons.Default.Keyboard, null) }, onClick = {
                expanded = false
                keyboardHelp()
            })
        }
    }
}
