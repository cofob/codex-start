package wtf.fob.cs.conversation

import org.json.JSONArray
import org.json.JSONObject
import wtf.fob.cs.data.*

internal val CONVERSATION_DELTAS =
    setOf(
        "item/agentMessage/delta",
        "item/reasoning/summaryTextDelta",
        "item/reasoning/textDelta",
        "item/commandExecution/outputDelta",
        "item/plan/delta",
    )

/** Preserve valid UTF-16 at the display limit, including split emoji in transport deltas. */
internal fun streamTail(
    text: String,
    limit: Int = 512 * 1024,
): String {
    require(limit > 0)
    var start = (text.length - limit).coerceAtLeast(0)
    if (start > 0 && text[start].isLowSurrogate() && text[start - 1].isHighSurrogate()) start++
    return text.substring(start)
}

/** Apply every delta in order, but copy and publish each changed item only once per batch. */
internal fun applyConversationDeltas(
    items: List<ConversationItem>,
    events: List<RemoteEvent>,
): List<ConversationItem> {
    val changed = linkedMapOf<String, JSONObject>()
    val originals = items.associateBy { it.id }
    for (event in events) {
        val method = event.message.text("method")
        if (method !in CONVERSATION_DELTAS) continue
        val params = event.message.optJSONObject("params") ?: continue
        val id = params.text("itemId")
        if (id.isBlank() || params.text("delta").isEmpty()) continue
        val previous = originals[id]
        val detail =
            changed.getOrPut(id) {
                previous?.details?.let { JSONObject(it.toString()) }
                    ?: obj("id" to id, "type" to method.split('/')[1], "text" to previous?.text.orEmpty())
            }
        val delta = params.text("delta")
        when (method) {
            "item/commandExecution/outputDelta" -> {
                detail.put("aggregatedOutput", streamTail(detail.text("aggregatedOutput") + delta))
            }
            "item/reasoning/summaryTextDelta", "item/reasoning/textDelta" -> {
                val summary = method == "item/reasoning/summaryTextDelta"
                val index = params.optInt(if (summary) "summaryIndex" else "contentIndex", 0)
                // Do not allocate an array from an unbounded remote index.
                if (index !in 0..1023) continue
                val field = if (summary) "summary" else "content"
                val parts = detail.optJSONArray(field) ?: JSONArray().also { detail.put(field, it) }
                val current = parts.optJSONObject(index)?.text("text") ?: parts.optString(index).takeUnless { it == "null" }.orEmpty()
                parts.put(index, streamTail(current + delta))
            }
            else -> detail.put("text", streamTail(detail.text("text") + delta))
        }
    }
    if (changed.isEmpty()) return items
    changed.values.filter { it.text("type") == "reasoning" }.forEach { detail ->
        listOf("summary", "content").forEach field@{ field ->
            val parts = detail.optJSONArray(field) ?: return@field
            var remaining = 512 * 1024
            for (index in parts.length() - 1 downTo 0) {
                if (parts.isNull(index)) continue
                val text = parts.optJSONObject(index)?.text("text") ?: parts.optString(index)
                if (remaining == 0) {
                    parts.put(index, JSONObject.NULL)
                } else {
                    val kept = streamTail(text, remaining)
                    parts.put(index, kept)
                    remaining -= kept.length
                }
            }
        }
    }
    val result = items.map { changed.remove(it.id)?.let(::conversationItem) ?: it }
    return boundedConversation(result + changed.values.map(::conversationItem))
}
