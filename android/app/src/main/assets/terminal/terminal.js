/* xterm.js owns the terminal parser. Remote output is always bytes, never HTML. */
(() => {
  'use strict';
  const term = new Terminal({
    cursorBlink: true,
    fontSize: 14,
    fontFamily: 'monospace',
    scrollback: 5000,
    screenReaderMode: true,
    disableStdin: true,
    theme: { background: '#101418', foreground: '#e6edf3', cursor: '#e6edf3',
      selectionBackground: '#405267', red: '#ff7b72', green: '#7ee787' }
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(document.getElementById('terminal'));
  const encode = text => {
    const bytes = new TextEncoder().encode(text);
    let binary = '';
    for (const byte of bytes) binary += String.fromCharCode(byte);
    return btoa(binary);
  };
  term.onData(data => TerminalBridge.input(encode(data)));
  term.onBinary(data => TerminalBridge.input(btoa(data)));
  term.onResize(({ cols, rows }) => TerminalBridge.resize(cols, rows));
  let resizeFrame;
  new ResizeObserver(() => {
    cancelAnimationFrame(resizeFrame);
    resizeFrame = requestAnimationFrame(() => fit.fit());
  }).observe(document.getElementById('terminal'));
  window.codexTerminal = {
    write(base64, token) {
      term.write(Uint8Array.from(atob(base64), c => c.charCodeAt(0)), () => {
        if (token !== undefined) TerminalBridge.rendered(token);
      });
    },
    reset() { term.reset(); term.clear(); },
    active(value) { term.options.disableStdin = !value; if (value) term.focus(); },
    key(value) {
      if (term.modes.applicationCursorKeysMode && /^\x1b\[[ABCDHF]$/.test(value)) value = value.replace('[', 'O');
      if (!term.options.disableStdin) term.input(value, true);
      term.focus();
    },
    focus() { term.focus(); },
    blur() { term.blur(); },
    paste(value) { if (!term.options.disableStdin) term.paste(value); },
    font(value) { term.options.fontSize = value; fit.fit(); },
    fit() { requestAnimationFrame(() => requestAnimationFrame(() => fit.fit())); },
    copy() {
      if (term.hasSelection()) return term.getSelection();
      const buffer = term.buffer.active;
      return Array.from({ length: term.rows }, (_, i) => buffer.getLine(buffer.viewportY + i)?.translateToString(true) || '').join('\n').trimEnd();
    },
    // Read-only terminal state used by device acceptance tests.
    inspect() {
      const buffer = term.buffer.active;
      return { cols: term.cols, rows: term.rows, type: buffer.type, inputEnabled: !term.options.disableStdin,
        width: innerWidth, height: innerHeight, paneHeight: document.getElementById('terminal').clientHeight,
        panePosition: getComputedStyle(document.getElementById('terminal')).position,
        paneCssHeight: getComputedStyle(document.getElementById('terminal')).height,
        cursorX: buffer.cursorX, cursorY: buffer.cursorY,
        lines: Array.from({ length: buffer.length }, (_, i) => buffer.getLine(i).translateToString(true)) };
    }
  };
  fit.fit();
  TerminalBridge.ready(term.cols, term.rows);
})();
