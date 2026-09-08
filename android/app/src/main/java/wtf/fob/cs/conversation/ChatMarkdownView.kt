package wtf.fob.cs.conversation

import android.annotation.SuppressLint
import android.content.Context
import android.content.res.Configuration
import android.text.SpannableString
import android.text.Spanned
import android.text.style.ClickableSpan
import android.util.LruCache
import android.view.ActionMode
import android.view.Menu
import android.view.MenuItem
import android.view.MotionEvent
import android.view.ViewConfiguration
import android.widget.TextView
import io.noties.markwon.Markwon
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import org.commonmark.node.AbstractVisitor
import org.commonmark.node.Node
import org.commonmark.node.OrderedList
import kotlin.math.abs

/** Cache syntax trees, not Android spans: list/heading spans contain mutable drawing metrics. */
internal class ChatMarkdownCache {
    private val documents =
        object : LruCache<String, Node>(2 * 1024 * 1024) {
            override fun sizeOf(
                key: String,
                value: Node,
            ): Int = weight(key)
        }

    // Conservative budget for the source, syntax nodes, and their text. No Activity is retained.
    private fun weight(source: String): Int = 128 + 64 * source.length

    fun get(source: String): Node? = documents.get(source)

    fun put(
        source: String,
        parsed: Node,
    ) {
        // One very large response must not evict all of the ordinary scrolling rows.
        if (weight(source) <= documents.maxSize()) documents.put(source, parsed)
    }

    companion object {
        private var configuration: Configuration? = null
        private var current = ChatMarkdownCache()

        // Called on the UI thread. Never keep an Activity or selection/focus spans in the cache.
        fun forContext(context: Context): ChatMarkdownCache {
            if (configuration != context.resources.configuration) {
                configuration = Configuration(context.resources.configuration)
                current = ChatMarkdownCache()
            }
            return current
        }
    }
}

/** First binding has a stable height; only updates to an existing answer parse asynchronously. */
@SuppressLint("AppCompatCustomView") // Compose owns the theme; use the platform selection toolbar.
internal class ChatMarkdownView(
    context: Context,
    private val cache: ChatMarkdownCache = ChatMarkdownCache.forContext(context),
) : TextView(context) {
    private val markwon = Markwon.create(context.applicationContext)
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val updates = Channel<String>(Channel.CONFLATED)
    private var requested: String? = null
    private var rendered: Spanned? = null
    private var displayed: Spanned? = null
    private var selecting = false
    private var downX = 0f
    private var downY = 0f
    private var downLink: ClickableSpan? = null
    private val slop = ViewConfiguration.get(context).scaledTouchSlop
    var selectionChanged: (Boolean) -> Unit = {}
    var renderChanged: () -> Unit = {}
    internal var renderCount = 0
        private set

    init {
        includeFontPadding = false
        setLineSpacing(0f, 1.15f)
        customSelectionActionModeCallback =
            object : ActionMode.Callback {
                override fun onCreateActionMode(
                    mode: ActionMode,
                    menu: Menu,
                ): Boolean {
                    selecting = true
                    selectionChanged(true)
                    return true
                }

                override fun onPrepareActionMode(
                    mode: ActionMode,
                    menu: Menu,
                ) = false

                override fun onActionItemClicked(
                    mode: ActionMode,
                    item: MenuItem,
                ) = false

                override fun onDestroyActionMode(mode: ActionMode) {
                    selecting = false
                    selectionChanged(false)
                    // Let Android finish removing its handles before replacing the text.
                    scope.launch {
                        yield()
                        if (!selecting) {
                            clearFocus()
                            rendered?.let(::display)
                        }
                    }
                }
            }
        scope.launch {
            for (source in updates) {
                val (document, parsed) =
                    withContext(Dispatchers.Default) {
                        val document = markwon.parse(source)
                        document to render(document, source)
                    }
                // Do not apply an obsolete prefix: it would cause an extra height change.
                if (requested == source) {
                    cache.put(source, document)
                    rendered = parsed
                    if (!selecting) display(parsed)
                }
            }
        }
    }

    private fun render(
        document: Node,
        source: String,
    ): Spanned =
        synchronized(document) {
            // Markwon 4.6.2 increments OrderedList.startNumber during rendering. Restore every
            // counter, including nested lists, before another view can use the cached syntax tree.
            val starts = mutableListOf<Pair<OrderedList, Int>>()
            document.accept(
                object : AbstractVisitor() {
                    override fun visit(orderedList: OrderedList) {
                        starts += orderedList to orderedList.startNumber
                        visitChildren(orderedList)
                    }
                },
            )
            try {
                markwon.render(document).takeIf { it.isNotEmpty() || source.isEmpty() } ?: SpannableString(source)
            } finally {
                starts.forEach { (list, start) -> list.startNumber = start }
            }
        }

    private fun display(parsed: Spanned) {
        // Selectable TextView copies its buffer, so comparing `text === parsed` is not valid.
        if (displayed === parsed) return
        displayed = parsed
        // Each view owns its selection and Android watcher spans.
        markwon.setParsedMarkdown(this, SpannableString(parsed))
        renderCount++
        renderChanged()
    }

    fun submit(source: String) {
        if (requested == source) return
        requested = source
        val cached = cache.get(source)
        if (cached != null || displayed == null) {
            // LazyColumn must never measure a restored answer as an empty TextView. Cold first
            // bindings parse once here; returning rows use the bounded cache without parsing.
            val document = cached ?: markwon.parse(source).also { cache.put(source, it) }
            // A fresh span tree keeps native drawing metrics and selection local to this view.
            rendered = render(document, source)
            if (!selecting) display(requireNotNull(rendered))
        } else {
            updates.trySend(source)
        }
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        if (event.actionMasked == MotionEvent.ACTION_DOWN) {
            downX = event.x
            downY = event.y
            downLink = if (isTextSelectable && !selecting) linkAt(event.x, event.y) else null
        }
        if (event.actionMasked == MotionEvent.ACTION_CANCEL) downLink = null
        if (!selecting &&
            event.actionMasked == MotionEvent.ACTION_MOVE &&
            (abs(event.x - downX) > slop || abs(event.y - downY) > slop)
        ) {
            downLink = null
            val cancel = MotionEvent.obtain(event).apply { action = MotionEvent.ACTION_CANCEL }
            super.onTouchEvent(cancel)
            cancel.recycle()
            parent?.requestDisallowInterceptTouchEvent(false)
            return false
        }
        // Selectable TextView uses ArrowKeyMovementMethod, which does not open Markdown links.
        val clicked =
            if (event.actionMasked == MotionEvent.ACTION_UP &&
                !selecting &&
                event.eventTime - event.downTime < ViewConfiguration.getLongPressTimeout()
            ) {
                downLink?.takeIf { it === linkAt(event.x, event.y) }
            } else {
                null
            }
        val handled = super.onTouchEvent(event)
        clicked?.onClick(this)
        // The chat owns ordinary swipes; selection handles own drags only after a long press.
        if (!selecting) parent?.requestDisallowInterceptTouchEvent(false)
        return handled || clicked != null
    }

    override fun performClick(): Boolean = super.performClick()

    private fun linkAt(
        x: Float,
        y: Float,
    ): ClickableSpan? {
        val layout = layout ?: return null
        val content = text as? Spanned ?: return null
        val localX = x - totalPaddingLeft + scrollX
        val localY = y - totalPaddingTop + scrollY
        if (localY < 0 || localY >= layout.height) return null
        val line = layout.getLineForVertical(localY.toInt())
        if (localX < layout.getLineLeft(line) || localX > layout.getLineRight(line)) return null
        val offset = layout.getOffsetForHorizontal(line, localX)
        return content.getSpans(offset, offset, ClickableSpan::class.java).firstOrNull()
    }

    fun close() {
        selectionChanged(false)
        selectionChanged = {}
        renderChanged = {}
        scope.cancel()
        updates.close()
    }
}
