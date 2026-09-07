package wtf.fob.cs.settings

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.json.JSONObject
import wtf.fob.cs.conversation.*
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*

@Composable fun AccountActivitySheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    close: () -> Unit,
) {
    val (query, state) = rememberRemoteData(repo, server, session, "account/usage/read")
    val (messageQuery, messages) = rememberRemoteData(repo, server, session, "account/workspaceMessages/read")
    NativeSheet("Account activity", close) {
        DataStatus(state) { repo.data.refresh(query, true) }
        LazyColumn(Modifier.weight(1f, fill = false)) {
            item {
                SettingsGroup("Token activity") {
                    val summary = state.value?.optJSONObject("summary")
                    listOf(
                        "lifetimeTokens" to "Total tokens",
                        "peakDailyTokens" to "Most tokens in one day",
                        "currentStreakDays" to "Current streak (days)",
                        "longestStreakDays" to "Longest streak (days)",
                        "longestRunningTurnSec" to "Longest turn (seconds)",
                    ).forEach { (key, title) ->
                        SettingsRow(
                            title,
                            if (summary == null ||
                                summary.isNull(key)
                            ) {
                                "Unavailable"
                            } else {
                                java.text.NumberFormat
                                    .getIntegerInstance()
                                    .format(summary.optLong(key))
                            },
                        )
                    }
                }
            }
            item { DataStatus(messages) { repo.data.refresh(messageQuery, true) } }
            items(
                messages.value
                    ?.optJSONArray("messages")
                    ?.objects()
                    .orEmpty(),
                key = {
                    it.text("messageId")
                },
            ) { message -> SettingsGroup { Markdown(message.text("messageBody")) } }
        }
        TextButton(onClick = {
            repo.data.refresh(query, true)
            repo.data.refresh(messageQuery, true)
        }) { Text("Refresh") }
    }
}

@Composable fun ProviderSetupSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    close: () -> Unit,
) {
    var bedrock by remember { mutableStateOf(false) }
    var apiKey by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var failure by remember { mutableStateOf("") }
    val scope = rememberCoroutineScope()

    fun save(
        method: String,
        params: JSONObject,
    ) {
        scope.launch {
            busy = true
            failure = ""
            try {
                repo.rpc(server, session, method, params)
                apiKey = ""
                repo.data.invalidate(server)
                repo.data.refreshObserved(server)
                close()
            } catch (
                error: Exception,
            ) {
                if (error is kotlinx.coroutines.CancellationException) throw error
                failure = error.message.orEmpty()
            } finally {
                busy = false
            }
        }
    }
    NativeSheet("Model provider", close, scrollContent = true) {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(!bedrock, { bedrock = false }, label = { Text("OpenAI API") })
            FilterChip(bedrock, { bedrock = true }, label = { Text("Amazon Bedrock") })
        }
        if (!bedrock) {
            Text("Store an API key on this computer to use API billing.", Modifier.padding(vertical = 16.dp))
            OutlinedTextField(apiKey, {
                apiKey = it
            }, label = {
                Text("API key")
            }, visualTransformation = PasswordVisualTransformation(), modifier = Modifier.fillMaxWidth(), singleLine = true)
            Button(enabled = !busy && apiKey.isNotBlank(), modifier = Modifier.fillMaxWidth().padding(top = 16.dp), onClick = {
                save(
                    "account/login/start",
                    obj(
                        "type" to "apiKey",
                        "apiKey" to apiKey.trim(),
                    ),
                )
            }) { Text("Use API key") }
        } else {
            val (query, state) = rememberRemoteData(repo, server, session, "account/bedrock/discover")
            var profile by remember { mutableStateOf("") }
            var region by remember { mutableStateOf("") }
            DataStatus(state) { repo.data.refresh(query, true) }
            Text("Use AWS credentials that are already on this computer.", Modifier.padding(vertical = 16.dp))
            Column {
                SettingsRow("Environment credentials", trailing = { RadioButton(profile.isEmpty(), { profile = "" }) }, onClick = {
                    profile =
                        ""
                })
                state.value?.optJSONArray("profiles")?.objects().orEmpty().forEach { entry ->
                    SettingsRow(entry.text("name"), entry.text("region"), trailing = {
                        RadioButton(profile == entry.text("name"), {
                            profile =
                                entry.text("name")
                            region = entry.text("region")
                        })
                    }, onClick = {
                        profile = entry.text("name")
                        region =
                            entry.text("region")
                    })
                }
            }
            OutlinedTextField(
                region,
                { region = it },
                label = { Text("AWS region") },
                modifier = Modifier.fillMaxWidth(),
                singleLine = true,
            )
            Button(enabled = !busy && region.isNotBlank(), modifier = Modifier.fillMaxWidth().padding(top = 16.dp), onClick = {
                save(
                    "account/bedrock/setup",
                    obj(
                        "type" to if (profile.isEmpty()) "environment" else "profile",
                        "region" to region.trim(),
                    ).apply { if (profile.isNotEmpty()) put("profile", profile) },
                )
            }) { Text("Use Bedrock") }
        }
        if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
    }
}
