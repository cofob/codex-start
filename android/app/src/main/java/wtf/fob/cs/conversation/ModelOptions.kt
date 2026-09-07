package wtf.fob.cs.conversation

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*
import kotlin.math.roundToInt

data class MessageOptions(
    val model: String = "",
    val effort: String = "",
    val fast: Boolean? = null,
    val plan: Boolean = false,
) {
    fun parameters(defaultModel: String = ""): JSONObject =
        JSONObject().apply {
            if (model.isNotEmpty()) put("model", model)
            if (effort.isNotEmpty()) put("effort", effort)
            fast?.let { put("serviceTier", if (it) "fast" else JSONObject.NULL) }
            if (plan &&
                (model.isNotEmpty() || defaultModel.isNotEmpty())
            ) {
                put(
                    "collaborationMode",
                    obj(
                        "mode" to "plan",
                        "settings" to obj("model" to model.ifEmpty { defaultModel }, "reasoning_effort" to effort.ifEmpty { null }),
                    ),
                )
            }
        }
}

fun modelEfforts(model: JSONObject?): List<String> =
    model
        ?.optJSONArray("supportedReasoningEfforts")
        ?.objects()
        ?.map {
            it.text("reasoningEffort")
        }?.filter { it.isNotEmpty() }
        .orEmpty()

@Composable fun ModelOptionsSheet(
    repo: RemoteRepository,
    server: String,
    session: String,
    thread: String,
    options: MessageOptions,
    changed: (MessageOptions) -> Unit,
    close: () -> Unit,
    activeTurn: String = "",
) {
    val (query, state) = rememberRemoteData(repo, server, session, "model/list")
    val models =
        state.value
            ?.optJSONArray("data")
            ?.objects()
            .orEmpty()
    val (_, config) = rememberRemoteData(repo, server, session, "config/read")
    val (_, capabilities) = rememberRemoteData(repo, server, session, "modelProvider/capabilities/read")
    val defaultModel =
        config.value
            ?.optJSONObject("config")
            ?.text("model")
            .orEmpty()
    var selected by remember { mutableStateOf(options) }
    var busy by remember { mutableStateOf(false) }
    var failure by remember { mutableStateOf("") }
    var applyToTurn by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val model =
        models.firstOrNull { it.text("model", it.text("id")) == selected.model.ifEmpty { defaultModel } }
            ?: models.firstOrNull { it.optBoolean("isDefault") }
    val efforts = modelEfforts(model)
    val fastSupported =
        model?.optJSONArray("serviceTiers")?.objects()?.any { it.text("id") == "fast" } == true ||
            model?.optJSONArray("additionalSpeedTiers")?.let { tiers -> (0 until tiers.length()).any { tiers.optString(it) == "fast" } } ==
            true
    NativeSheet("Configure", close) {
        Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState())) {
            DataStatus(state) { repo.data.refresh(query, true) }
            SettingsGroup("Model") {
                SettingsRow("Use host default", "Use the model set on this computer.", trailing = {
                    RadioButton(selected.model.isEmpty(), onClick = {
                        selected =
                            selected.copy(model = "", effort = "")
                    })
                }, onClick = { selected = selected.copy(model = "", effort = "") })
                models.forEach { item ->
                    val id = item.text("model", item.text("id"))
                    val choose = { selected = selected.copy(model = id, effort = "") }
                    SettingsRow(item.text("displayName", id), item.text("description"), trailing = {
                        RadioButton(selected.model == id, onClick = choose)
                    }, onClick = choose)
                }
            }
            if (efforts.isNotEmpty()) {
                SettingsGroup("Thinking effort") {
                    val index =
                        efforts
                            .indexOf(
                                selected.effort.ifEmpty { model?.text("defaultReasoningEffort").orEmpty() },
                            ).coerceAtLeast(0)
                    Column(Modifier.padding(20.dp)) {
                        Text(humanize(efforts[index]), style = MaterialTheme.typography.titleMedium)
                        Text(
                            "More thinking can take longer.",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        if (efforts.size >
                            1
                        ) {
                            Slider(
                                value = index.toFloat(),
                                onValueChange = { selected = selected.copy(effort = efforts[it.roundToInt()]) },
                                valueRange =
                                    0f..efforts.lastIndex.toFloat(),
                                steps = (efforts.size - 2).coerceAtLeast(0),
                            )
                        }
                    }
                }
            }
            SettingsGroup("Response") {
                SettingsRow(
                    "Fast mode",
                    if (fastSupported) "Uses more of your account limit." else "Not available for this model.",
                    trailing = {
                        Switch(
                            selected.fast == true && fastSupported,
                            enabled = fastSupported,
                            onCheckedChange = { selected = selected.copy(fast = it) },
                        )
                    },
                )
                SettingsRow("Plan first", "Make a plan before changing files.", enabled = model != null, trailing = {
                    Switch(
                        selected.plan,
                        enabled =
                            model != null,
                        onCheckedChange = { selected = selected.copy(plan = it) },
                    )
                })
                if (activeTurn.isNotEmpty()) {
                    SettingsRow("Apply to the current turn", "Update its model, effort, and speed.", trailing = {
                        Switch(applyToTurn, {
                            applyToTurn =
                                it
                        })
                    })
                }
            }
            capabilities.value?.let { provider ->
                SettingsGroup("Provider features") {
                    listOf(
                        "webSearch" to "Web search",
                        "imageGeneration" to "Image generation",
                        "namespaceTools" to "Connected tools",
                    ).forEach { (key, title) ->
                        SettingsRow(title, if (provider.optBoolean(key)) "Available" else "Not available")
                    }
                }
            }
            if (failure.isNotEmpty()) Text(failure, color = MaterialTheme.colorScheme.error)
        }
        Button(enabled = !busy && (!selected.plan || model != null), modifier = Modifier.fillMaxWidth(), onClick = {
            scope.launch {
                busy = true
                failure = ""
                try {
                    val resolvedModel = selected.model.ifEmpty { defaultModel.ifEmpty { model?.text("model", model.text("id")).orEmpty() } }
                    val effective =
                        selected.copy(
                            fast =
                                if (fastSupported) {
                                    selected.fast
                                } else if (options.fast == true) {
                                    false
                                } else {
                                    null
                                },
                        )
                    if (thread.isNotEmpty() && effective != options) {
                        val params = effective.parameters(resolvedModel).put("threadId", thread)
                        if (selected.model.isEmpty() &&
                            options.model.isNotEmpty() &&
                            resolvedModel.isNotEmpty()
                        ) {
                            params.put("model", resolvedModel)
                        }
                        if (selected.effort.isEmpty() &&
                            options.effort.isNotEmpty()
                        ) {
                            model?.text("defaultReasoningEffort")?.takeIf { it.isNotEmpty() }?.let { params.put("effort", it) }
                        }
                        if (!selected.plan &&
                            options.plan &&
                            model != null
                        ) {
                            params.put(
                                "collaborationMode",
                                obj(
                                    "mode" to "default",
                                    "settings" to obj("model" to model.text("model", model.text("id"))),
                                ),
                            )
                        }
                        repo.rpc(server, session, "thread/settings/update", params)
                        if (applyToTurn && activeTurn.isNotEmpty()) {
                            val turnParams =
                                JSONObject(params.toString()).apply {
                                    remove("collaborationMode")
                                    put("turnId", activeTurn)
                                }
                            repo.rpc(server, session, "turn/settings/update", turnParams)
                        }
                    }
                    changed(effective.copy(model = if (selected.plan) resolvedModel else selected.model))
                    close()
                } catch (error: Exception) {
                    if (error is kotlinx.coroutines.CancellationException) throw error
                    failure =
                        error.message.orEmpty()
                } finally {
                    busy = false
                }
            }
        }) { Text(if (busy) "Saving…" else "Done") }
    }
}
