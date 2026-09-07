package wtf.fob.cs.data

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject

/** Protocol fixtures are part of the APK and do not use the reference checkout. */
class ProtocolCatalog(
    context: Context,
) {
    private val requests =
        JSONObject(
            context.assets
                .open("ClientRequest.json")
                .bufferedReader()
                .use { it.readText() },
        )
    private val serverRequests =
        JSONObject(
            context.assets
                .open("ServerRequest.json")
                .bufferedReader()
                .use { it.readText() },
        )
    private val responses =
        context.assets.list("responses").orEmpty().filter { it.endsWith(".json") }.associate { file ->
            file.removeSuffix(".json") to
                JSONObject(
                    context.assets
                        .open("responses/$file")
                        .bufferedReader()
                        .use { it.readText() },
                )
        }

    fun resolve(
        input: JSONObject,
        server: Boolean = false,
    ): JSONObject {
        input.optJSONArray("allOf")?.optJSONObject(0)?.let { return resolve(it, server) }
        val ref = input.text("\$ref")
        if (ref.isEmpty()) return input
        val root = if (server) serverRequests else requests
        val name = ref.substringAfterLast('/')
        return root.optJSONObject("definitions")?.optJSONObject(name)
            ?: serverRequests.optJSONObject("definitions")?.optJSONObject(name)
            ?: responses.values.firstNotNullOfOrNull { it.optJSONObject("definitions")?.optJSONObject(name) }
            ?: input
    }

    fun response(name: String): JSONObject = responses.getValue(name)
}

fun humanize(name: String): String =
    name.replace(Regex("([a-z])([A-Z])"), "$1 $2").replace('/', ' ').replace('_', ' ').replaceFirstChar {
        it.uppercase()
    }

/** Validate types before a form is submitted. Optional fields remain absent. */
fun scalarValue(
    text: String,
    type: String,
): Any =
    when (type) {
        "integer" -> text.toLongOrNull() ?: error("Enter a whole number")
        "number" -> text.toDoubleOrNull()?.takeIf { it.isFinite() } ?: error("Enter a number")
        "boolean" -> text.toBooleanStrictOrNull() ?: error("Choose true or false")
        else -> text
    }

/** Report field errors before sending a mutation. This does not replace host validation. */
fun validateForm(
    catalog: ProtocolCatalog,
    input: JSONObject,
    value: Any?,
    path: String = "Parameters",
    depth: Int = 0,
) {
    require(depth <= 16) { "$path is too deeply nested" }
    val schema = catalog.resolve(input)
    val variants = schema.optJSONArray("oneOf") ?: schema.optJSONArray("anyOf")
    if (variants != null) {
        require(
            variants.objects().any {
                runCatching { validateForm(catalog, it, value, path, depth + 1) }.isSuccess
            },
        ) { "$path has an incomplete value" }
        return
    }
    schema.optJSONArray("enum")?.let { values ->
        require((0 until values.length()).any { values.opt(it).toString() == value.toString() }) { "Choose $path" }
    }
    val types = schema.optJSONArray("type")?.let { a -> (0 until a.length()).map { a.getString(it) } } ?: listOf(schema.text("type"))
    val actual =
        when (value) {
            null, JSONObject.NULL -> "null"
            is JSONObject -> "object"
            is JSONArray -> "array"
            is Boolean -> "boolean"
            is Long, is Int -> "integer"
            is Number -> "number"
            else -> "string"
        }
    require(types == listOf("") || actual in types || (actual == "integer" && "number" in types)) {
        "$path must be ${types.joinToString(" or ")}"
    }
    if (value is JSONObject) {
        schema.optJSONArray("required")?.let { a ->
            for (i in 0 until a.length()) require(value.has(a.getString(i))) { "${humanize(a.getString(i))} is required" }
        }
        val properties = schema.optJSONObject("properties")
        value.keys().asSequence().forEach { name ->
            (properties?.optJSONObject(name) ?: schema.optJSONObject("additionalProperties"))?.let {
                validateForm(
                    catalog,
                    it,
                    value.opt(name),
                    "$path · ${humanize(name)}",
                    depth + 1,
                )
            }
        }
    }
    if (value is JSONArray) {
        require(value.length() >= schema.optInt("minItems", 0) && value.length() <= schema.optInt("maxItems", Int.MAX_VALUE)) {
            "$path has an invalid item count"
        }
        schema.optJSONObject("items")?.let { item ->
            for (i in 0 until value.length()) {
                validateForm(
                    catalog,
                    item,
                    value.opt(i),
                    "$path · ${i + 1}",
                    depth + 1,
                )
            }
        }
    }
    if (value is Number) {
        val n = value.toDouble()
        require(n >= schema.optDouble("minimum", -Double.MAX_VALUE) && n <= schema.optDouble("maximum", Double.MAX_VALUE)) {
            "$path is outside the allowed range"
        }
    }
    if (value is String) {
        require(
            value.length >= schema.optInt("minLength", 0) && value.length <= schema.optInt("maxLength", Int.MAX_VALUE),
        ) {
            "$path has an invalid length"
        }
    }
}
