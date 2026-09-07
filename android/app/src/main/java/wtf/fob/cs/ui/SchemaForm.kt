package wtf.fob.cs.ui

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import wtf.fob.cs.workspace.*

@Composable
fun SchemaField(
    catalog: ProtocolCatalog,
    source: JSONObject,
    label: String,
    value: Any?,
    onChange: (Any?) -> Unit,
    depth: Int = 0,
) {
    val schema = catalog.resolve(source)
    if (schema.length() == 0) {
        val kinds = listOf("string", "number", "boolean", "object", "array", "null")
        var kind by remember {
            mutableStateOf(
                when (value) {
                    is JSONObject -> "object"
                    is JSONArray -> "array"
                    is Boolean -> "boolean"
                    is Number -> "number"
                    else -> "string"
                },
            )
        }
        Column {
            ChoiceMenu(label, kinds, kinds.indexOf(kind)) {
                kind = kinds[it]
                onChange(null)
            }
            SchemaField(catalog, obj("type" to kind, "additionalProperties" to JSONObject()), label, value, onChange, depth + 1)
        }
        return
    }
    if (depth > 14) {
        Text("This value exceeds the form nesting limit.")
        return
    }
    val variants = schema.optJSONArray("oneOf") ?: schema.optJSONArray("anyOf")
    if (variants != null) {
        val choices = variants.objects()
        var index by remember(source.toString()) {
            mutableIntStateOf(
                choices
                    .indexOfFirst { variant ->
                        val candidate = catalog.resolve(variant)
                        val kind =
                            candidate
                                .optJSONObject("properties")
                                ?.optJSONObject("type")
                                ?.optJSONArray("enum")
                                ?.optString(0)
                        kind != null && (value as? JSONObject)?.text("type") == kind
                    }.coerceAtLeast(0),
            )
        }
        Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(label, style = MaterialTheme.typography.titleSmall)
            ChoiceMenu(
                "Value type",
                choices.mapIndexed {
                    i,
                    option,
                    ->
                    catalog.resolve(option).let { it.text("title", it.text("type", "Option ${i + 1}")) }
                },
                index,
            ) {
                index =
                    it
                onChange(null)
            }
            choices.getOrNull(index)?.let { SchemaField(catalog, it, label, value, onChange, depth + 1) }
        }
        return
    }
    val types = schema.optJSONArray("type")
    if (types != null && (0 until types.length()).any { types.optString(it) == "null" }) {
        Row {
            FilterChip(value != JSONObject.NULL, { onChange(null) }, label = { Text("Set value") })
            FilterChip(value == JSONObject.NULL, { onChange(JSONObject.NULL) }, label = { Text("None") })
        }
        if (value == JSONObject.NULL) return
    }
    val type =
        if (types !=
            null
        ) {
            (0 until types.length()).map { types.getString(it) }.firstOrNull { it != "null" } ?: "null"
        } else {
            schema.text("type", "object")
        }
    val enum = schema.optJSONArray("enum")
    if (enum != null) {
        val choices = (0 until enum.length()).map { enum.get(it) }
        if (choices.size == 1) {
            LaunchedEffect(schema.toString()) { onChange(choices.first()) }
            Text("$label: ${choices.first()}")
        } else {
            ChoiceMenu(label, choices.map(Any::toString), choices.indexOf(value)) { onChange(choices[it]) }
        }
        return
    }
    when (type) {
        "object" -> {
            val current = value as? JSONObject ?: JSONObject()
            val props = schema.optJSONObject("properties") ?: JSONObject()
            val required = schema.optJSONArray("required")?.let { a -> (0 until a.length()).map { a.getString(it) }.toSet() } ?: emptySet()
            Column(verticalArrangement = Arrangement.spacedBy(10.dp), modifier = Modifier.fillMaxWidth()) {
                if (depth > 0) Text(label, style = MaterialTheme.typography.titleSmall)
                props.keys().asSequence().toList().forEach { name ->
                    key(name) {
                        val mandatory = name in required
                        var enabled by remember(source.toString(), name) { mutableStateOf(mandatory || current.has(name)) }
                        if (!mandatory) {
                            Row {
                                Checkbox(enabled, {
                                    enabled = it
                                    if (!it) {
                                        val next = JSONObject(current.toString())
                                        next.remove(name)
                                        onChange(next)
                                    }
                                })
                                Text(humanize(name), modifier = Modifier.padding(top = 12.dp))
                            }
                        }
                        if (enabled) {
                            SchemaField(catalog, props.optJSONObject(name) ?: JSONObject(), humanize(name), current.opt(name), { updated ->
                                val next = JSONObject(current.toString())
                                next.put(name, updated ?: JSONObject.NULL)
                                onChange(next)
                            }, depth + 1)
                        }
                    }
                }
                val additional = schema.optJSONObject("additionalProperties")
                if (additional != null || (props.length() == 0 && schema.optBoolean("additionalProperties", false))) {
                    var field by remember { mutableStateOf("") }
                    current.keys().asSequence().filterNot { props.has(it) }.toList().forEach { name ->
                        SchemaField(catalog, additional ?: JSONObject(), name, current.opt(name), { updated ->
                            onChange(JSONObject(current.toString()).put(name, updated))
                        }, depth + 1)
                    }
                    Row {
                        OutlinedTextField(field, { field = it }, label = { Text("Property name") }, modifier = Modifier.weight(1f))
                        TextButton(onClick = {
                            if (field.isNotBlank()) {
                                onChange(JSONObject(current.toString()).put(field, ""))
                                field = ""
                            }
                        }) { Text("Add") }
                    }
                }
                if (props.length() == 0 && additional == null) Text("No parameters")
            }
        }
        "array" -> {
            val values = value as? JSONArray ?: JSONArray()
            val item = schema.optJSONObject("items") ?: obj("type" to "string")
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(label, style = MaterialTheme.typography.titleSmall)
                for (i in 0 until values.length()) {
                    key(i) {
                        Row {
                            Column(Modifier.weight(1f)) {
                                SchemaField(
                                    catalog,
                                    item,
                                    "Item ${i + 1}",
                                    values.opt(i),
                                    { update -> onChange(JSONArray(values.toString()).put(i, update)) },
                                    depth + 1,
                                )
                            }
                            TextButton(onClick = {
                                val next = JSONArray(values.toString())
                                next.remove(i)
                                onChange(next)
                            }) { Text("Remove") }
                        }
                    }
                }
                TextButton(onClick = { onChange(JSONArray(values.toString()).put(JSONObject.NULL)) }) { Text("Add item") }
            }
        }
        "boolean" -> {
            LaunchedEffect(Unit) { if (value == null) onChange(false) }
            Row {
                Switch(value == true, { onChange(it) })
                Text(label, Modifier.padding(12.dp))
            }
        }
        "null" -> {
            LaunchedEffect(Unit) { onChange(JSONObject.NULL) }
            Text("$label: none")
        }
        else -> {
            var text by remember(value) { mutableStateOf(value?.toString().orEmpty()) }
            var invalid by remember { mutableStateOf(false) }
            OutlinedTextField(
                text,
                {
                    text = it
                    runCatching { scalarValue(it, type) }
                        .onSuccess { v ->
                            invalid = false
                            onChange(v)
                        }.onFailure {
                            invalid =
                                true
                        }
                },
                label = { Text(label) },
                isError = invalid,
                modifier = Modifier.fillMaxWidth(),
                supportingText =
                    schema.text("description").takeIf { it.isNotEmpty() }?.let { description ->
                        { Text(description.take(320)) }
                    },
                keyboardOptions =
                    KeyboardOptions(
                        keyboardType =
                            if (type in
                                setOf("integer", "number")
                            ) {
                                KeyboardType.Number
                            } else {
                                KeyboardType.Text
                            },
                    ),
            )
        }
    }
}

@Composable fun ChoiceMenu(
    label: String,
    choices: List<String>,
    selected: Int,
    choose: (Int) -> Unit,
) {
    var open by remember { mutableStateOf(false) }
    Box {
        OutlinedButton(onClick = { open = true }) { Text("$label: ${choices.getOrNull(selected) ?: "Choose"}") }
        DropdownMenu(open, { open = false }) {
            choices.forEachIndexed { index, text ->
                DropdownMenuItem(text = { Text(humanize(text)) }, onClick = {
                    open =
                        false
                    choose(index)
                })
            }
        }
    }
}

@Composable fun ValueView(
    value: Any?,
    depth: Int = 0,
) {
    when (value) {
        null, JSONObject.NULL -> Text("None")
        is JSONObject ->
            Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                value.keys().asSequence().take(80).toList().forEach { name ->
                    var expanded by remember { mutableStateOf(depth < 1) }
                    val child = value.opt(name)
                    if (child is JSONObject || child is JSONArray) {
                        TextButton(onClick = { expanded = !expanded }) { Text("${if (expanded) "−" else "+"} ${humanize(name)}") }
                        if (expanded && depth < 14) Box(Modifier.padding(start = 12.dp)) { ValueView(child, depth + 1) }
                    } else {
                        Text("${humanize(name)}: ${child?.toString().orEmpty().take(32_768)}")
                    }
                }
            }
        is JSONArray -> {
            var count by remember { mutableIntStateOf(30) }
            Column {
                for (i in 0 until minOf(count, value.length())) {
                    ValueView(value.opt(i), depth + 1)
                    HorizontalDivider()
                }
                if (value.length() > count) TextButton(onClick = { count += 30 }) { Text("Show more") }
            }
        }
        else -> Text(value.toString().take(32_768))
    }
}
