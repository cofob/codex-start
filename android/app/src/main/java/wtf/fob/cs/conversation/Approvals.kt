package wtf.fob.cs.conversation

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.tasks.*
import wtf.fob.cs.ui.*
import wtf.fob.cs.workspace.*

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun ApprovalCard(
    repo: RemoteRepository,
    event: RemoteEvent,
    catalog: ProtocolCatalog,
    resolved: () -> Unit,
) {
    val params = event.message.optJSONObject("params") ?: JSONObject()
    val method = event.message.text("method")
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    fun reply(result: JSONObject) {
        scope.launch {
            busy = true
            runCatching { repo.reply(event, result) }.onSuccess { resolved() }.onFailure(repo::report)
            busy =
                false
        }
    }
    Card(colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.tertiaryContainer)) {
        Column(Modifier.fillMaxWidth().padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            TaskStatus("Needs your input", attention = true)
            Text(
                when (method) {
                    "item/commandExecution/requestApproval", "execCommandApproval" -> "Allow this command?"
                    "item/fileChange/requestApproval", "applyPatchApproval" -> "Allow these file changes?"
                    "item/permissions/requestApproval" -> "Review requested permissions"
                    "item/tool/requestUserInput" -> "Codex has a question"
                    else -> humanize(method)
                },
                style = MaterialTheme.typography.titleMedium,
            )
            val details = JSONObject(params.toString()).apply { listOf("threadId", "turnId", "itemId").forEach(::remove) }
            ValueView(details)
            when (method) {
                "item/tool/requestUserInput" -> {
                    var answers by remember { mutableStateOf(JSONObject()) }
                    params.optJSONArray("questions")?.objects()?.forEach { question ->
                        val id = question.getString("id")
                        var value by remember { mutableStateOf("") }
                        Text(question.text("question"))
                        question.optJSONArray("options")?.objects()?.forEach { option ->
                            FilterChip(selected = value == option.text("label"), onClick = {
                                value = option.text("label")
                                answers =
                                    JSONObject(answers.toString()).put(id, obj("answers" to JSONArray().put(value)))
                            }, label = { Text(option.text("label")) })
                            Text(option.text("description"), style = MaterialTheme.typography.bodySmall)
                        }
                        OutlinedTextField(value, {
                            value = it
                            answers =
                                JSONObject(answers.toString()).put(id, obj("answers" to JSONArray().put(value)))
                        }, label = { Text("Your answer") })
                    }
                    Button(onClick = { reply(obj("answers" to answers)) }, enabled = !busy) { Text("Submit answers") }
                }
                "mcpServer/elicitation/request" -> {
                    var content by remember { mutableStateOf<Any?>(JSONObject()) }
                    params.optJSONObject("requestedSchema")?.let {
                        SchemaField(catalog, it, "Requested input", content, { value -> content = value })
                    }
                    if (params
                            .text(
                                "url",
                            ).isNotEmpty()
                    ) {
                        androidx.compose.foundation.text.selection
                            .SelectionContainer { Text(params.text("url")) }
                    }
                    Row {
                        Button(onClick = { reply(obj("action" to "accept", "content" to content)) }, enabled = !busy) { Text("Submit") }
                        TextButton(onClick = { reply(obj("action" to "decline", "content" to null)) }, enabled = !busy) { Text("Decline") }
                        TextButton(onClick = { reply(obj("action" to "cancel", "content" to null)) }, enabled = !busy) { Text("Cancel") }
                    }
                }
                "item/commandExecution/requestApproval",
                "item/fileChange/requestApproval",
                "execCommandApproval",
                "applyPatchApproval",
                -> {
                    val choices =
                        params.optJSONArray("availableDecisions")
                            ?: JSONArray(
                                if (method.startsWith(
                                        "item/",
                                    )
                                ) {
                                    listOf(
                                        "accept",
                                        "acceptForSession",
                                        "decline",
                                        "cancel",
                                    )
                                } else {
                                    listOf("approved", "approved_for_session", "denied", "abort")
                                },
                            )
                    FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        for (index in 0 until choices.length()) {
                            val decision = choices.get(index)
                            val label =
                                when (decision) {
                                    "accept", "approved" -> "Approve"
                                    "acceptForSession", "approved_for_session" -> "Allow this session"
                                    "decline", "denied" -> "Reject"
                                    "cancel", "abort" -> "Cancel task"
                                    else ->
                                        if (decision is JSONObject) {
                                            humanize(
                                                decision
                                                    .keys()
                                                    .asSequence()
                                                    .firstOrNull()
                                                    .orEmpty(),
                                            )
                                        } else {
                                            humanize(decision.toString())
                                        }
                                }
                            if (decision in
                                listOf("accept", "approved")
                            ) {
                                Button(onClick = { reply(obj("decision" to decision)) }, enabled = !busy) { Text(label) }
                            } else {
                                OutlinedButton(onClick = { reply(obj("decision" to decision)) }, enabled = !busy) { Text(label) }
                            }
                        }
                    }
                }
                "item/permissions/requestApproval" -> {
                    var scopeName by remember { mutableStateOf("turn") }
                    ChoiceMenu(
                        "Scope",
                        listOf("turn", "session"),
                        if (scopeName ==
                            "turn"
                        ) {
                            0
                        } else {
                            1
                        },
                    ) { scopeName = if (it == 0) "turn" else "session" }
                    Row {
                        Button(onClick = {
                            reply(obj("permissions" to params.optJSONObject("permissions"), "scope" to scopeName))
                        }, enabled = !busy) { Text("Grant requested permissions") }
                        TextButton(
                            onClick = { reply(obj("permissions" to JSONObject(), "scope" to "turn")) },
                            enabled = !busy,
                        ) { Text("Decline") }
                    }
                }
                "item/tool/call" -> {
                    var result by remember {
                        mutableStateOf<Any?>(
                            obj(
                                "success" to true,
                                "contentItems" to JSONArray().put(obj("type" to "inputText", "text" to "")),
                            ),
                        )
                    }
                    var error by remember { mutableStateOf("") }
                    val schema = catalog.response("DynamicToolCallResponse")
                    Text("This tool belongs to an attached client. Enter its result only after the tool has run.")
                    SchemaField(catalog, schema, "Tool result", result, { result = it })
                    if (error.isNotEmpty()) Text(error, color = MaterialTheme.colorScheme.error)
                    Button(onClick = {
                        runCatching {
                            validateForm(catalog, schema, result)
                            reply(result as JSONObject)
                        }.onFailure {
                            error =
                                it.message.orEmpty()
                        }
                    }, enabled = !busy) { Text("Submit tool result") }
                    TextButton(onClick = {
                        reply(
                            obj(
                                "success" to false,
                                "contentItems" to
                                    JSONArray().put(obj("type" to "inputText", "text" to "The attached client could not run this tool.")),
                            ),
                        )
                    }, enabled = !busy) { Text("Report tool unavailable") }
                }
                else -> Text("The host must handle this request. Open its client to continue.")
            }
            if (busy) LinearProgressIndicator()
        }
    }
}
