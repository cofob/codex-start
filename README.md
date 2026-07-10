# codex-start

codex-start runs the complete OpenAI Codex CLI in reproducible, project-aware Docker or Podman environments. It preserves Codex’s own homes, config, profiles, MCP servers, skills, plugins, hooks, rules, app-server, mcp-server, cloud, exec, review, resume, and arbitrary CLI arguments while adding safe worktrees, typed launcher configuration, managed secrets, and allowlisted networking.

The host orchestrator, container init, egress proxy, and transport relays are Rust. The workspace forbids `unsafe` and treats Clippy’s pedantic lint group as a release gate.

## Install

Download the signed archive for macOS or Linux (amd64 or arm64) from GitHub Releases, verify it against `SHA256SUMS` and the adjacent Sigstore bundle, then place `codex-start` on `PATH`. Building from source requires Rust 1.88 or newer:

```console
cargo install --locked --path crates/codex-start-host
```

You also need Docker 27+ or Podman 5.4+, Git, and the usual engine VM on macOS. `codex-start doctor` validates configuration, environments, the selected home, Git discovery, and the selected runtime. `doctor --deep` also runs `codex --version` and an offline `codex sandbox -- /bin/true` probe in the selected image; a failed nested-sandbox probe is a warning because the container remains the default security boundary.

On rootless Podman, codex-start automatically maps the Podman service user to the workload UID/GID with a `keep-id` user namespace so the final non-root workload can write the bind-mounted checkout. The Rust init helper still starts as container root, remaps the `codex` account, prepares container-owned volumes, and drops privileges before running Codex. Docker and rootful Podman retain their normal identity mapping; remote rootless Podman receives the explicit workload mapping rather than assuming the client and service use the same numeric IDs.

## Quick start

```console
# Detect rust/web/uv from project markers, otherwise use generic.
codex-start run

# Select an environment and pass every following argument to Codex unchanged.
codex-start run rust -- exec --json "run the tests"

# Open a shell with the same mounts, caches, home, and network policy.
codex-start shell uv

# Opt out of the default persistent session lifecycle for one run.
codex-start run --ephemeral

# Merge ordered local branches or managed worktrees into the current branch.
codex-start merge feature-api agent-ui

# Override the dedicated merge-agent model for one task.
codex-start merge --model another-model feature-api

# Validate and print the redacted logical plan without contacting an engine.
codex-start --output json run --dry-run

# Positional pi-start compatibility: a known first value selects an environment.
codex-start rust exec --json "run the tests"

# Otherwise every positional value is passed to Codex with normal environment detection.
codex-start exec --json "summarize this repository"
```

By default a Git project runs in a reusable `codex/<name>` worktree, uses a shared managed Codex home named `default`, and has allowlisted egress. The original repository-relative working directory is preserved inside `/workspaces/<project-id>/<worktree>`.

Common lifecycle commands:

```console
codex-start worktree commit --name feature-name
codex-start worktree squash --name feature-name
codex-start worktree move --name feature-name
codex-start worktree edit --name feature-name
codex-start worktree cleanup
codex-start worktree list
codex-start resources list
codex-start resources logs RUN_ID
codex-start resources stop RUN_ID
codex-start resources cleanup
codex-start session list
codex-start session attach SESSION
codex-start session logs SESSION --follow
codex-start session refresh SESSION
codex-start session stop SESSION
codex-start session recovery enable
```

Run bare `codex-start session` or `codex-start worktree` in a terminal to open the corresponding full-screen manager. The managers provide filtering, details, refresh, and context-sensitive lifecycle actions; force removal and force cleanup remain explicit CLI-only operations. The `session list` and `worktree list` subcommands remain suitable for scripts and support `--output json`.

`run` is session-managed by default. A bare interactive run keeps a Codex app-server in the workload and reconnects TUI clients to it; an explicit command after `--` runs as a managed background job. Closing or killing the client terminal does not stop either kind. Interactive sessions can restart and resume their most recent project-scoped Codex thread; non-interactive jobs are deliberately never replayed after a reboot. Use `--ephemeral` for the previous foreground disposable lifecycle.

Persistent sessions force the authenticated SSH-agent relay so `session attach` and `session refresh` can retarget new connections to the caller's current `SSH_AUTH_SOCK`. The container-side socket remains stable. Normal TUI exit prompts to detach or stop; terminal loss detaches implicitly.

Cross-reboot recovery is opt-in. `session recovery enable` installs a user launchd service on macOS or systemd user service on Linux. It waits for the configured Docker or Podman engine rather than starting the engine itself, then reconciles interactive containers. Rootless Podman therefore requires an active user systemd instance. This first implementation restores the container/app-server path; recreating host-side SSH, browser, OAuth, and declared-service listeners after a full host reboot is still pending. SSH-agent retargeting works while the detached session supervisor survives (including terminal loss and explicit stop/restart).

`worktree cleanup` refuses dirty managed worktrees and deletes only merged owned branches; add `--force` to remove dirty worktrees and unmerged owned branches. `resources cleanup` removes stopped/stale owned resources and reports running workloads as skipped; add `--force` to stop and remove running workloads too.

`merge` requires a clean, attached current branch and clean named source worktrees. Each source first resolves as an exact local branch and otherwise as a codex-start-managed worktree name. Codex merges sources in argument order, resolves conflicts, repairs integration failures, runs relevant checks, and must leave committed clean history. Failed or blocked runs preserve the repository for inspection; codex-start never resets or aborts it automatically.

Migration aliases from pi-start are accepted, including `--commit`, `--squash`, `--move`, `--edit`, `--shell`, `--cleanup`, `--cleanup-git`, and `--no-network`. The last is a deprecated name for allowlist mode; `--offline` means no egress. In positional compatibility mode, a first value matching a loaded environment selects it and the remainder is passed to Codex; when it does not match an environment, all values are passed to Codex with the configured or detected environment.

## Environments

| Environment | Includes | Defaults |
| --- | --- | --- |
| `generic` | Node 24, Python/build tools, Git/GH/GPG/SSH, diagnostics including `socat`, Codex | shared npm/GH caches |
| `web` | generic | requires `package.json`; loopback 5173/4173 |
| `uv` | generic plus pinned uv and the `just` task runner | requires `pyproject.toml`; fresh venv; full sync |
| `rust` | pinned stable Rust 1.97/Clippy/rustfmt/analyzer, LLVM/debuggers, Node 24, `socat` | requires `Cargo.toml`; project Cargo/target caches |

Base images and non-Codex artifacts are pinned in [assets/images.lock.toml](assets/images.lock.toml) by version, digest, or checksum for Linux amd64 and arm64. Built-in environment builds install the current Codex npm release. Custom schema-v1 manifests can inherit a built-in and configure an image/build, argv-only preparation, mounts, cache scopes, ports, host services, environment and secret references, markers, and egress hosts. A custom prebuilt `image` must use an explicit non-`latest` tag or a complete `sha256` digest. It must contain the version-matched `codex-start-init` helper at `/usr/local/bin/codex-start-init`; allowlist-mode host SSH also requires `/usr/local/bin/codex-start-host-ssh`. See [environment documentation](docs/environments.md).

Normal runs reuse content-addressed local builds. `--pull` fetches versioned built-in images from `${CODEX_START_IMAGE_REGISTRY:-ghcr.io/cofob}` or refreshes a custom `image` reference; custom `[build]` environments use `--rebuild` instead. `env update` copies the lock embedded in the installed binary to the user config; its timestamp refreshes built-in image tags, so the next run rebuilds and installs the current Codex release.

## Configuration

Global settings live at `~/.config/codex-start/config.toml`. A Git repository stores private shared defaults in `<git-common-dir>/codex-start.toml`; a non-Git project uses a canonical-path hash below `~/.config/codex-start/projects`. Ordinary launches never write settings.

Run `codex-start config` in a terminal for a guided editor of common project or global settings. It shows each layer's explicit value alongside the effective value and source, stages changes until confirmation, and offers `inherit` to remove an override. Advanced settings remain available through `codex-start config edit` and typed updates through `codex-start config set`.

```toml
schema_version = 1

[settings]
environment = "rust"
runtime = "auto"
network = "allowlist"
worktree = "auto"
home = "default"

[settings.resources]
cpus = 4.0
memory = "8g"
pids_limit = 512
shm_size = "1g"

[settings.merge]
model = "gpt-5.6-terra"

[settings.secret_refs]
DOCS_MCP_TOKEN = "docs-mcp"

[settings.codex.config]
model_reasoning_effort = "high"

[settings.codex.config.mcp_servers.docs]
url = "https://mcp.example.test/v1"
bearer_token_env_var = "DOCS_MCP_TOKEN"

[secrets.docs-mcp]
kind = "environment"
variable = "DOCS_MCP_TOKEN"
```

Precedence is CLI, `CODEX_START__...` variables, project, selected profile, global, environment defaults, then built-ins. `config show` renders the result and `config explain` shows each value’s source. Launcher keys are strict; `[settings.codex.config]` intentionally accepts all native/future Codex keys. The [configuration reference](docs/configuration.md) includes schema-validated examples, homes, profiles, MCP, and secret providers.

`codex-start run ENV -- ...` always executes the real `codex` binary. Native `-c` overrides, the configured Codex profile, and `[settings.codex].args` are placed before the arguments after `--`; those final arguments are retained byte-for-byte and may select any Codex command or feature. `shell` is the exception: it executes the requested shell argv directly.

`codex-start merge [--environment ENV] [--model MODEL] SOURCE...` runs a non-interactive merge agent in the current worktree. Its task model follows normal configuration precedence through `[settings.merge].model`, with `gpt-5.6-terra` as the built-in default and `--model` as the highest-precedence override.

## Network and secrets

The default `allowlist` mode gives the workload only an internal network. A non-root, read-only, capability-dropped Rust sidecar is dual-homed onto a dedicated per-run outer network and forwards allowed HTTP/HTTPS and CONNECT traffic without TLS interception. Every non-health request requires a generated per-run bearer token. A loopback-only Rust bridge receives and injects that token after engine inspection, so proxy credentials never appear in configured environment variables, engine inspection, command previews, or logs. The proxy blocks private/reserved destinations unless explicitly permitted and emits structured redacted denials. `offline`, `bridge`, and `host` modes are also available.

Browser opening, native MCP OAuth callbacks, SSH/GPG-agent fallbacks, declared loopback services, and automatically detected Ollama/LM Studio endpoints use Rust bridges. OAuth callback port/URL settings and final Codex `-c` overrides are coordinated with the host listener. In `allowlist` mode, Git SSH also uses an authenticated, destination-restricted host SSH bridge; `bridge` and `host` mode use the container's OpenSSH client directly. Every engine-reachable bridge endpoint authenticates with a per-run token mounted at `/run/codex-start/secrets/host-services`. `offline` disables host bridges and SSH/GPG-agent forwarding.

Secrets can be read from a host environment variable, permission-checked file, argv-based command, or native keychain. Projects and environments reference trusted global provider names. Values are materialized as private files below `/run/secrets` and loaded into the child environment by the init helper, so they do not appear in TOML, engine inspection, dry-run plans, logs, or errors. See the [security model](docs/security.md).

Static native Codex HTTP-header tables are intentionally rejected; configure `env_http_headers` with environment-variable names backed by global providers. Literal launcher/environment fields are for non-secret configuration only.

## Codex homes and repository features

`managed` homes are owned by codex-start and shared between repositories/runtimes. `host` directly mounts `~/.codex` and `~/.agents`; `path` selects explicit directories. Import/export commands provide coordinated migration. Repository `.codex/`, `.agents/skills`, and `AGENTS.md` remain in place, so Codex discovers them naturally.

```console
codex-start home create team
codex-start home import team --from ~/.codex
codex-start home exec team -- login
codex-start run --home team
```

## Development and release

```console
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic
cargo run --locked --package xtask -- validate
cargo deny check
```

CI also covers MSRV, current stable, Docker, rootless Podman, multi-architecture OCI builds, vulnerability scanning, SPDX SBOMs, provenance, and signing. The manual platform matrix is in [docs/releasing.md](docs/releasing.md).

## License

codex-start is licensed under GPL-3.0-or-later. See [LICENSE](LICENSE).
