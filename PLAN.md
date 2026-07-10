# Codex Start v0 — Rust Containerized Codex Launcher

## Summary

Replace the current hello-world stub with a GPL-3.0-or-later Rust workspace that provides feature parity with [`pi-start`](/Users/cofob/.local/bin/pi-start) and the four [`~/.pi-presets`](/Users/cofob/.pi-presets) environments, while adding typed configuration, Docker/Podman portability, reproducible images, selectable Codex homes, and Rust-only proxy infrastructure.

The compatibility contract is “complete Codex CLI”: invoke the real pinned Codex binary, preserve arbitrary commands and flags, and support native config, profiles, project `.codex/` layers, `AGENTS.md`, MCP, skills, plugins, hooks, rules, app-server, mcp-server, cloud, exec, review, resume, and future Codex configuration keys. GUI-only desktop/IDE/browser/computer-use surfaces are not emulated. This follows Codex’s documented [configuration precedence](https://learn.chatgpt.com/docs/config-file/config-basic) and [`CODEX_HOME` state model](https://learn.chatgpt.com/docs/config-file/environment-variables).

Target macOS and Linux on amd64/arm64, using Docker 27+ or Podman 5.4+. Publish signed binaries and multi-architecture OCI images to GHCR.

## Public Interfaces and Configuration

### CLI

- `codex-start run [ENVIRONMENT] [runner options] -- [CODEX_ARGS...]`
  - Launch interactive Codex when no Codex arguments are supplied.
  - Pass everything after `--` unchanged and return Codex’s exit status.
- `codex-start shell [ENVIRONMENT]`
- `codex-start worktree commit|squash|move|edit|cleanup [NAME]`
- `codex-start resources list|logs|stop|cleanup`
- `codex-start env list|show|build|update`
- `codex-start home list|create|import|export|exec`
- `codex-start config init|show|explain|edit|set`
- `codex-start doctor`
- Support human and redacted JSON output, `--dry-run`, `--runtime`, `--home`, `--network`, `--worktree`, `--name`, `--publish`, `--rebuild`, and TTY controls.
- Preserve migration-friendly positional invocation and aliases for `--commit`, `--squash`, `--move`, `--edit`, `--shell`, `--cleanup`, and `--cleanup-git`.
- Retain Pi-compatible `--no-network` as a deprecated alias for `allowlist`; add `--offline` for actual zero-egress operation.

### Configuration layout and precedence

Use schema-versioned TOML:

- Global: `$XDG_CONFIG_HOME/codex-start/config.toml`
- Custom environments: `$XDG_CONFIG_HOME/codex-start/environments/*.toml`
- Git project: `<git-common-dir>/codex-start.toml`, shared by linked worktrees and untracked by Git
- Non-Git project: `$XDG_CONFIG_HOME/codex-start/projects/<canonical-path-blake3>.toml`
- Mutable managed homes/worktrees: `$XDG_DATA_HOME/codex-start/`
- Runtime/build data: `$XDG_CACHE_HOME/codex-start/`

Precedence is CLI → `CODEX_START__...` environment overrides → project config → selected codex-start profile → global config → environment defaults → built-ins. Track source provenance for every resolved field so `config explain` can show why a value won.

Reject unknown codex-start and environment fields with source spans and suggestions. Keep `[codex.config]` as arbitrary TOML and flatten it into generated `-c key=value` arguments before user-supplied Codex arguments. This preserves future Codex keys without weakening validation of codex-start’s own schema.

Arrays replace by default. Maps deep-merge. Environment resources such as caches, mounts, ports, and host services merge by stable `id`, with explicit removal supported.

### Selectable Codex homes

- `managed` is the default: a named, codex-start-owned home shared across repositories and runtimes.
- `host` directly binds `~/.codex` and `~/.agents`; this is opt-in and `doctor` warns about absolute host paths, platform-specific plugin binaries, and concurrent access.
- `path` allows additional named homes at explicit locations.
- Support explicit import/export between host and managed homes without copying live SQLite files while Codex is running.
- Preserve Codex config, auth, sessions, logs, marketplaces, plugin state, user skills, and profiles together as Codex expects. Repository skills remain naturally available from `.agents/skills`; user and plugin layouts follow the documented [skills](https://learn.chatgpt.com/docs/build-skills) and [plugin](https://learn.chatgpt.com/docs/build-plugins) structures.

Project config may select a home, environment, runtime, network/worktree defaults, Codex arguments, arbitrary `[codex.config]` values, and named secret references. Persistence happens only through `config init/set`; launches never silently rewrite project settings.

## Implementation Architecture

### Rust workspace

- `codex-start-core`: strict config types, merge/provenance engine, environment inheritance, project identity, runtime-neutral `ContainerPlan`, lifecycle state machines, and error taxonomy.
- `codex-start-host`: CLI/orchestrator plus adapters for Git, Docker, Podman, editors, keychains, filesystem, signals, and process execution.
- `codex-start-proxy`: shared proxy/protocol library and Rust binaries for the egress sidecar, container init/helper, SSH-agent transport, host-open/OAuth bridge, and local-provider tunnels.
- `xtask`: reproducible image locking, release metadata, SBOM/provenance, and cross-platform packaging.

Use Rust 2024 with MSRV 1.88. Every workspace crate declares `unsafe_code = "forbid"`. Never invoke user values through a shell: commands and hooks are argv arrays passed directly to `Command`.

### Runtime-neutral orchestration

- Build a typed `ContainerPlan` before contacting an engine. `--dry-run` renders this plan with secrets redacted.
- Docker and Podman adapters translate the same plan into CLI invocations and probe capabilities rather than depending on daemon-specific Rust APIs.
- Runtime `auto` tries an explicitly configured engine first, then healthy Docker, then healthy Podman.
- Use canonical-path hashes, per-run UUIDs, `io.codex-start.*` labels, and advisory locks for collision-free names and safe cleanup.
- Handle Ctrl-C/termination with RAII cleanup. Recovery commands remove only resources carrying matching ownership labels.
- Preserve the original repository-relative working directory while mounting the repository/worktree at a unique `/workspaces/<project-id>/<worktree>` path.
- Use the Git CLI for worktrees, signing, editors, and user configuration fidelity. Mount linked-worktree common metadata at the path required by its `.git` pointer without duplicating the entire workspace.

### Git behavior parity

- Default `worktree=auto`; support forced and disabled modes.
- Create branches under configurable `codex/`, reuse named worktrees, and remove newly created worktrees only when clean and still at their base commit.
- Preserve `commit`, autosave-and-`squash`, uncommitted `move`, editor launch, and guarded cleanup semantics.
- Require a clean target for squash/move, prevent self-application, protect untracked destinations and symlinks, and never delete branches/worktrees outside the owned namespace.
- Editor is configurable as an argv template; default resolution is `$VISUAL`, `$EDITOR`, Zed, then platform opener.

### Environments and images

Ship declarative manifests plus pinned Dockerfiles for:

- `generic`: Node 24 Bookworm, Python/build tooling, Git/GH/GPG/SSH, common diagnostics, Codex, and shared npm/GH caches.
- `web`: generic plus `package.json` validation and loopback-published ports 5173/4173.
- `uv`: generic plus pinned `uv`, `pyproject.toml` validation, fresh per-run virtualenv, `uv sync --all-packages --all-groups`, and persistent uv/Python caches.
- `rust`: pinned Rust Bookworm toolchain, clippy/rustfmt/rust-src/rust-analyzer/LLVM tools, the existing C/C++/LLVM/debugging package set, Node 24 for MCP/plugin tooling, and project-scoped Cargo/target caches.

Allow custom manifests to inherit environments and configure an OCI image or custom Dockerfile, build arguments, target, mounts, caches, validation markers, in-container preparation commands, environment, ports, host services, and allowlists. Do not support arbitrary host-sourced shell hooks.

Use a Rust init binary to map host UID/GID, initialize forwarded resources, run preparation commands, drop privileges, and `exec` Codex. Pin Codex, base images, Node, uv, and Rust artifacts with per-architecture checksums/digests. Image tags incorporate the manifest, Dockerfile/assets, architecture, and version lock hash. `env update` explicitly refreshes the user lock; ordinary runs never consume `latest`.

### Networking, proxies, and secrets

- Modes:
  - `offline`: isolated internal network, no sidecar.
  - `allowlist`: default; workload has only an internal network and a dual-homed Rust egress sidecar.
  - `bridge`: normal unrestricted engine egress.
  - `host`: engine host networking.
- The Rust sidecar supports HTTP forwarding and CONNECT without TLS interception, exact/wildcard/IDNA host rules, explicit ports, DNS validation, IPv4/IPv6, timeouts, connection limits, half-close handling, health checks, and structured denial logs.
- Derive allowed hosts from Codex/OpenAI base URLs, configured HTTP MCP servers, environment registries, and declared host services; require explicit entries for unknown stdio-MCP egress. Codex supports stdio and streamable HTTP MCP, bearer tokens, OAuth, and per-server/tool policy, so preserve its complete native tables rather than inventing a parallel MCP schema. [Codex MCP reference](https://learn.chatgpt.com/docs/extend/mcp)
- Block private/reserved resolved addresses unless they correspond to an explicitly declared host service.
- Run the egress service from a minimal non-root, read-only, capability-dropped multi-arch Rust sidecar image. Include the same Rust helper binaries in development images; no JS, Python, or `socat` proxy implementation remains.
- Keep only host-required endpoints in the main Rust process:
  - Authenticated SSH-agent Unix/TCP relay fallback.
  - Allowlisted host-SSH execution with strict argument parsing and no shell.
  - Browser opener and reverse callback tunnel for Codex/MCP OAuth.
  - Automatic loopback tunnels for host Ollama 11434 and LM Studio 1234 when `--oss`/`--local-provider` is detected.
- Prefer direct SSH-agent socket mounts where supported—Docker Desktop documents `/run/host-services/ssh-auth.sock`—and use the authenticated Rust fallback for Podman machines or incompatible sockets. [Docker networking reference](https://docs.docker.com/desktop/features/networking/networking-how-tos/), [Podman runtime reference](https://docs.podman.io/en/latest/markdown/podman-run.1.html)
- Preserve known-hosts, Git config/signing files, SSH-agent, host SSH, GPG-agent, GH config, package caches, and port-publishing behavior. Each is independently configurable.
- Define secrets only in global named providers: host environment, permission-checked file, argv-based command, or native keychain. Every schema-defined credential channel in project/environment files must reference a provider or environment-variable name; native static HTTP-header tables are rejected entirely in favor of `env_http_headers`. Arbitrary literal fields are explicitly non-secret configuration and must not be used to hide credentials.
- Resolve secrets at launch into `0600` temporary files, mount them at `/run/secrets`, and let the Rust init process set any required child environment variables so values do not appear in engine inspect output, dry-run output, logs, or errors.

### Codex security default

Inject `sandbox_mode="danger-full-access"` and `approval_policy="on-request"` as codex-start defaults: the outer container, mounts, worktree, and egress policy are the hard boundary. User Codex arguments and project `[codex.config]` may override these. `doctor` probes nested Codex sandbox support before recommending `workspace-write`, since Landlock/bubblewrap support varies inside container VMs.

## Test, Quality, and Release Plan

- Unit/property tests:
  - CLI compatibility and byte-preserving Codex passthrough.
  - Config precedence, provenance, strict-key diagnostics, environment inheritance/cycles, and secret rejection/redaction.
  - Git/common-directory project resolution, path hashing, ports, mounts, resource naming, and runtime-plan generation.
  - Allowlist matching, CONNECT/HTTP framing, DNS rebinding/private-address policy, SSH argv validation, authenticated relay protocol, backpressure, and half-closes.
- Repository integration tests with temporary Git repos:
  - Empty repos, main and linked worktrees, reuse, clean auto-removal, retained changes/commits, squash, move, conflicts, untracked files/symlinks, editor templates, and safe cleanup.
- Runtime integration matrix on Linux with Docker and rootless Podman:
  - Build and smoke-test all four environments on amd64/arm64.
  - Validate markers, preparation, persistent caches, UID/GID writes, signals, TTY/shell attachment, published ports, managed homes, and direct-host-home warnings.
  - Verify allow/deny/offline/bridge/host networking, fake SSH/GPG agents, host SSH, OAuth callback, secret injection, and Ollama/LM Studio tunnels.
  - Smoke-test pinned Codex `--version`, help, features, config/profile loading, stdio/HTTP MCP, skills, plugins, hooks, and argument/exit-code propagation without requiring paid API calls.
- Add mocked adapter tests and a required release smoke checklist for Docker Desktop/OrbStack and Podman Machine on macOS arm64/x86_64.
- CI gates:
  - `cargo fmt --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic`
  - MSRV and current-stable test matrices, doctests, coverage for core/proxy logic, dependency/license/advisory checks, and unused-dependency checks.
  - BuildKit multi-arch image builds, vulnerability scan, SPDX SBOM, provenance attestation, and keyless signing.
  - Signed macOS/Linux release archives and checksums; OCI images published under the repository owner’s GHCR namespace.
- Acceptance requires no workspace `unsafe`, no runtime JS/Python/socat proxy code, no plaintext persistence through a supported secret-bearing field, no mutation of tracked project files during ordinary operation, exact parity for the enumerated Pi launcher features, and successful Docker/Podman execution of all four environments.

## Assumptions and Defaults

- Default runtime: auto, preferring Docker over Podman when both are healthy.
- Default environment: explicit project value; otherwise marker-based suggestion, with `generic` fallback. Detection never persists automatically.
- Default home: managed `default`, shared across repositories.
- Default network: allowlist.
- Default Git mode: automatic worktree with branch prefix `codex/`.
- Default container security: non-root workload, read-only/capability-dropped sidecars, outer isolation, Codex on-request approvals.
- Complete Codex CLI compatibility does not promise in-container Codex desktop/IDE GUI parity.
- Pi migration and `devcontainer.json` import are deferred beyond v0.
- The existing uncommitted stub may be freely replaced; no migration compatibility is needed for its current Rust API.
