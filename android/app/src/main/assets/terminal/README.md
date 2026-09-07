# Terminal assets

The APK includes xterm.js 6.0.0 and its Fit addon 0.11.0 from
https://github.com/xtermjs/xterm.js (MIT license). The LICENSE files are included.
The JavaScript and CSS libraries are unmodified npm release files. Run
`python3 scripts/vendor-terminal.py` from the repository root to restore them.
The script pins versions and verifies the npm SHA-512 archive checksums.

The terminal loads only these local assets. It has no network or file access.
Terminal bytes use the existing authenticated Codex RPC connection.
