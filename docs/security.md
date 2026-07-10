# Security model

codex-start treats the workload container, its explicit mounts, and its network policy as the primary boundary. The default Codex settings are `sandbox_mode="danger-full-access"` and `approval_policy="on-request"` inside that boundary. User Codex argv and native configuration can override them. Use a nested Codex sandbox only after verifying that the container runtime supplies the Linux facilities it needs.

The autonomous `merge` command explicitly disables Codex's nested approvals and sandbox because the workload container is its execution boundary. It exposes only the current target checkout read-write, the repository's shared Git directory read-write, and selected owned source worktrees read-only, in addition to the normal configured home, caches, secrets, and host services. Arbitrary worktree paths are not accepted. Source branches/commits and worktree cleanliness are captured before launch and revalidated afterward.

On a rootless Podman server, the service user's identity is mapped to the selected workload UID/GID with `keep-id:uid=...,gid=...`. The init helper is explicitly started as namespace root so it can atomically remap the image's `codex` account and prepare container-owned writable roots; every preparation command, helper, and the final Codex process runs after dropping to the mapped identity. This preserves checkout ownership without making the final workload host root, including when a remote service account and client use different numeric IDs. Identity-altering expert runtime arguments are rejected in this mode. Rootful Podman, Docker, and the non-workload sidecars do not receive this mapping.

## Network modes

- `allowlist` (default): the workload joins only an internal network. The Rust sidecar is additionally attached to a dedicated, non-internal per-run outer network rather than the engine's shared default bridge. CONNECT tunnels do not intercept TLS.
- `offline`: an internal network with no egress sidecar; host bridges and declared host services are disabled too.
- `bridge`: normal unrestricted container egress.
- `host`: engine host networking. Published ports are unavailable because host and container share the network namespace; native MCP OAuth uses that shared loopback directly or a host-loopback alias relay when its safe callback URL differs from Codex's IPv4 listener.

The egress proxy normalizes exact and wildcard IDNA host rules, checks ports, resolves both address families, rejects private/reserved results by default, and validates the selected resolved address before connecting. Every non-health request requires an exact per-run bearer token. Standard clients use a loopback-only Rust proxy bridge; container init reads the private mount before dropping privileges and passes the token only to that child, which injects authentication. `HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY` therefore contain only a credential-free loopback URL. The token value is absent from engine-configured argv/environment inspection, dry-run output, and logs. Private access requires a declared host-service authority with `allow_private = true`, an automatically authenticated host relay, or the explicit global `block_private_addresses = false` setting. Denials and health events are structured; authorization headers, URL query data, and secret values are not logged.

The sidecar runs as numeric UID/GID 65532 from a digest-pinned Debian base. Its Rust init starts as container/namespace root only long enough to read the host-owned `0600` token, loads it after the engine-visible configuration boundary, drops permanently to 65532:65532, and execs the proxy. The runtime drops `ALL` capabilities, then adds only `SETUID` and `SETGID` for that bootstrap transition; Linux clears both when init changes to the non-root identity, leaving the proxy with zero permitted/effective capabilities. The container is read-only and has `no-new-privileges`. It contains the Rust sidecar/init binaries and CA roots; environment images contain the matching Rust init and host-SSH helpers. No Python, JavaScript, or socat proxy is used. Possession of the container-engine socket remains an administrator-equivalent trust boundary.

The sidecar listen port and connection, header, handshake, idle, concurrency, and header-size limits are configured in `[settings.proxy]`; `listen_port` defaults to 3128 and is propagated to the workload proxy variables. The egress process and authenticated bridges share the applicable limits, so host integration does not create an unbounded alternate transport.

## Authenticated host boundary

Direct Unix-socket mounts are used for SSH/GPG agents when the runtime can carry them. Docker Desktop's `/run/host-services/ssh-auth.sock` is used on macOS Docker; Linux uses the discovered host socket. `ssh_agent_bridge = "tcp"` forces a TCP-to-Unix fallback, and macOS Podman `auto` selects it automatically. GPG uses a fallback on macOS and Podman.

Fallback listeners bind an ephemeral wildcard host port because a native Linux engine reaches them through its bridge gateway. A fresh per-run token is required before a listener touches its Unix or TCP target. The read-only token directory is mounted at `/run/codex-start/secrets/host-services`; this path is separate from workload secrets. Listener tasks are health-checked before the workload starts. Foreground runs shut them down with the workload; a persistent session supervisor retains them across terminal loss and explicit stop/restart, while full host-reboot listener restoration remains pending.

Host integration features are independently configurable:

- In allowlist mode, host SSH accepts only an authenticated request, a restricted OpenSSH argv for Git upload/receive/archive or Git LFS, and a destination allowed by `allow_ssh_hosts`. It rejects arbitrary remote commands, shell interpolation, control/proxy/local commands, unsafe `-o` options, and arbitrary host-side file options before starting the configured host `ssh` executable. Bridge and host networking use the container's OpenSSH client directly instead of this bridge.
- Browser opening accepts authenticated HTTP(S) URLs without credentials, enforces the resolved authority allowlist, suppresses opener output, and invokes the configured opener argv without a shell.
- OAuth resolves Codex's native `mcp_oauth_callback_port` and safe loopback `mcp_oauth_callback_url` (including ordered `-c` overrides), binds that host-loopback address, and relays through a loopback-only ephemeral publication to Codex's IPv4 listener. Host networking uses direct shared loopback or a local alias relay instead. A busy callback address disables only the callback path and produces a warning.
- Environment host services aimed at host loopback use the authenticated TCP relay. Non-loopback services use the egress CONNECT path in allowlist mode or a Rust TCP forwarder in bridge/host mode.
- `--oss` and `--local-provider` detection opens only the selected authenticated host Ollama (11434) or LM Studio (1234) loopback relay and sets its corresponding child environment variable.

Offline mode creates none of these listeners or token mounts and disables both direct and relayed SSH/GPG-agent forwarding. Static Git, known-hosts, SSH-user, and GitHub CLI configuration mounts do not create host communication channels and remain available.

## Workload secrets

Global configuration is the only place that can define a secret provider. A project or environment can reference a provider name but cannot define or persist its value. Providers read an exact host environment variable, a permission-checked regular file, the stdout of an argv-only command, or the native macOS/Linux keychain command. Secret commands do not use a shell and suppress stderr from configuration errors.

Schema-defined credential fields accept only provider/environment-variable references. Native Codex static `http_headers`/`headers` tables are rejected for every header name; `env_http_headers` values must be environment-variable names. Literal environment/build/argv settings are non-secret configuration, and credentials must not be disguised under unrelated names because no validator can infer the meaning of arbitrary strings.

For a real launch, selected values are written to mode-`0600` files in a mode-`0700` temporary directory and mounted read-only at `/run/secrets`. `/run/secrets/map.json` contains paths, not values. The Rust init helper verifies that mappings remain under the canonical secret root, rejects insecure permissions/NUL values, reads the files, and supplies the values only to preparation commands and the final workload process. The engine receives only a mount and non-secret environment.

Dry-run records provider names, target child variables, and non-secret `/run/secrets/...` paths. It does not invoke providers or create their files. Serialized launch plans reject a resolved secret value placed directly in the environment.

## Dry-run and local state

`--dry-run` does not contact Docker/Podman or create runtime resources, worktrees, homes, secret bundles, or host listeners. It validates a typed logical launch plan, including all raw Codex argv, network rules, mounts, init preparation, secret metadata, and environment host-service declarations. Host socket discovery, authenticated addresses, and OAuth ephemeral ports are finalized only during a real launch and are identified as such in plan warnings.

Persistent session records and launch bundles are stored in user-only XDG data directories. Records expose only redacted metadata; resolved secret files and relay tokens remain mode `0600`, are never placed in engine environment configuration, and are deleted with session runtime state. Session SSH forwarding always uses the authenticated relay rather than a direct socket mount. Its private target file is re-read for each new connection, permission-checked, and atomically replaced when an attachment supplies a new `SSH_AUTH_SOCK`; existing connections continue using their original target.

For `merge --dry-run`, source resolution and clean-state validation are read-only Git probes. The plan includes the current target mount, shared Git directory, read-only named source mounts, merge model, generated Codex argv, and a planned result-bundle mount; no agent, merge, or result bundle is started.

Configuration discovery can create codex-start's private XDG directories, and environment loading can extract the embedded, content-addressed build bundle into the cache. Dry-run is therefore runtime-side-effect-free, not a promise of zero local filesystem writes.

## Reporting

Please do not open a public issue for a suspected vulnerability. Use GitHub's private security advisory flow for this repository. Include the affected version, runtime/platform, configuration with values redacted, reproduction steps, and the security impact.
