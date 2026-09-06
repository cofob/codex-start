# Desktop and VS Code adapter

From version 0.2, the matching installer, release packages, and `cargo install` install two executables together: `codex-start` and `codex-start-adapter`. Use the installed adapter path in both clients. No script creation, project argument, or per-project editor setting is required.

Keep both executables in the same directory. The adapter finds the launcher beside itself and passes all client arguments to `codex-start adapter -- ...`. On Unix it replaces its process, so streams, signals, and exit status pass directly to the launcher. Launcher updates keep this stable adapter entry point usable.

Do not start the adapter by itself for an interactive session. With no arguments, it starts an app-server and waits for JSON requests on stdin. Image downloads can appear before this wait. Set its path in the client, which sends these requests. Use `codex-start` for an interactive terminal session.

## Install or remove client integration

Start the terminal selection screen:

```console
codex-start-adapter install
codex-start-adapter uninstall
```

Use Space to select VS Code, ChatGPT app (Codex Desktop), or both, then Enter to apply. Escape cancels. Explicit targets skip the TUI:

```console
codex-start-adapter install --vscode --desktop
codex-start-adapter uninstall --vscode --desktop
codex-start-adapter install --all
```

`--desktop` (alias `--chatgpt`) requires macOS. `--all` selects the clients supported on the current platform. VS Code setup uses its user settings file; `--vscode-settings /path/to/settings.json` selects a different user profile or VS Code distribution and also selects VS Code. Use the same path for uninstall. `--non-interactive` fails if no target is given.

VS Code setup changes only `chatgpt.cliExecutable`. It preserves JSONC comments and other settings. Desktop setup writes `~/Library/LaunchAgents/io.codex-start.adapter.plist` and sets `CODEX_CLI_PATH` in the login environment. The LaunchAgent restores this value at login. It does not modify the application bundle. Quit and reopen Desktop, or reload the VS Code window, to use the adapter.

Setup stores receipts below the codex-start data directory in `adapter-install`. Uninstall restores the prior setting or environment value, removes its LaunchAgent, and preserves later manual override changes. A prior manual override that already points to this adapter is removed on uninstall. Setup commands do not start containers or remove the adapter binaries.

## VS Code

Set this **user** setting once, then reload the VS Code window:

```json
{
  "chatgpt.cliExecutable": "/absolute/path/to/codex-start-adapter"
}
```

Use the same setting in every project. The setting accepts an executable path, not a command with arguments. The installed adapter supplies the launcher arguments itself.

OpenAI documents `chatgpt.cliExecutable` as a development setting and states that replacing the bundled executable can affect extension features. See [the official editor settings reference](https://learn.chatgpt.com/docs/developer-settings?surface=ide).

## Desktop

The macOS Desktop build inspected for this integration reads `CODEX_CLI_PATH` when it selects the CLI executable. This is an internal override, not a documented compatibility guarantee.

Use `codex-start-adapter install --desktop` to apply the override. For a temporary manual launch, quit the app, then start its executable from a shell with the override. Use the actual application executable on your system; for a `Codex.app` installation:

```sh
CODEX_CLI_PATH=/absolute/path/to/codex-start-adapter \
  /Applications/Codex.app/Contents/MacOS/Codex
```

An already running process does not receive the new environment variable. App bundle names can differ between builds. Recheck the override after an app update. The same adapter serves requests for different projects and existing worktrees.

## Automatic project selection

The extension does not reliably start its CLI process in the project directory. The adapter therefore selects projects from app-server JSON requests:

- `cwd` selects the project and its configuration. The adapter canonicalizes the path and discovers the Git worktree root. Each root gets its own server and container.
- A thread remains associated with its server. Later requests that contain only `threadId`, including turns and approvals, go to that server.
- To restore a thread that the adapter has not seen, it reads thread metadata from the default server, then selects the recorded project directory.
- Desktop and IDE `fs/*` requests for host paths run on the host. They do not start project containers. This includes attachments, visualizations, and file watches. These RPCs come from the client connection; workload tools still run in their container. Paths below `/home/codex` retain their container meaning and use the common server. Requests with multiple `cwds`, such as skill discovery, run in the respective project servers and combine their list results.
- Requests without a project, such as initialization, account information, and model lists, use a generic server in a temporary directory. Its Codex home persists. Its scratch directory is removed when the client exits. New execution requests must specify `cwd`; they cannot start tasks in this temporary directory. CLI probes such as `--version` also use a scratch directory, so a GUI process started at `/` cannot cause the adapter to mount the host root.

The adapter preserves request and response IDs at the client boundary. It remaps server-generated request IDs internally so two project servers can request approval at the same time. Each project server receives the original initialization capabilities before it receives project requests.

Each container mounts its project at the same canonical absolute path as the host. A linked worktree also mounts its common Git metadata. The adapter does not create another worktree. Project and global settings select the environment, network policy, resources, secrets, and Codex home. Set common defaults through `codex-start config`, its global configuration file, or `CODEX_START__...` environment variables. Adapter arguments belong to Codex, not the launcher.

The client owns the server processes. The adapter disables TTY allocation, update prompts, and persistent-session management. Container preparation gets no stdin, and its output goes to stderr. The foreground runner handles SIGINT and SIGTERM. When the common client closes, the adapter closes and stops its project servers. A project server with no loaded threads, active turns, pending requests, or approval replies stops after 30 idle minutes. Closing or unsubscribing from a thread releases its server when no other work uses it. Metadata-only project servers also expire. Loaded threads remain available between turns; they are not stopped just because the user pauses. One common server remains for the client connection. Host file watches use one-second polling and report changed paths through `fs/changed`.

Set the idle timeout in seconds in the global launcher config:

```toml
[settings.adapter]
idle_timeout_seconds = 1800
```

The default is 30 minutes. The minimum is one second. `CODEX_START__ADAPTER__IDLE_TIMEOUT_SECONDS` overrides the config value. The adapter reads this connection-wide setting at startup, before selecting any project. Restart the Desktop app or reload the IDE window after changing it. Project settings do not change this connection-wide timeout.

## Images and authentication

Use a Codex image with a protocol version compatible with the client. Its init helper must include this change: older helpers can send preparation output to stdout. Build images before connecting the GUI, so startup does not wait for downloads or compilation.

For a source checkout, inspect a rebuilt environment with:

```console
codex-start adapter --project /absolute/path/to/project --rebuild -- --version
```

This is a one-off probe. With a source build, `settings.rebuild = true` selects local builds and allows the engine to reuse build layers, but it still invokes the builder at launch. A custom environment with a prebuilt versioned image avoids that step. Once an image release includes the updated helper, normal published images can be used.

The default Codex home is the managed `default` home. Authenticate it before connecting:

```console
codex-start home exec default -- login
```

Set `settings.home` in the global configuration if you need a different shared home. Host-only paths and commands in a native home must also work inside Linux; mounting credentials does not make macOS keychain access or macOS executables work in a container.

## Explicit mode and inspection

`--project` is optional. Use it with the launcher subcommand for a direct protocol test without the multi-project router:

```console
codex-start adapter --project /absolute/path/to/project --dry-run -- app-server
codex-start adapter --project /absolute/path/to/project -- app-server
```

The explicit mode forwards protocol bytes unchanged and returns the workload exit code. The automatic router parses JSON to select servers and remap request IDs. `--dry-run` prints a launch plan instead of serving a client.

## Compatibility limits

This integration is experimental. It supports macOS and Linux host paths and the app-server stdio transport. Native Windows paths are not supported; use it inside WSL instead. Socket and WebSocket listeners are not connected automatically to host listeners. See [the official app-server protocol](https://learn.chatgpt.com/docs/app-server).

The default server supplies global account and history requests. If project configuration selects a separate Codex home, that home's history is not part of the default server's list. A restored thread in a separate home needs an explicit `cwd`. Prefer one shared home for a common Desktop or IDE connection.

Each project container sees its mounted checkout. Paths in another project are not automatically available during a turn or a cross-project file copy. Host-only Desktop helper binaries and native tools also need Linux-compatible alternatives. Client features that depend on these paths or on other app-server connection state require separate validation.

Automated tests cover two projects in one connection, thread restoration, concurrent approval routing, multi-project skill lists, filesystem routing, linked worktrees, installed-executable argument forwarding, startup errors, preparation stream isolation, and foreground byte/exit-code forwarding. Protocol tests use a mock engine and server; they do not certify all features in a live Desktop or VS Code client. The protocol fixture uses Python 3 on the test host.
