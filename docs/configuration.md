# Configuration

codex-start uses strict, schema-versioned TOML for launcher settings and passes native Codex settings through without maintaining a second Codex schema. Unknown launcher keys are errors; arbitrary keys below `[settings.codex.config]` are accepted and rendered as deterministic `-c key=value` arguments. Recognized secret-bearing keys reject literals, and static native HTTP-header tables are prohibited regardless of header name; use an environment-variable reference instead.

## Files and precedence

From highest to lowest precedence:

1. command-line options;
2. `CODEX_START__...` environment variables;
3. project settings;
4. the selected codex-start profile;
5. global settings;
6. environment defaults;
7. built-in defaults.

The global document is `$XDG_CONFIG_HOME/codex-start/config.toml` (normally `~/.config/codex-start/config.toml`). A Git project uses `<git-common-dir>/codex-start.toml`, so one setting is shared by its linked worktrees without becoming a tracked file. A non-Git project uses `$XDG_CONFIG_HOME/codex-start/projects/<canonical-path-blake3>.toml`.

`codex-start config show` prints the merged, redacted value. `codex-start config explain` reports the layer that supplied every value. Launches never create or rewrite settings; use `config init` or `config set` to persist them.

Bare `codex-start config` opens an interactive project/global editor when stdin and stderr are terminals and human output is selected. It covers environment, runtime, network, worktree, home, TTY, and rebuild settings. Each choice can be made explicit or reset to `inherit`; edits are staged and the selected document is atomically updated only after Save confirmation. Use `config edit` for advanced tables and `config set` for scripts or other non-interactive use.

The schema-validated global example is [examples/config.toml](examples/config.toml), and a valid project-only document is [examples/project.toml](examples/project.toml).

## Documents and launcher settings

Every document begins with `schema_version = 1`. Global documents may contain `settings`, `profiles`, `homes`, and `secrets`. Project documents may contain only `settings`; definitions remain global so a repository cannot replace a trusted secret provider or redirect a Codex home.

Arrays replace lower-precedence arrays. String-keyed maps deep-merge. Environment mounts, caches, ports, and host services merge by stable `id`; a child manifest removes an inherited resource with `remove = true`.

The scalar settings are:

| Key | Values or purpose |
| --- | --- |
| `profile` | Select a globally defined codex-start profile. |
| `environment` | Select a built-in or user environment. |
| `runtime` | `auto`, `docker`, or `podman`. |
| `network` | `allowlist`, `offline`, `bridge`, or `host`. |
| `worktree` | `auto`, `always`, or `never`. |
| `home` | Select a globally defined Codex home. |
| `name` | Reusable worktree/container name. |
| `publish` | Port specifications such as `127.0.0.1:8080:80/tcp`. |
| `rebuild` | Rebuild build-backed environment and sidecar images. |
| `tty` | `auto`, `always`, or `never`. |
| `workdir` | Absolute container working-directory override. |
| `allow_hosts` | Additional egress/browser authority rules. |
| `allow_ssh_hosts` | Host-SSH authority rules; ports default to 22. |
| `secret_refs` | Map child environment-variable names to global providers. |

`[settings.resources]` applies typed limits to the primary Codex workload container. It does not constrain the egress sidecar or host bridge processes. Every field is optional; when the table is absent, Docker or Podman retains its normal defaults. Resource fields follow normal per-field configuration precedence and may also be supplied by an environment manifest's `[settings.resources]` table.

```toml
[settings.resources]
cpus = 4.0
cpu_shares = 1024
cpuset_cpus = "0-3"
memory = "8g"
memory_reservation = "4g"
memory_swap = "10g" # total memory plus swap; use "-1" for unlimited swap
pids_limit = 512     # use -1 for unlimited processes
shm_size = "1g"

[settings.resources.ulimits]
nofile = "65536:65536"
memlock = "-1:-1"
```

`cpus` is a positive fractional CPU count. `cpu_shares` accepts 2 through 262144, and `cpuset_cpus` accepts comma-separated CPU numbers and ascending ranges such as `0-3,6`. Memory and shared-memory values are positive integers with an optional `b`, `k`, `m`, or `g` binary unit; the hard memory limit is at least `6m`, a reservation must be lower than the hard limit, and a finite swap total must be at least the hard limit. Supported ulimits are `core`, `cpu`, `data`, `fsize`, `locks`, `memlock`, `msgqueue`, `nice`, `nofile`, `nproc`, `rss`, `rtprio`, `rttime`, `sigpending`, and `stack`, using `soft[:hard]` syntax with `-1` for unlimited.

The resolved limits appear in `run --dry-run` output and apply equally to `run`, `shell`, and `merge`. A `--runtime-arg` that repeats a configured typed limit is rejected instead of relying on engine-specific duplicate-option precedence; non-conflicting expert options remain available. Enforcement ultimately depends on the selected engine and host cgroup support, particularly for rootless Podman.

`[settings.merge].model` selects the Codex model used only by `codex-start merge`; it defaults to `gpt-5.6-terra`. The equivalent environment override is `CODEX_START__MERGE__MODEL`, and the command's `--model` option has highest precedence.

Rootless Podman reserves the engine's user, user-namespace, UID/GID-map, and subordinate-ID options. codex-start supplies `--userns keep-id:uid=<workload-uid>,gid=<workload-gid> --user 0:0` itself: the explicit root user is only for the container init helper, which then runs preparation and the final workload as the mapped UID/GID. Conflicting `--runtime-arg` values are rejected instead of producing a checkout that appears writable but fails at runtime. A remote rootless service gets the same explicit target mapping, which avoids assuming its service account shares the client's numeric ID. Docker and rootful Podman are unchanged.

Environment overrides use the same nesting with double underscores. Values are parsed as TOML scalars or arrays:

```console
CODEX_START__NETWORK=offline codex-start run
CODEX_START__FORWARDING__SSH_AGENT=false codex-start run
```

## Profiles and Git behavior

Profiles are global named settings layers and may inherit another profile:

```toml
[profiles.offline.settings]
network = "offline"

[profiles.review]
extends = "offline"

[profiles.review.settings]
worktree = "always"
```

`[settings.git]` accepts `worktree_base` (an alternate host directory), `branch_prefix` (default `codex/`), and `editor` (an argv template). An editor argument containing `{path}` receives the selected worktree path; if no argument contains the placeholder, the path is appended. An empty editor vector discovers `$VISUAL`, `$EDITOR`, Zed, VS Code, then the platform opener. `$VISUAL`/`$EDITOR` are parsed into argv, and no editor invocation is evaluated by a shell.

`codex-start merge SOURCE...` fixes the target to the worktree containing the invocation directory and ignores normal automatic worktree creation. Sources are processed in argument order. An exact local branch wins; otherwise the value must name an owned worktree below `git.worktree_base`, whose checked-out `git.branch_prefix` branch becomes the source. The target and named sources must be clean and attached, duplicate/current sources are rejected, and no remote refs are fetched implicitly.

## Forwarding and host services

Forwarding features are independent and enabled by default:

```toml
[settings.forwarding]
ssh_agent = true
ssh_agent_bridge = "auto" # auto, socket, or tcp
gpg_agent = true
git_config = true
known_hosts = true
host_ssh = true
gh_config = true
browser = true
local_providers = true
git_config_file = "~/.gitconfig"            # default when HOME is available
known_hosts_file = "~/.ssh/known_hosts"     # default when HOME is available
container_ssh_dir = "/home/codex/.ssh"
ssh_user = "git"                            # omit to inherit the host USER
browser_opener = []       # empty selects /usr/bin/open or xdg-open
host_ssh_program = "ssh"
oauth_callback_port = 1455
```

`ssh_agent_bridge = "auto"` uses a direct Unix-socket bind where the runtime supports it and an authenticated TCP-to-Unix relay for the macOS Podman-machine case. `socket` requests the direct path; `tcp` forces the authenticated fallback. When no usable agent exists, codex-start attempts to start `ssh-agent`, add a default identity with `ssh-add`, and stop the temporary agent after the run. GPG is launched with `gpgconf`, then uses a direct agent socket with native Linux Docker and an authenticated fallback on macOS or Podman.

`git_config_file` and `known_hosts_file` accept an absolute host path or `~/...`; omit them to use `$HOME/.gitconfig` and `$HOME/.ssh/known_hosts`. Git configuration and known hosts are copied to private temporary files before their read-only mounts. `container_ssh_dir` must be an absolute normalized path below `/home/codex` and controls the known-hosts and generated SSH-config destinations. `ssh_user` writes a private `Host *` user setting there; when omitted, a valid host `$USER` is used. GitHub CLI state is mounted separately.

In `allowlist` mode, host SSH sets `GIT_SSH_COMMAND` to the container helper. The host endpoint authenticates every connection, accepts only Git upload/receive/archive and Git LFS transfers, parses a restricted OpenSSH argv, rejects unsafe `-o` options, and enforces `allow_ssh_hosts`. When no SSH rules are configured, the defaults are `github.com:22` and `ssh.github.com:443`. `host_ssh_program` selects the host executable and is never evaluated by a shell. `bridge` and `host` networking instead leave Git on the container's OpenSSH client and direct egress.

The browser bridge sets `BROWSER` to the init helper, accepts only authenticated HTTP(S) requests, and limits URL authorities to the resolved egress rules plus built-in OpenAI login authorities. `browser_opener` is an argv vector whose first item is the host executable and whose remaining items are fixed arguments placed before the URL. MCP OAuth derives its listener from native `mcp_oauth_callback_port`/`mcp_oauth_callback_url` settings (including final `-c` overrides), with `oauth_callback_port` as the fallback. Only HTTP loopback URLs without credentials, query, or fragment are accepted; their path is preserved. Missing native values are injected before user overrides. Non-host modes publish a run-scoped authenticated loopback tunnel; host mode uses the shared loopback namespace directly or a local alias relay. If the callback address is occupied, browser opening remains enabled and the run reports a warning.

With `local_providers = true`, Codex argv containing `--oss` starts an authenticated Ollama relay on port 11434; `--local-provider=ollama` and `--local-provider=lmstudio` select Ollama or LM Studio (port 1234) explicitly. Offline mode disables direct and fallback SSH/GPG-agent forwarding, host SSH, browser/OAuth, declared host services, and local-provider relays. Static Git, known-hosts, generated SSH-user, and GitHub CLI configuration mounts remain available offline.

Per-run bridge credentials are mounted separately at `/run/codex-start/secrets/host-services`; they are not workload secret-provider values.

## Proxy controls

`[settings.proxy]` applies common bounds to the egress sidecar and authenticated bridges:

| Key | Default | Effect |
| --- | ---: | --- |
| `listen_port` | 3128 | Egress sidecar listen port and workload proxy authority. |
| `connect_timeout_seconds` | 10 | Target connection timeout. |
| `idle_timeout_seconds` | 300 | Bidirectional relay idle timeout. |
| `max_connections` | 256 | Concurrent connection limit per service. |
| `block_private_addresses` | true | Deny private/reserved egress except explicit host-service authorities. |
| `header_timeout_seconds` | 10 | Egress HTTP header deadline. |
| `max_header_bytes` | 65536 | Maximum egress request-header size. |
| `handshake_timeout_seconds` | 5 | Authenticated bridge/CONNECT handshake deadline. |

All limits must be non-zero. Setting `block_private_addresses = false` allows any otherwise allowlisted authority to resolve privately; it does not bypass the hostname/port allowlist.

## Codex settings, MCP, skills, and arbitrary commands

The selected home is mounted at `/home/codex/.codex` and exported as `CODEX_HOME`; `/home/codex/.agents` carries user skills and plugin state. Project `.codex/`, `.agents/skills`, and `AGENTS.md` files remain in the mounted repository and are discovered by Codex itself.

Native Codex configuration belongs below `[settings.codex.config]`. Nested tables are preserved, including complete MCP server definitions:

```toml
[settings.codex]
profile = "container"
args = []

[settings.codex.config]
model = "gpt-5.4"
approval_policy = "on-request"
sandbox_mode = "danger-full-access"

[settings.codex.config.mcp_servers.docs]
url = "https://mcp.example.test/v1"
bearer_token_env_var = "DOCS_MCP_TOKEN"

[settings.codex.config.mcp_servers.docs.env_http_headers]
X-Client-Value = "DOCS_CLIENT_VALUE"

[settings.codex.config.mcp_servers.local]
command = "uvx"
args = ["example-mcp-server"]
startup_timeout_sec = 30
```

The workload argv order is `codex`, generated `-c` overrides, the configured Codex profile, `[settings.codex].args`, then the exact arguments supplied after the CLI `--`. Because the raw argv is last and byte-preserving, it can select any command or override supported by the installed Codex binary. `codex-start shell` does not prepend `codex`; it runs the supplied shell argv directly.

Native `http_headers`/`headers` tables are rejected because every static header value is a potential credential channel. Use Codex's `env_http_headers` form, whose values must be valid environment-variable names backed by global codex-start secret providers when sensitive. Ordinary arbitrary strings remain available for non-secret native Codex settings; codex-start cannot safely infer that a credential hidden in an unrelated field is secret.

The explicit form is `codex-start run [ENVIRONMENT] -- CODEX_ARGS...`. For pi-start-compatible positional use, `codex-start rust exec ...` treats `rust` as the environment because it matches a loaded manifest. If the first value is not an environment, as in `codex-start exec ...`, every positional value becomes a Codex argument and normal configured/marker-based environment selection applies.

## Homes

- `managed` stores a named `.codex` and `.agents` pair below the codex-start data directory and is the default.
- `host` bind-mounts `~/.codex` and `~/.agents` directly.
- `path` uses an explicit absolute Codex directory and optional agents directory.

Use `home import` and `home export` for deliberate migration. They take an exclusive codex-start home lock, reject overlapping or symlinked copy targets, skip codex-start's own lock and live SQLite sidecar files, and omit a SQLite database when a WAL, SHM, or rollback journal is observed during the staged copy. Ordinary project files such as `Cargo.lock` are copied.

## Secrets

Only global configuration can define secret providers. Supported providers read a host environment variable, a permission-checked absolute file, the stdout of an argv-only command, or the native keychain. Project settings and environment manifests map an environment variable needed by Codex or another tool to a trusted provider name:

```toml
[settings.secret_refs]
OPENAI_API_KEY = "openai"
```

An environment manifest uses a top-level table instead:

```toml
[secret_refs]
PACKAGE_TOKEN = "package-registry"
```

Project references override environment references for the same child variable. Every selected provider is required and resolved only for a real launch. Values are written to mode-`0600` files below a mode-`0700` temporary directory, mounted read-only at `/run/secrets`; `/run/secrets/map.json` contains only environment-variable-to-file mappings. The Rust init helper reads the files and supplies values to preparation commands and the final child process. Values are absent from engine configuration, serialized dry-run plans, logs, and errors.

## Dry-run boundary

`codex-start run --dry-run` resolves and validates configuration, environment inheritance and project markers, the logical Codex argv, mounts, cache names, ports, labels, init preparation, network/sidecar policy, declared host services, and secret provider names. It emits the same typed, redacted launch-plan schema that is validated before a real runtime request.

Dry-run does not detect or contact Docker/Podman, build or pull images, create a worktree/home/container/network/volume, resolve a secret provider, or bind a host listener. It can initialize codex-start's XDG directories and materialize the embedded build bundle in the cache. Runtime-selected socket paths, authenticated listener addresses, OAuth ephemeral ports, and an automatically generated worktree name are finalized only during launch and therefore appear as planned values or explicit warnings rather than claimed live endpoints.
