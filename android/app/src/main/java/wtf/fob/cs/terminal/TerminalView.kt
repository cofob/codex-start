package wtf.fob.cs.terminal

import android.annotation.SuppressLint
import android.content.Context
import android.os.Handler
import android.os.Looper
import android.text.Editable
import android.text.InputType
import android.text.Selection
import android.view.KeyEvent
import android.view.ViewGroup
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.webkit.JavascriptInterface
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.withTimeout
import org.json.JSONObject
import org.json.JSONTokener
import wtf.fob.cs.data.*
import wtf.fob.cs.navigation.*
import java.io.ByteArrayInputStream

/** An offline terminal surface. Only bundled assets can reach the input bridge. */
@SuppressLint("SetJavaScriptEnabled", "ViewConstructor")
class TerminalView(
    context: Context,
    onReady: (Int, Int) -> Unit,
    onInput: (String) -> Unit,
    onResize: (Int, Int) -> Unit,
    private val onExitKeyboardFocus: () -> Unit = {},
    private val onModifiersUsed: () -> Unit = {},
) : WebView(context) {
    private var closed = false
    private var active = false
    private var ctrl = false
    private var alt = false
    private var nextWrite = 0
    private val writes = mutableMapOf<Int, CompletableDeferred<Unit>>()
    private val ui = Handler(Looper.getMainLooper())

    init {
        // WebView treats WRAP_CONTENT as an unbounded page even when Compose
        // measures an exact height. That makes viewport-based CSS height zero.
        layoutParams = ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT)
        setBackgroundColor(android.graphics.Color.rgb(16, 20, 24))
        settings.apply {
            javaScriptEnabled = true
            allowFileAccess = false
            allowContentAccess = false
            blockNetworkLoads = true
            domStorageEnabled = false
            mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
            setSupportMultipleWindows(false)
        }
        webViewClient =
            object : WebViewClient() {
                override fun shouldOverrideUrlLoading(
                    view: WebView,
                    request: WebResourceRequest,
                ) = true

                override fun shouldInterceptRequest(
                    view: WebView,
                    request: WebResourceRequest,
                ): WebResourceResponse {
                    val uri = request.url
                    val name = uri.path?.removePrefix("/").orEmpty()
                    val mime =
                        when (name) {
                            "index.html" -> "text/html"
                            "xterm.js", "addon-fit.js", "terminal.js" -> "text/javascript"
                            "xterm.css", "terminal.css" -> "text/css"
                            else -> null
                        }
                    if (uri.scheme == "https" && uri.host == HOST && uri.port == -1 && mime != null) {
                        return WebResourceResponse(mime, "UTF-8", context.assets.open("terminal/$name"))
                    }
                    return WebResourceResponse("text/plain", "UTF-8", 403, "Blocked", emptyMap(), ByteArrayInputStream(ByteArray(0)))
                }
            }
        addJavascriptInterface(
            object {
                @Suppress("unused")
                @JavascriptInterface
                fun ready(
                    cols: Int,
                    rows: Int,
                ) {
                    post { if (!closed) onReady(cols, rows) }
                }

                @Suppress("unused")
                @JavascriptInterface
                fun input(base64: String) {
                    post { if (!closed) onInput(base64) }
                }

                @Suppress("unused")
                @JavascriptInterface
                fun resize(
                    cols: Int,
                    rows: Int,
                ) {
                    post { if (!closed) onResize(cols, rows) }
                }

                @Suppress("unused")
                @JavascriptInterface
                fun rendered(token: Int) {
                    post { writes.remove(token)?.complete(Unit) }
                }
            },
            "TerminalBridge",
        )
        loadUrl("https://$HOST/index.html")
    }

    override fun dispatchKeyEventPreIme(event: KeyEvent): Boolean {
        // IMEs can consume hardware Escape, Tab, arrows, and Ctrl chords. The
        // terminal owns these keys; Android Back still closes the keyboard.
        val control = terminalKeyInput(event.keyCode, event.isCtrlPressed, event.isShiftPressed, event.isAltPressed, event.isMetaPressed)
        val exit = keyboardCommand(event.keyCode, event.isCtrlPressed, event.isShiftPressed, event.isAltPressed, event.isMetaPressed)
        if (active && (exit == KeyboardCommand.ExitTerminal || control != null && event.keyCode != KeyEvent.KEYCODE_DEL)) {
            return dispatchKeyEvent(event)
        }
        return super.dispatchKeyEventPreIme(event)
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (keyboardCommand(event.keyCode, event.isCtrlPressed, event.isShiftPressed, event.isAltPressed, event.isMetaPressed) ==
            KeyboardCommand.ExitTerminal
        ) {
            if (event.action == KeyEvent.ACTION_DOWN && event.repeatCount == 0) {
                // Finish queued terminal input and its focus changes before leaving WebView.
                script("window.codexTerminal.blur()") { onExitKeyboardFocus() }
            }
            return true
        }
        val input = terminalKeyInput(event.keyCode, event.isCtrlPressed, event.isShiftPressed, event.isAltPressed, event.isMetaPressed)
        if (input != null && event.action in listOf(KeyEvent.ACTION_DOWN, KeyEvent.ACTION_UP)) {
            if (event.action == KeyEvent.ACTION_DOWN) sendKey(input)
            return true
        }
        if ((ctrl || alt) && event.unicodeChar > 0 && !event.isMetaPressed) {
            if (event.action == KeyEvent.ACTION_DOWN) sendKey(String(Character.toChars(event.unicodeChar)))
            return true
        }
        return super.dispatchKeyEvent(event)
    }

    override fun onCheckIsTextEditor() = true

    override fun onSizeChanged(
        w: Int,
        h: Int,
        oldw: Int,
        oldh: Int,
    ) {
        super.onSizeChanged(w, h, oldw, oldh)
        script("window.codexTerminal?.fit()")
    }

    /** Keep IME composition local. Only committed text goes to the PTY. */
    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType =
            InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions =
            EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING or EditorInfo.IME_ACTION_NONE
        return object : BaseInputConnection(this, true) {
            override fun getEditable(): Editable = super.getEditable()!!

            override fun commitText(
                text: CharSequence?,
                newCursorPosition: Int,
            ): Boolean {
                if (text != null && active) sendKey(text.toString().replace("\n", "\r"))
                editable.clear()
                Selection.setSelection(editable, 0)
                return true
            }

            override fun finishComposingText(): Boolean {
                val text = editable.toString()
                if (text.isNotEmpty()) commitText(text, 1)
                return super.finishComposingText()
            }

            override fun deleteSurroundingText(
                beforeLength: Int,
                afterLength: Int,
            ): Boolean {
                if (editable.isNotEmpty()) return super.deleteSurroundingText(beforeLength, afterLength)
                if (active) {
                    repeat(beforeLength.coerceIn(0, 128)) { sendKey("\u007f") }
                    repeat(afterLength.coerceIn(0, 128)) { sendKey("\u001b[3~") }
                }
                return true
            }

            override fun sendKeyEvent(event: KeyEvent): Boolean = dispatchKeyEvent(event)
        }
    }

    // Use the view queue for every operation, including output from coroutine
    // collectors. This also keeps reset, output, and close in arrival order.
    private fun script(
        source: String,
        complete: ((String) -> Unit)? = null,
    ) {
        ui.post { if (!closed) evaluateJavascript(source) { if (!closed) complete?.invoke(it) } }
    }

    fun writeBase64(bytes: String) = script("window.codexTerminal.write(${JSONObject.quote(bytes)})")

    suspend fun writeChunk(bytes: String) {
        val token = ++nextWrite
        val done = CompletableDeferred<Unit>()
        writes[token] = done
        try {
            script("window.codexTerminal.write(${JSONObject.quote(bytes)}, $token)")
            withTimeout(10_000) { done.await() }
        } finally {
            writes.remove(token)
        }
    }

    fun resetTerminal() = script("window.codexTerminal.reset()")

    fun setActive(active: Boolean) {
        this.active = active
        script("window.codexTerminal.active($active)")
    }

    fun modifiers(
        ctrl: Boolean,
        alt: Boolean,
    ) {
        this.ctrl = ctrl
        this.alt = alt
    }

    fun sendKey(key: String) {
        val value = terminalModifiedText(key, ctrl, alt)
        if (ctrl || alt) {
            ctrl = false
            alt = false
            onModifiersUsed()
        }
        script("window.codexTerminal.key(${JSONObject.quote(value)})")
    }

    fun paste(text: String) = script("window.codexTerminal.paste(${JSONObject.quote(text)})")

    fun fontSize(size: Int) = script("window.codexTerminal.font(${size.coerceIn(10, 24)})")

    fun copyText(done: (String) -> Unit) =
        script("window.codexTerminal.copy()") {
            done((JSONTokener(it).nextValue() as? String).orEmpty())
        }

    fun focusTerminal() {
        requestFocus()
        val manager = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        manager.restartInput(this)
        script("window.codexTerminal.focus()") {
            post { if (!closed) manager.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT) }
        }
    }

    fun close() {
        ui.post {
            if (!closed) {
                closed = true
                writes.values.forEach { it.cancel() }
                writes.clear()
                removeJavascriptInterface("TerminalBridge")
                stopLoading()
                destroy()
            }
        }
    }

    companion object {
        private const val HOST = "terminal.cs.fob.wtf"
    }
}

internal fun terminalModifiedText(
    text: String,
    ctrl: Boolean,
    alt: Boolean,
): String {
    val modifier = 1 + (if (alt) 2 else 0) + (if (ctrl) 4 else 0)
    if (modifier > 1) {
        if (text.matches(Regex("\u001b\\[[ABCDHF]")) || text.matches(Regex("\u001bO[PQRS]"))) {
            return "\u001b[1;$modifier${text.last()}"
        }
        if (text.matches(Regex("\u001b\\[[0-9]+~"))) return text.dropLast(1) + ";$modifier~"
    }
    val value =
        if (ctrl && text.length == 1) {
            val code = text[0].uppercaseChar().code
            when {
                code in 64..95 -> (code and 31).toChar().toString()
                text == " " || text == "2" -> "\u0000"
                text == "?" || text == "8" -> "\u007f"
                text == "6" -> "\u001e"
                text == "-" || text == "7" -> "\u001f"
                else -> text
            }
        } else {
            text
        }
    return (if (alt) "\u001b" else "") + value
}

/** WebView can use control keys for browser actions. Send terminal controls directly. */
internal fun terminalKeyInput(
    code: Int,
    ctrl: Boolean,
    shift: Boolean,
    alt: Boolean,
    meta: Boolean,
): String? {
    // Preserve platform commands, clipboard chords, and AltGr character input.
    if (meta || ctrl && alt) return null
    if (ctrl && !shift) {
        if (code in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z) return (code - KeyEvent.KEYCODE_A + 1).toChar().toString()
        when (code) {
            KeyEvent.KEYCODE_SPACE -> return "\u0000"
            KeyEvent.KEYCODE_LEFT_BRACKET -> return "\u001b"
            KeyEvent.KEYCODE_BACKSLASH -> return "\u001c"
            KeyEvent.KEYCODE_RIGHT_BRACKET -> return "\u001d"
        }
    }
    val modifier = 1 + (if (shift) 1 else 0) + (if (alt) 2 else 0) + (if (ctrl) 4 else 0)

    fun cursor(final: Char) = if (modifier == 1) "\u001b[$final" else "\u001b[1;$modifier$final"

    fun tilde(number: Int) = if (modifier == 1) "\u001b[$number~" else "\u001b[$number;$modifier~"
    return when (code) {
        KeyEvent.KEYCODE_DPAD_UP -> cursor('A')
        KeyEvent.KEYCODE_DPAD_DOWN -> cursor('B')
        KeyEvent.KEYCODE_DPAD_RIGHT -> cursor('C')
        KeyEvent.KEYCODE_DPAD_LEFT -> cursor('D')
        KeyEvent.KEYCODE_MOVE_HOME -> cursor('H')
        KeyEvent.KEYCODE_MOVE_END -> cursor('F')
        KeyEvent.KEYCODE_INSERT -> tilde(2)
        KeyEvent.KEYCODE_FORWARD_DEL -> tilde(3)
        KeyEvent.KEYCODE_PAGE_UP -> tilde(5)
        KeyEvent.KEYCODE_PAGE_DOWN -> tilde(6)
        KeyEvent.KEYCODE_TAB ->
            if (ctrl || alt) {
                null
            } else if (shift) {
                "\u001b[Z"
            } else {
                "\t"
            }
        KeyEvent.KEYCODE_ESCAPE -> if (modifier == 1) "\u001b" else null
        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> if (alt) "\u001b\r" else "\r"
        KeyEvent.KEYCODE_DEL -> (if (alt) "\u001b" else "") + if (ctrl) "\b" else "\u007f"
        in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F4 -> {
            val final = ('P'.code + code - KeyEvent.KEYCODE_F1).toChar()
            if (modifier == 1) "\u001bO$final" else "\u001b[1;$modifier$final"
        }
        in KeyEvent.KEYCODE_F5..KeyEvent.KEYCODE_F12 -> tilde(listOf(15, 17, 18, 19, 20, 21, 23, 24)[code - KeyEvent.KEYCODE_F5])
        else -> null
    }
}
