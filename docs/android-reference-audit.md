# Android UI reference

Reference inspected on 2026-09-07 and 2026-09-08: `/Users/cofob/Downloads/reference.xapk`.

- Package: `com.openai.chatgpt`, version `1.2026.244` (2624421).
- XAPK SHA-256: `0381494401c7f3515c50c39d5db1d057744a33756b94ac0c81861ff0d04b2faa`.
- The archive contains the base APK and ARM64, English, and mdpi splits.
- The base APK passes `apksigner verify`. This checks its signature, not its source or all runtime behavior.

The review used the package manifest and compiled Android resources, read with
Android SDK `aapt2 dump resources`. The reference was not installed over the
user's ChatGPT app. No account data, private endpoints, or authentication tokens
were used. No APK code, icons, fonts, or other assets were copied into codex-start.

The chat pass also used partial local inspection with jadx 1.5.6. The decode
reported 140 errors, so it is not a complete source review. The inspected shared
`ValdiTextSelectionGroup` component uses Android selection actions, word selection,
handles, and a movement threshold to yield to scrolling. This does not prove that
every Remote screen uses that component.

## Findings and changes

| Reference evidence | codex-start change |
| --- | --- |
| Remote task states at resource IDs `0x7f14056d`–`0x7f140570` distinguish failure, approval, input, and running. | Work overview groups tasks by attention, progress, and recent activity. All chat rows read `ThreadStatus.activeFlags`; waiting tasks do not appear to be running. |
| Project and recent sections at `0x7f14057d`–`0x7f140581`. | Work and Projects views share the preloaded home data. Search includes chat titles, project paths, and profile names. Profiles on the same host remain separate. |
| Connection messages at `0x7f1403d3`–`0x7f1403f0` distinguish offline, reconnecting, and recovery. | A compact connection notice explains what to check. Try now wakes the existing retry loop. Identity changes and rejected access direct the user to connection settings. |
| Activity disclosure at `0x7f1403a1` and `0x7f1403fc`; command and file summaries in the `codex_tool_call_*` plurals. | One compact disclosure groups thinking and tool calls. Its summary names action types. Failed or declined actions have a review indicator. |
| Queue, steer, edit, and ordering actions at `0x7f140558`–`0x7f140566`. | Keep the existing queue view, structured item editing, ordering, and force-send controls. This pass does not replace their protocol. |
| Select text at `0x7f1408ae`, stop generating at `0x7f1408cd`, and activity/reasoning collapse at `0x7f1408df`–`0x7f1408e0`. | Native inline answer selection, a small message action row, and one compact activity disclosure. Copy and a separate Select text sheet remain available. |

## Chat rendering and interaction

- User messages have a rounded surface. Assistant answers have no card border.
  Copy and More controls use small icons with 48 dp touch targets.
- Stream deltas are applied in order and published in short batches (32 ms, or
  earlier at 32 Ki characters or 256 events). Final items and recovery flush the
  pending batch first. This timing is our implementation, not a measured timing
  from the reference app.
- Markdown parsing runs off the UI thread. A per-view queue keeps the latest
  source; unchanged text is not parsed again on each UI update.
- Native selection pauses visible answer updates and automatic scrolling. New
  text is retained and appears when selection ends. Android finishes removing
  its selection handles before the text is replaced.
- A short tap opens an answer link. A long press selects text. Ordinary swipes
  still scroll the chat. The link test intercepts its action; it opens no external
  app and makes no external request.
- Thinking and consecutive tool calls share a borderless disclosure. Command
  output updates in place and is shown as selectable, literal monospace text.
  Expanded output shows a bounded tail; Copy retains the full loaded output.
- Reasoning summary and body deltas keep separate indexed parts. Command output
  updates `aggregatedOutput` without changing the command. Bounded stream tails
  do not cut a UTF-16 surrogate pair.

These are original implementations of the reference interaction patterns. Private
streaming services and logged-in layouts were not reproduced or verified.

## Work and Codex Mobile logic

[Official Work guidance](https://learn.chatgpt.com/docs/use-chatgpt) describes a
goal-to-result workflow and distinguishes hosted cloud work from local work.
[Official Remote guidance](https://learn.chatgpt.com/docs/remote) describes selecting
a computer and project, starting or steering a task, answering requests, and
reviewing results on the phone. The OpenAI Docs skill was used to check these
product boundaries before implementation.

The selected computer scrolls into view in the host selector. Pending requests
also set the chat header to an input state, without a working indicator.

The new Work view is a local task overview for **codex-start**. It is not a claim
of ChatGPT Work cloud access or compatibility with ChatGPT's private mobile
pairing service. It keeps the existing codex-start pairing, encrypted connection,
session, profile, queue, and approval protocols. A task opens the existing Chat,
Changes, and Files views. New work starts through project and profile selection.

The original home cache contained a bounded recent-chat list per session. The
host-history follow-up below replaces this limit for supported hosts. Counts and filters
cover loaded rows. Idle or unloaded state is not treated as
proof of completion. A separate “ready for review” inbox needs reliable completed
turn and read-state data before it can be added.

## Limits of this review

Compiled resources show available UI labels and states. They do not prove which
features an account can use, exact screen layout, server-side behavior, or private
service contracts. The reference's logged-in Work screens were not run. The new
layout is an original adaptation, not a pixel comparison with those screens.

Tests cover state classification, profile identity, filtering, action summaries,
and failure indicators. USB device tests cover light mode, dark mode with large
text, task selection, recovery controls, and navigation through an isolated real
Codex app-server. Full-app acceptance remains a separate, incomplete checklist.

## Verified result

- 49 Android unit tests passed.
- 18 targeted tests passed on the USB-connected Pixel 5a (Android 13): four chat
  presentation tests, two composer tests, three Work UI tests, seven navigation
  state tests, one full navigation fixture, and one queue fixture. The queue test
  uses a local test model, not an external model or user chat.
- Debug and release builds, Android lint, ktlint, and 16 KB native-library checks passed.
- The universal release APK includes ARM64 and x86-64, uses the local test key,
  and passes APK signature and ZIP alignment checks.
- The first full navigation attempt stopped when the phone slept. The test now
  keeps only its activity awake; the final run passed. No global display setting
  was changed.
- The live codex-start daemon was not updated or restarted.
- Screenshots confirmed inline selection, clean handle removal after streaming,
  and literal command output. One test attempt failed while the phone was asleep.
  After waking it, the final 16-test batch and both server fixtures passed.
- Chat-pass APK SHA-256 (before projectless work):
  `f5b42ece0ac08996cf7bf7da0b037a6fe4e0b6bd68481ce9e7b2c9c9ebd0e7cc`.

## Projectless work follow-up (2026-09-08)

New work can now create its own host project under `~/Documents/Codex`, choose a
profile, and open the chat composer. See [the work flow](android-remote.md#work-without-an-existing-project).
This is a codex-start feature, not a claim about the reference app's private APIs.

- 26 host remote tests and 49 Android unit tests passed.
- Six targeted USB tests passed: four Work UI tests, the new projectless-work
  fixture, and the full navigation fixture.
- The work fixture verifies actual folder creation, Git setup, retry identity,
  profile routing, the first message's working directory, and interruption.
  Its host folders and app-server are isolated. It uses a loopback-only test model.
  Session startup reuses prepared fixture sessions; it does not test a cold
  container build for each profile.
- Android lint, ktlint, both release builds, APK signature verification, and
  16 KB alignment checks passed. Host Clippy completed with existing warnings in
  `runtime.rs` and the container test; strict warning-free Clippy is not claimed.
- The first work fixture attempt found a JUnit return-type error in the test.
  It was fixed before the passing device run.
- The live host daemon was not replaced or restarted. It must be updated to
  advertise `workProjects` before the new button is enabled on that host.
- Updated APK SHA-256:
  `8b0fb094078df3a463a698b18803ed0e414b66c62236ec65eff9efa950e6625f`.

## Host history follow-up (2026-09-08)

The missing-chat fault had several causes:

- Home discovered history only through known, running codex-start sessions.
- Work read the first 50 chats from each session. Search used at most eight sessions.
- The source allowlist included CLI, VS Code, and app-server, but excluded exec
  and agent threads. VS Code itself was not excluded by that list.
- A project query used only the execution path, not the host path.
- The host `.codex` and managed `.codex` are separate stores.
- A read-only query of the host `state_5.sqlite` failed with
  `database disk image is malformed`. No repair or database change was made.

The new host-wide reader and its limits are described in
[All local chats](android-remote.md#all-local-chats-and-saved-history). Compact
single-line titles, source/home labels, pinned grouping, archive selection,
search, and paging extend the reference-inspired Work layout. Saved read-only
chats hide unusable queue and composer controls. The layout remains an original
adaptation; no logged-in reference screen or private API compatibility is claimed.

A read-only local audit reached all pages of 2,213 active saved copies across 116
working folders. This includes separate home copies, not 2,213 unique thread IDs.
The earlier audit found 2,211 copies across 115 folders; host history changed
during the tests.
The first debug scan fell from 36.7 seconds to 10.0 seconds after limiting summary
reads to 256 KiB per changed file. These are local debug measurements, not release
startup guarantees. The daemon now preloads this index while a phone connects.

New tests cover all source kinds, two homes with the same thread ID, more than
50 chats, archives, search, host/container path matching, damaged metadata,
database-only threads, tool items, forward/reverse transcript pages, removal,
and symbolic-link replacement. The local audit prints counts only; it does not
print user messages or copy history into the test fixture.

Verified checks for this follow-up:

- 31 host remote tests, 12 home-storage tests, and the separate read-only audit passed.
- 52 Android unit tests passed. Debug and release builds, lint, ktlint, APK
  signature, ZIP alignment, and all 12 native-library 16 KB checks passed.
- Eleven targeted USB tests passed: two history tests, one complete navigation
  fixture, four Work UI tests, and four chat presentation/selection tests.
- The new main-navigation test checks that saved chats do not show unsupported
  Files, Changes, queue, or message controls. Normal connected-chat navigation
  still includes those controls.
- The fixture harness originally accepted only `OK (1 test)`. Android reported
  both history tests as passed, but the harness rejected `OK (2 tests)`. The
  success check now accepts a positive test count. The final fixture runs passed.
- Clippy completed with the existing runtime and container-test warnings, plus
  a function-length warning in the gateway dispatcher. Warning-free Clippy is
  not claimed.
- Release APK SHA-256:
  `b7d10e2dd85e5f7147de2606137936108726608f1737d65d770af2739c3aec36`.
- At the end of this test pass, the updated host binary was built but not
  installed. The approved host update is recorded below.

## Host installation follow-up (2026-09-08)

The user approved the host update and daemon restart.

- Rebuilt `codex-start` and `codex-start-adapter` from the current workspace with
  Rust 1.88.0 and the locked release dependencies. Installed both in
  `/Users/cofob/.local/bin` through the source-install workflow.
- Both installed executables match the release build byte for byte. The version
  remains `0.2.0-rc.1`; this is the updated local source build.
- Saved the previous executables and installation receipt in
  `/Users/cofob/.local/share/codex-start/install-backup.rV2M4NQ1`.
- `codex-start daemon restart` completed. The replacement daemon responds through
  local control and HTTPS discovery, and reports `hostHistory` and `workProjects`.
- Yggdrasil reconnected. The daemon identity, TLS fingerprint, saved options, and
  installation receipt are unchanged. All six previously running project
  containers and the checked desktop adapter processes remain running.
- No Codex history database was repaired or replaced.

## Chat scrolling follow-up (2026-09-08)

The native Markdown view previously started empty and parsed asynchronously on
every first binding. Lazy-list disposal and recreation could thus measure an
answer with the wrong height during scrolling. The first binding now supplies
formatted text before measurement. A bounded syntax-tree cache avoids parsing
recent answers again. Streaming updates still parse off the UI thread; obsolete
results are discarded.

Each view gets separate drawing and selection spans. Markwon 4.6.2 changes
ordered-list counters during rendering; those counters are restored under a
document lock before the cached tree is reused. The first device run caught that
cache regression through a pixel mismatch. The final repeated-scroll comparisons
pass with unchanged list numbers. A test expectation for a native collapsed
caret, and a streaming fixture that targeted an answer already scrolled out of
view, were also corrected before the final run.

The renderer no longer resets unchanged text when selection closes. It clips
drawing to its bounds and uses the Compose text scale. Automatic scrolling checks
for a user scroll or selection again after waiting for layout.

- All eight chat device tests passed on the USB-connected Pixel 5a, Android 13.
  These cover immediate full-height binding, cache isolation, repeated scrolling
  with identical viewport pixels, streaming position, dark mode with 1.5x text,
  grouped tool output, native selection, copy, and links.
- The complete navigation fixture also passed on the same phone. No emulator
  was used for these acceptance checks, and no external model calls were made.
- All 52 Android unit tests, lint, ktlint, debug/release builds, APK signature,
  ZIP alignment, and all 12 native-library 16 KB checks passed.
- Release APK SHA-256:
  `3dcf8cb992f758debf7dd4ead210af56c6f5a87c0000583c42edc245d69610d4`.

## Compact app navigation follow-up (2026-09-08)

Phone and short-window workspace views no longer reserve space for the app's
bottom Chat/Changes/Files tabs. The **Workspace views** button in the existing
top bar opens Chat, Changes, Files, and Terminals. It also provides keyboard
shortcut help. Wide layouts retain their side rail.

Android navigation and status bars are unchanged. No immersive-mode or system
bar-hiding code is included. The user clarified that only app navigation should
be hidden before the device build was installed.

- The complete navigation fixture passed on the USB Pixel 5a. It checks the
  top-bar menu, Chat, Changes, Files, embedded and standalone terminals, Back,
  and draft retention. It checks that Android navigation and status bars remain
  visible in those views.
- Five navigation-state tests and eight chat rendering/selection tests also
  passed on the phone: 14 device tests in total.
- All 52 Android unit tests, lint, ktlint, debug/release builds, APK signature,
  ZIP alignment, and all 12 native-library 16 KB checks passed.
- Release APK SHA-256:
  `022f259751a04a66e4d4f063e228f9897b1c242f6843dd7eac3e81472267e9cc`.
