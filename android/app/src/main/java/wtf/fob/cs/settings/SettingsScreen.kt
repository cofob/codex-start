package wtf.fob.cs.settings

import android.content.Intent
import android.provider.Settings
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.HelpOutline
import androidx.compose.material.icons.automirrored.filled.MenuBook
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.core.content.edit
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlinx.coroutines.launch
import org.json.JSONObject
import wtf.fob.cs.BuildConfig
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*

enum class FeaturePage(
    val title: String,
    val icon: ImageVector,
) {
    Plugins("Plugins", Icons.Default.Extension),
    Skills("Skills and hooks", Icons.Default.AutoAwesome),
    Connections("Connected apps", Icons.Default.Link),
    Account("Account and usage", Icons.Default.AccountCircle),
    Personalization("Personalization", Icons.Default.Tune),
    Permissions("Permissions", Icons.Default.Shield),
    Memory("Memory", Icons.AutoMirrored.Filled.MenuBook),
    Features("Experimental features", Icons.Default.Science),
    Diagnostics("Help and diagnostics", Icons.AutoMirrored.Filled.HelpOutline),
    Organization("Library", Icons.Default.FolderSpecial),
    Configuration("Host configuration", Icons.Default.Settings),
    Launcher("Codex Start settings", Icons.Default.Tune),
    Imports("Import settings", Icons.Default.FileDownload),
}

@Composable fun SettingsScreen(
    repo: RemoteRepository,
    server: String,
    monitor: () -> Unit,
    guide: () -> Unit,
    sessions: () -> Unit,
    feature: (FeaturePage) -> Unit,
) {
    var devices by remember(server) { mutableStateOf<List<JSONObject>>(emptyList()) }
    var showDevices by remember { mutableStateOf(false) }
    var confirm by remember { mutableStateOf<String?>(null) }
    var appearance by remember { mutableStateOf(false) }
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var rename by remember { mutableStateOf(false) }
    val servers by repo.servers.collectAsStateWithLifecycle()
    val selected = servers.firstOrNull { it.id == server }
    val name = selected?.name.orEmpty()
    var draftName by remember { mutableStateOf("") }
    LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(top = 16.dp, bottom = 24.dp)) {
        item {
            SettingsGroup("Your Codex") {
                SettingsRow(
                    "Personalization",
                    "Set your preferred response style",
                    Icons.Default.Tune,
                    onClick = { feature(FeaturePage.Personalization) },
                )
                SettingsRow(
                    "Memory",
                    "Control what Codex remembers",
                    Icons.AutoMirrored.Filled.MenuBook,
                    onClick = { feature(FeaturePage.Memory) },
                )
                SettingsRow("Plugins", "Add tools to your workspace", Icons.Default.Extension, onClick = { feature(FeaturePage.Plugins) })
            }
            SettingsGroup("App") {
                SettingsRow("Appearance", "Theme and accent color", Icons.Default.Palette, onClick = { appearance = true })
                SettingsRow(
                    "Notifications",
                    "Get updates when work finishes or needs input",
                    Icons.Default.NotificationsNone,
                    onClick = monitor,
                )
                SettingsRow("Battery settings", "Manage background activity", Icons.Default.BatteryStd, onClick = {
                    context.startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
                })
            }
            if (server.isNotEmpty()) {
                SettingsGroup("Computer") {
                    SettingsRow(name, selected?.status.orEmpty(), Icons.Default.Computer, trailing = {
                        TextButton(onClick = {
                            draftName = name
                            rename =
                                true
                        }) { Text("Rename") }
                    })
                    SettingsRow("Sessions", "Environments, restart, and logs", Icons.Default.Terminal, onClick = sessions)
                    SettingsRow(
                        "Codex Start settings",
                        "Profiles, global settings, and project settings",
                        Icons.Default.Tune,
                        onClick = { feature(FeaturePage.Launcher) },
                    )
                    SettingsRow("Registered devices", "Manage access to this computer", Icons.Default.Devices, onClick = {
                        scope.launch {
                            runCatching {
                                devices =
                                    (repo.request(server, "device/list") as JSONObject).getJSONArray("data").objects()
                                showDevices = true
                            }.onFailure(repo::report)
                        }
                    })
                    SettingsRow("Rotate invitation password", "Invalidate existing invitations", Icons.Default.Key, onClick = {
                        confirm =
                            "rotate"
                    })
                    SettingsRow("Forget server", "Remove this connection from your device", Icons.Default.LinkOff, onClick = {
                        confirm =
                            "forget"
                    })
                }
            }
            SettingsGroup("Workspace") {
                listOf(
                    FeaturePage.Account,
                    FeaturePage.Permissions,
                    FeaturePage.Connections,
                    FeaturePage.Skills,
                    FeaturePage.Organization,
                    FeaturePage.Features,
                    FeaturePage.Configuration,
                    FeaturePage.Imports,
                ).forEach { page ->
                    SettingsRow(page.title, icon = page.icon, onClick = { feature(page) })
                }
            }
            SettingsGroup("Support") {
                SettingsRow("App guide", "Connect a computer and start working", Icons.Default.AutoStories, onClick = guide)
                SettingsRow("Help and diagnostics", "Connection details and feedback", Icons.AutoMirrored.Filled.HelpOutline, onClick = {
                    feature(FeaturePage.Diagnostics)
                })
                SettingsRow("Codex Start", "Version ${BuildConfig.VERSION_NAME}", Icons.Default.Info)
            }
        }
    }
    if (appearance) AppearanceSheet { appearance = false }
    if (rename) {
        AlertDialog(onDismissRequest = { rename = false }, title = { Text("Server name") }, text = {
            OutlinedTextField(draftName, {
                draftName =
                    it
            }, label = { Text("Name on this device") }, singleLine = true)
        }, confirmButton = {
            TextButton(onClick = {
                repo.rename(server, draftName)
                rename =
                    false
            }, enabled = draftName.isNotBlank()) { Text("Save") }
        }, dismissButton = { TextButton(onClick = { rename = false }) { Text("Cancel") } })
    }
    if (showDevices) {
        NativeSheet("Registered devices", { showDevices = false }) {
            Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState())) {
                devices.forEach { device ->
                    SettingsRow(device.text("name"), icon = Icons.Default.Smartphone, trailing = {
                        TextButton(onClick = {
                            scope.launch {
                                runCatching {
                                    repo.request(
                                        server,
                                        "device/revoke",
                                        obj(
                                            "deviceId" to device.text("id", device.text("deviceId")),
                                        ),
                                    )
                                }.onSuccess { devices = devices.filterNot { it === device } }.onFailure(repo::report)
                            }
                        }) { Text("Revoke") }
                    })
                }
            }
            Text(
                "Rotate the invitation password to prevent a removed device from registering again.",
                Modifier.padding(top = 16.dp),
                style = MaterialTheme.typography.bodySmall,
            )
        }
    }
    confirm?.let { operation ->
        AlertDialog(onDismissRequest = { confirm = null }, title = {
            Text(
                if (operation ==
                    "rotate"
                ) {
                    "Rotate invitation password?"
                } else {
                    "Forget this server?"
                },
            )
        }, text = {
            Text(
                if (operation ==
                    "rotate"
                ) {
                    "Old invitations and pending registrations will stop working. Registered devices keep access."
                } else {
                    "This removes the local credentials. It does not revoke the device on the host."
                },
            )
        }, confirmButton = {
            TextButton(onClick = {
                scope.launch {
                    runCatching {
                        if (operation ==
                            "rotate"
                        ) {
                            repo.request(server, "connectionPassword/rotate")
                        } else {
                            repo.forget(server)
                        }
                    }.onFailure(repo::report)
                    confirm =
                        null
                }
            }) { Text("Continue") }
        }, dismissButton = { TextButton(onClick = { confirm = null }) { Text("Cancel") } })
    }
}

@Composable fun AppearanceSheet(close: () -> Unit) {
    val preferences = LocalContext.current.getSharedPreferences("appearance", 0)
    var theme by remember { mutableStateOf(preferences.getString("theme", "system")) }
    var dynamic by remember { mutableStateOf(preferences.getBoolean("dynamic", false)) }
    NativeSheet("Appearance", close, scrollContent = true) {
        SettingsGroup("Theme") {
            listOf("system" to "Use device setting", "light" to "Light", "dark" to "Dark").forEach { (value, label) ->
                val select = {
                    theme = value
                    preferences.edit { putString("theme", value) }
                }
                SettingsRow(label, trailing = { RadioButton(theme == value, onClick = select) }, onClick = select)
            }
        }
        SettingsGroup {
            SettingsRow("Use wallpaper colors", "Match your Android accent color", trailing = {
                Switch(dynamic, onCheckedChange = {
                    dynamic =
                        it
                    preferences.edit { putBoolean("dynamic", it) }
                })
            })
        }
        Button(onClick = close, modifier = Modifier.fillMaxWidth()) { Text("Done") }
    }
}
