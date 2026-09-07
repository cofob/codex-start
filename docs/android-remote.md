# Android and remote daemon

This feature is in development. The protocol coverage map records controls and test
status separately. A form that renders does not prove that the host action works.
Do not release the feature until its acceptance entries pass.

## Host installation and connection

Build the host with `cargo +1.88.0 build --locked --release -p codex-start --bins`.
Install both `codex-start` and `codex-start-adapter` from `target/release` in the
same directory on PATH. Linux and macOS support the remote daemon.

Run `codex-start connect` to start the daemon and show its QR invitation.
Yggdrasil is on by default. The daemon downloads the public peer list, then uses
the last valid cache or the bundled snapshot if the download fails. It probes a
bounded sample and chooses the reachable peer with the lowest link RTT.

Scan the QR in the Android app. The QR grants device registration. Treat it as a
secret. Manual host entry needs `codex-start approve <code>` on the host.

Use `codex-start daemon status` to inspect the daemon and Yggdrasil state.
Use `codex-start daemon stop` to stop remote access. Use `daemon install` or
`daemon uninstall` to manage the user service. Ordinary launcher commands do not
start a remote listener.

For a direct connection, start with
`codex-start daemon start --no-yggdrasil --advertise-host HOST`.
The default port is 47321. Manual discovery checks ports 47321–47336 on the named
host. It does not scan other hosts. The app never falls back from Yggdrasil to
direct access without a user choice.

`codex-start device list` lists registered devices. `device revoke ID` removes
access for one device. `connection-password rotate` invalidates existing QR
invitations and pending registrations. Registered devices keep their access.
Revocation does not change the invitation password.

## Android navigation

The first launch shows an app guide. The empty home screen has Add server and
Settings. The top switcher lists saved servers and active tasks across connected
servers. The daemon builds each active-task snapshot from connected app servers
and running jobs. A failed session produces a partial result instead of hiding
the other tasks. The list refreshes when task and session events arrive.
Switching views keeps the other server connections open.
An existing chat requests write access when it opens. The user can change it to
read-only mode from the chat header.

A server uses its host name by default. Settings can save a different name on
that Android device. Select a server to list its projects. Add project opens the
host home directory. Open folders to browse, then select Use this folder. A
project opens its Codex chats. Chat names come from Codex; an unnamed chat uses
its first message or New chat. New chat starts a conversation in that workspace.
The list includes loaded chats that do not yet have a saved turn. The home screen
also shows recent chats from the selected server.

Chat, Changes, and Files use bottom navigation on phones and a side rail on wide
screens. Settings also has Advanced session controls for environment selection,
session restart, and logs. The app guide can be opened again from Settings.

The supplied Codex mobile screenshots and [CC Pocket](https://github.com/K9i-0/ccpocket)
informed the layout. The app uses compact project rows, server chips, separate
user message bubbles, plain assistant replies, expandable tool activity, and a
small message bar. Neutral light/dark surfaces retain Material 3 dynamic accent
colors. Approval cards keep the command or permission details visible.

After Yggdrasil authentication, the app reads the host's public peers. It checks
them while its current route stays open and moves to a reachable host peer.
Multicast discovery can form a direct Yggdrasil link on the local network. Android
supplies its IPv6 interface data and holds a multicast lock while Yggdrasil
connections are configured. Group passwords still apply to endpoint traffic.
A failed public peer is replaced even when a LAN link remains available.

## Terminal and diff views

The main menu's **Terminals** entry or Files → Terminal opens an embedded xterm.js
6.0.0 terminal. The main menu can open a host shell without a chat or session.
Select **New terminal**,
choose **Host** or **Container / session**, then select **Start shell**. An optional
command replaces the login shell. Host shells run with the daemon user's permissions;
session shells run in the selected session's environment and working directory.
The APK includes the terminal and Fit addon assets and their MIT licenses.
It does not download scripts at runtime. Run `python3 scripts/vendor-terminal.py`
to restore the pinned assets with checksum verification.

The terminal supports ANSI colours, cursor control, alternate screens, Unicode,
scrollback, native Android keyboard composition, and bracketed paste. The compact key
row has Escape, Tab, one-shot Ctrl and Alt, and arrows. The menu adds function keys,
copy selection or screen, paste, text size, rename, and full-screen mode. Window and
keyboard changes resize the remote PTY. Vim and other alternate-screen programs use
the same parser as ordinary shells.

Each paired device can keep eight named terminals open (32 per daemon). A terminal
stays active when you change tabs, leave the screen, recreate the activity, or lose
the connection. The host keeps the last 2 MiB of raw output in memory for replay;
the app reports when older output was removed. Terminal bytes and input are not
stored in the event journal. Input has a bounded FIFO and byte offsets. An unknown
delivery result stops input; check the screen before selecting **Reconnect input**.
Unsent input is never replayed automatically.

Use **Close terminal** to stop a shell and its foreground program. Device revocation
also closes that device's terminals. A daemon restart closes terminals; it does not
restore their processes or output. Older daemons retain the single-terminal screen
and show an update notice. That legacy screen closes its shell when you leave it.

The session list and extra-key design use Termux as a reference. No Termux source
is included in the app. The local reference clone is separate from this repository.

Host terminal access is full access as the daemon user, not a Codex sandbox.
Only pair devices that you trust with that access. Device ownership prevents one
paired device from reading or controlling another device's terminal windows; it
does not reduce a paired device's host shell permissions.

Changes shows unified patches with green added lines, red removed lines, neutral
file headers, and old and new source line numbers. These colours are fixed in
both light and dark themes. Working tree, staged, branch, and task-turn views use
the same renderer. The gateway disables Git colour codes in patch output.

`TerminalAndDiffTest` checks terminal parsing, input, resizing, and diff rendering
on Android. To test a real shell, its two-minute idle period, and Git output, run:

```sh
python3 scripts/test-android-navigation.py --serial DEVICE \
  --test-class wtf.fob.cs.terminal.RemoteTerminalTest
```

This uses a separate Codex app-server and a temporary workspace. It sends no
model requests.

To test host and session windows, Vim, Unicode input, Ctrl-C, and screen re-entry:

```sh
python3 scripts/test-android-navigation.py --serial DEVICE \
  --test-class wtf.fob.cs.terminal.TerminalWorkspaceTest
```

Add `--container-fixture` to use an isolated Docker container for the session
terminal. This uses the local `ghcr.io/cofob/codex-start-generic:v0.2.0-rc.1` image,
disables network access, mounts only the temporary test project, and removes the
test container on exit. It does not restart the live host daemon.

## Android build

Use Java 17, the Android SDK, platform 36, and NDK 28.2.13676358. The project pins
Gradle 8.13, AGP 8.10.1, Kotlin 2.1.20, Rust 1.88.0 and UniFFI 0.29.4. The app
supports API 28 and later. Builds include ARM64 and x86-64 native libraries.

```sh
cd android
./gradlew assembleDebug lintDebug testDebugUnitTest connectedDebugAndroidTest
```

The native build sets both maximum and common page size to 16384. JNA 5.18.1
contains the Android RELRO fix. Graphics Path 1.1.0 is pinned explicitly. Each APK
build checks all native ELF LOAD and RELRO boundaries and ZIP entry alignment.
It does not suppress Android's compatibility warning.

The standalone check is:

```sh
python3 scripts/check-android-native.py android/app/build/outputs/apk/debug/app-debug.apk
```

Test native loading on both 4 KB and 16 KB Android systems. Check the device page
size with `adb shell getconf PAGE_SIZE`. Run physical device tests for background
connections, Doze, force-stop, network changes and notifications before release.

After installing both debug APKs, run
`python3 scripts/test-android-remote.py --serial DEVICE` to test two independent
Yggdrasil daemons through the Android native API. It uses temporary host state and
removes its invitation fixture when the test ends. Use `--direct` for the emulator
host address `10.0.2.2`. The test registers two devices, restores both native
connections, checks session access, reports a network change and restores again.

For a sustained connection test, run:

```sh
python3 scripts/test-android-yggdrasil-stability.py --serial DEVICE \
  --duration 600 --report /tmp/yggdrasil-stability.json
```

This test uses two isolated daemons and the Android app's saved connections and
automatic reconnect loop. It measures request latency, leaves one connection idle
for one minute, then stops that daemon for 15 seconds. The other daemon remains
available. The test verifies that the saved device token still works after restart.
The report contains measurements, without invitation passwords or access tokens.
Use `--require-lan` when the phone and host share a LAN. This also checks that a
direct peer with each host's public key is active and carries traffic.

`python3 scripts/test-android-navigation.py --serial DEVICE --codex /path/to/codex`
checks projects, home directory browsing, server names, chat names and new-chat
creation against a separate real Codex app-server. It uses an isolated Codex home
and does not send model requests. `--screenshots DIR` saves the inspected screens.

## Protocol baseline and acceptance

The Android app uses native Material 3 screens in place of the host action menu.
The navigation drawer contains projects, sessions, plugins, skills, connections,
the library, and recent chats. Settings contain account, appearance, permissions,
memory, model provider, configuration, diagnostics, and import controls. Chat
actions include names, forks, queues, goals, review, history, and background
terminals. Message options contain the model, thinking effort, speed, and plan
mode. Forms based on schemas are used only for inputs and results owned by a
connected tool or an approval request.

The composer has one rounded surface in both writing and read-only modes. When
the available chat height is below 420 dp, the composer uses one row. Its plus
button shows attachment, model, and voice controls. Buttons retain their touch
targets. Attachments scroll across one row. Windows below 480 dp use a smaller
toolbar and compact chat tabs. The tabs are hidden while the keyboard is open.
Sheets use less padding on short displays, and longer forms scroll above the
keyboard. These rules use the available window size, including a fold pane or a
resized window; they do not depend on a phone model name.

`ConversationComposerTest` checks the disabled draft surface and controls in a
320 by 240 dp area. Both checks passed on the USB-connected Pixel 5a (API 33,
4 KB pages) on 2026-09-07. No extra emulator was started.

### Launcher profiles and settings

Open Settings, then **Codex Start settings**, to edit the host's global settings,
named profiles, or a registered project's settings. The editor uses TOML so all
launcher settings remain available, including profile inheritance and native
Codex overrides. The host validates the complete configuration before saving.
It rejects stale revisions, invalid profiles, and symbolic-link targets. Unsaved
edits need discard confirmation and stay in memory across activity recreation.
Settings text is not stored in saved-instance state, the request outcome journal,
or the Android page cache. Process death can still discard an unsaved draft.

Hosts advertise `launcherSettings` when these controls are available. Older hosts
show an update message. Settings apply to new sessions; existing sessions keep
their configuration. Open a project and select a profile to use several profiles
on the same host. Session reuse and thread-owner lookup keep profiles separate.
Chat rows show the profile name.

To check global settings, two profiles, project defaults, draft retention, and edit conflicts on a
device without changing the user's host configuration, run:

```sh
python3 scripts/test-android-navigation.py --serial DEVICE \
  --test-class wtf.fob.cs.settings.LauncherSettingsTest --screenshots /tmp/launcher-settings
```

### Chat activity, text selection, and queue

Consecutive thinking and tool calls share one compact, collapsed activity block.
Expand it to see each item, its output, and optional raw details. Agent answers
have a copy button and a **Select text** control. A long press also opens text
selection. Selection uses a separate native view so normal chat scrolling works.

The queue is visible above the message box. It supports adding, editing, moving,
removing, and sending a selected message now. Edits retain attached input. Drafts
stay in memory across rotation; closing with unsaved edits needs confirmation.
Read-only chats can view the queue but cannot change it. **Send now** asks for
confirmation, interrupts the active turn, waits for it to stop, then starts the
selected message. Other queued messages stay in their order. Loading all queue
pages is required before reordering.

This device test uses an isolated Codex app-server and a loopback-only test model.
It does not send model requests to an external service or change live user chats:

```sh
python3 scripts/test-android-navigation.py --serial DEVICE --queue-fixture \
  --test-class wtf.fob.cs.conversation.MessageQueueTest --screenshots /tmp/message-queue
```

### Data loading and live updates

`RemoteDataCache` shares in-flight reads and keeps recent results in memory for
immediate display. Queries include the server, session, method, and sorted
parameters, so one server cannot supply another server's cached results. The app
preloads projects, sessions, recent chats, three recent chat snapshots, and the
selected session's model and account data. Preloading history does not acquire
write access.

Project and session lists appear before the recent-chat reads finish. The home
page loads one bounded page per session, not the full chat history. The project
chat screen provides search and further pages. Reconnecting keeps cached lists
visible. Each server has its own request limit, so an unavailable server does not
use another server's request slots.

Pairing now keeps the established Yggdrasil route when it activates the saved
device credentials. Network changes also wake the reconnect retry loop. A closed
event channel ends the connection instead of running an empty polling loop; an
unresponsive socket is closed after three missed heartbeat intervals.

On 2026-09-07, two isolated Yggdrasil hosts connected from the USB Pixel 5a in
869 and 986 ms. Credential activation on those routes took 218 and 107 ms.
Saved connections took 863 and 3182 ms to restore. These are measurements from
one local test, not latency guarantees for other networks.

The six-minute two-host test also passed on this device: 206 successful probes
and one unavailable probe during the planned host outage. Navigation and all 24
UI/native checks passed, including keyboard focus, QR detection, and small-window
layouts. The launcher settings test passed for global settings, two profiles,
project defaults, activity recreation, and edit conflicts. All 33 Android unit tests passed.

Screens keep their current data during refresh. Active screens refresh every
30 seconds. Server events mark affected queries as stale and group refreshes over
350 ms. Text and audio deltas update the conversation directly without a network
read per token. An event received during a read triggers a further read. A failed
refresh retains the previous result and provides a retry action.

The gateway sends project, session, and device changes through the authenticated,
replayable event journal. Other connected devices receive these changes without
manual refresh. Reconnect also refreshes cached data. Idle cache entries are
bounded to 96 entries and 12 million JSON characters; active screens can
temporarily exceed that limit. Conversation and account cache data stay in memory.
Cache coalescing, isolation, failed-refresh retention, event races, and gateway
catalogue event delivery have local tests.

`protocol/codex` contains the app-server schemas from Codex revision
`ad931a45b201e3877d6ba542ba5dbbd85e7e31b4`. `protocol/tui-commands.json` records
the 60 TUI command names from the same revision. The build does not read a local
Codex source tree.

`protocol/coverage-map.json` maps the client requests, server requests,
notifications, TUI commands and three deprecated APIs. Run
`python3 scripts/check-protocol-coverage.py` to detect missing or duplicate entries.
The `--release` option also blocks release while acceptance remains incomplete.

The upstream remote-control APIs are replaced by codex-start transport and device
management. Provider credentials and model calls stay on the host. Device
attestation and provider token refresh require the host component. Windows sandbox
setup has no equivalent on the Linux/macOS daemon. Internal Codex test commands
are listed in the map and are excluded from production controls.

## Direct notifications

The user starts task monitoring from the Android app. Its foreground service uses
the daemon event journal. Completion, failure and required input have separate
notification channels. Stable notification IDs and persisted acknowledgements
prevent replay from producing repeated alerts. Android sleep and force-stop can
delay delivery. No push provider is used.

After installing both debug APKs, run
`python3 scripts/test-android-background.py --serial DEVICE --doze` to check input
alerts, resolution dismissal, completion alerts, force-stop replay, process death,
and Doze recovery. `--doze-only` checks Doze without another process-death cycle.
The test verifies that Android entered deep idle and restores its battery and idle
settings. It uses synthetic events through production TLS and journal handlers;
it does not prove that a real Codex turn completed.

On 2026-09-07, these background checks passed on the USB-connected ARM64 phone
(API 37, 4 KB pages). The full background test also passed on the API 36 emulator.
Earlier native loading and UI checks passed on 4 KB and 16 KB emulator systems.

On the Pixel 5a (API 33), input alerts, alert dismissal, background completion,
force-stop replay, and a separate forced-Doze recovery test passed. This device denied `run-as` permission to signal
the app process, so the process-death stage could not run. This is separate from
the successful force-stop test. No live model requests were sent by these checks.
All 12 APK native libraries pass ELF LOAD, RELRO, and ZIP alignment checks, including the bundled QR scanner libraries.

## Current acceptance gaps

Feature acceptance is not complete. Live local/mobile approval races with a real
Codex process, all mapped native controls, longer network trials, and signed release
packaging still need acceptance tests. Background checks with synthetic events do
not replace these tests. Source and license notices must accompany the signed
release APKs.

The native navigation fixture passed on the Pixel 5a. It exercised server and
chat renaming, the drawer, model loading, read-only drafts, draft retention across
chat tabs, and Back navigation. The composer checks also passed. The real Codex
shell test passed, including input, resizing, idle recovery, and Git output
from its temporary test project.
Host guardian overrides and device signing still require the host component.

Use `codex-start daemon restart` to stop the gateway, wait for shutdown, and start
it with the saved bind address and Yggdrasil settings. If it is stopped, the
command starts it. Device registrations and invitation secrets are retained.

Local gateway commands print readable text by default. Use `--output json` for
scripts. `codex-start connect` prints the QR, invitation link, and manual connection
parameters. The link can be pasted into the Android Invitation field. Yggdrasil
parameters include the host public key, group password, port, and connection
password. Direct parameters include the host, port, TLS fingerprint, and connection
password.

## Read and write access

Existing chats open in read-only mode. The app reads history without resuming the
chat and checks for updates every three seconds. Failed history reads show an
error in the chat. They do not show an empty task as ready.

Select **Enable writing** to send messages, steer, interrupt, or answer approval
requests. The gateway looks for a loaded copy of the task in the selected project
and uses that app-server. Desktop and Android can thus share one Codex writer.
Codex still controls turn conflicts. Selecting **Read only** disables chat write
controls on Android; it does not stop the task or disconnect the desktop client.

If an external process holds the writer lock and the gateway cannot reach that
process, close the task in the owning client, then select **Enable writing** on
Android. The gateway does not remove lock files or force a second storage writer.

If history metadata contains invalid UTF-8, or SQLite reports a malformed
database, the gateway can show saved user and assistant text from the session
transcript. This recovery view is read-only and is limited to the last 16 MiB and
1,000 text messages, with a 4 MiB response limit. Each message is limited to 512 KiB. It can omit tool details, images, and incomplete records.
It does not repair or change the database. Host database repair is still required
before writing to a damaged task. Both the host and Android app must be updated
for the `thread/snapshot` and `thread/writeAccess` gateway extensions.

## Fast Android Yggdrasil startup

Android starts its overlay without a peer-list download or RTT probe round.
For a server without saved peers, it starts three distinct peer connections from
a random order of the bundled fallback data. DNS checks run in parallel while
the app starts its server connection. Local multicast discovery remains active.

After authentication, the app reads the server's public peer addresses in the
background. It validates and saves up to eight addresses in a separate cache for
each server public key. The next launch tries these saved addresses first and
fills the remaining connection slots from the bundled fallback list. A failed
lookup or an empty response does not erase the saved addresses.

The client checks its three outbound slots every five seconds. A slot that stays
down for about ten seconds is moved to the back of the list and replaced. Working
outbound peers and LAN links stay connected. Host-peer selection also runs in the
background, so the app does not wait for it before showing the server as ready.

Run `scripts/test-android-remote.py --serial DEVICE --report /tmp/startup.json`
to check two isolated server groups, verify peer-cache persistence, and record
cold and saved-peer connection times. Timings depend on the network and peers.

## Tablets and foldable devices

The Android app uses Jetpack WindowManager to track folds and hinges while it is
visible. Layout decisions use the available app window, including split-screen
windows, system bars, and the keyboard.

- Windows at least 840 dp wide and 480 dp high show persistent project and chat
  navigation beside the current screen.
- A vertical separating fold places navigation and the task on separate sides.
  The layout excludes the hinge area and supports right-to-left navigation.
- A horizontal separating fold uses tabletop mode: the task is above the fold,
  and navigation is below it. Each region must be at least 240 dp high.
- If a fold leaves a region too small for controls, the app uses the larger safe
  region with a navigation drawer.
- Chat tabs use a rail when the content pane is at least 600 dp wide. Smaller
  panes use bottom navigation.

The selected server, project, session, chat, feature page, and pending task link
are saved across activity recreation. Message drafts already use saved state.
The main screen can resize, and the QR camera screen permits both orientations.

`AdaptiveLayoutTest` checks tablet thresholds, density, book and tabletop layouts,
small regions, right-to-left placement, and stale fold bounds after resizing.
`AdaptiveNavigationTest` checks draft retention during resize and saved-state
restoration on an Android device. On 2026-09-07, all 20 local unit tests and both adaptive navigation tests
passed on the API 37 emulator. The debug APK build, lint, and native alignment
checks also passed. Physical foldable and tablet acceptance checks must cover posture changes, split-screen resizing, camera rotation, and the
keyboard with an active remote chat.

## Attached keyboards

USB and Bluetooth keyboards can use the app controls and terminal. Select the
keyboard icon in the top bar or press **Ctrl+/** to show the shortcut list. The
app also supplies its shortcuts to the Android system shortcut helper (**Meta+/**).
This uses Android's [keyboard shortcut helper API](https://developer.android.com/codelabs/large-screens/add-keyboard-and-mouse-support-with-compose).

| Keys | Action |
| --- | --- |
| Ctrl+K | Open chat search, or focus its search field |
| Ctrl+Shift+S | Open the server and task switcher |
| Ctrl+, | Open Settings |
| Ctrl+Shift+H | Open Projects |
| Alt+Left / Escape | Close the navigation drawer, or go back |
| Ctrl+1 / Ctrl+2 / Ctrl+3 | Open Chat / Changes / Files while in a task |
| Ctrl+N | Start a chat from the project chat list |
| Ctrl+Shift+L | Focus the message field in a writable chat |
| Ctrl+Enter | Send, queue, or steer from the message field |
| Ctrl+Shift+A | Add an attachment from the message field |
| Ctrl+/ | Show keyboard shortcuts |
| Ctrl+Shift+Tab | Move focus from the terminal to app controls |

Enter and Shift+Enter add a new line in a message. Ctrl+Enter also works with the
numeric keypad. Sending uses the button's write-access, content, loading, and
busy checks. A held shortcut runs once. Tab and Shift+Tab move between the message
field, search fields, and controls. Standard text editing shortcuts remain
available. Ctrl+Alt combinations remain available for AltGr text input.

The terminal receives its own keys, including Ctrl+C, Tab, Escape, arrow keys,
Home, End, Page Up, Page Down, Insert, Delete, and F1–F12. App shortcuts do not take these keys while the terminal has focus. Press
Ctrl+Shift+Tab to focus its Keyboard button and return to app navigation.

`KeyboardShortcutsTest` checks exact modifiers, AltGr, and Enter handling.
`TerminalKeyInputTest` checks control bytes, modified navigation keys, function
keys, and the keys that must remain with WebView.
`KeyboardNavigationTest` checks app navigation, search focus, dialog isolation,
message new lines, Tab traversal, send guards, key repeats, and native shortcut
help. `TerminalKeyboardTest` sends Android key events through the terminal and
checks the resulting input bytes and focus exit.
