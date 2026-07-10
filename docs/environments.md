# Environments and images

Built-in environment manifests live in `assets/environments` and use schema version 1. Custom files in `$XDG_CONFIG_HOME/codex-start/environments` use the same strict schema and may replace a built-in by name. Run `codex-start env show NAME` to inspect the fully inherited result.

## Built-ins

| Name | Detection marker | Added behavior |
| --- | --- | --- |
| `generic` | fallback | Node.js 24, Python/build tooling, Git/GitHub/GPG/SSH, diagnostics including `socat`, Codex, shared npm and GitHub CLI state |
| `web` | `package.json` | generic plus loopback ports 5173 and 4173 |
| `uv` | `pyproject.toml` | generic plus pinned uv and `just`, a run-scoped virtualenv, full workspace/group sync, and a project uv cache |
| `rust` | `Cargo.toml` | pinned Rust 1.97, rustfmt, Clippy, rust-analyzer, LLVM/debuggers, Node.js, `socat`, and project Cargo/target caches |

Detection only proposes an environment and never writes project configuration. An explicit CLI or project value wins; `generic` is the fallback.

## Schema and inheritance

After inheritance, an environment has exactly one of `image` or `[build]`. `image` must be an OCI reference with an explicit non-`latest` tag (for example `registry.example.test/team/codex:1.4.2`) or a complete `@sha256:` digest containing 64 hexadecimal digits. Untagged, empty-tag, `latest`, whitespace-containing, and non-SHA-256 digest references are rejected. A build specifies `context`, `dockerfile`, optional `target`, and `[build.args]`. A relative build context is resolved from the defining manifest, then a relative Dockerfile is resolved from that context. Relative bind-mount sources are resolved from the manifest that defines the mount.

Other fields are:

| Field | Purpose |
| --- | --- |
| `[settings]` | Launcher defaults below global/profile/project layers. It cannot select another environment or profile. |
| `workdir` | Absolute base for project workspaces; built-ins use `/workspaces`. |
| `user` | `USER`/`LOGNAME` value exposed after init; the helper still maps the numeric host UID/GID. |
| `markers` | Project-relative paths that must all exist. |
| `prepare` | Program/argv commands run by the Rust init helper before the workload. |
| `env` | Declared non-secret environment values. Secret-bearing names are rejected. |
| `secret_refs` | Child environment-variable names mapped to globally defined secret providers. |
| `allow_hosts` | Egress and browser authority rules contributed by the environment. |
| `mounts` | Bind, named-volume, or tmpfs mounts with stable IDs. |
| `caches` | Managed named volumes with stable IDs and lifetime scopes. |
| `ports` | Loopback-default port publications. |
| `host_services` | Host or network endpoints exposed on container loopback. |

See the schema-validated [custom environment example](examples/environment.toml). Arrays replace inherited arrays unless they are stable-ID resources. `mounts`, `caches`, `ports`, and `host_services` merge by `id`; `remove = true` removes the inherited item.

Cache scopes are:

- `shared`: one named volume across environments and projects;
- `project`: one volume per canonical project identity;
- `environment`: one volume per environment;
- `run`: a fresh volume removed after the run.

Preparation commands are arrays of a program and arguments and never invoke a host shell. They receive resolved secret-reference environment variables from the init helper. Ports default to `127.0.0.1`; use a broader `host_ip` only when the service must be reachable beyond host loopback.

Values in `env`, build arguments, and preparation argv are literal configuration. Known credential-shaped names/assignments are rejected, but arbitrary data cannot be classified reliably; do not place a credential under an innocuous name. Use `secret_refs` for every sensitive value.

`run` validates every marker and executes preparation before Codex. `shell` deliberately skips marker validation and preparation, so it can open an environment for recovery or bootstrap work even when the selected project marker is absent.

## Host services

Each `[[host_services]]` entry has:

- `id`: stable inheritance key;
- `host`: target host, defaulting to `host.containers.internal`;
- `port`: required target port;
- `container_port`: container-loopback listen port, defaulting to `port`;
- `container_host`: optional hostname mapped to container loopback;
- `allow_private`: permit a non-loopback target through the private-address policy;
- `remove`: delete an inherited declaration.

Host-loopback targets (`localhost`, loopback IPs, the runtime host-gateway names, or `codex-start-host`) use a per-run authenticated Rust relay unless `network = "host"` makes a direct, unremapped loopback connection possible. Other targets use the allowlist sidecar's CONNECT path in allowlist mode and a direct Rust TCP forwarder in bridge/host modes. Duplicate container listener ports and host-network port remapping are rejected. Offline mode disables every declared host service.

The automatic local-provider feature uses the same authenticated loopback relay: `--oss` defaults to host Ollama on 11434, while `--local-provider=ollama` and `--local-provider=lmstudio` select 11434 or 1234. A declaration that conflicts with one of those listener ports is rejected.

## Custom image contract

codex-start overrides the workload entrypoint and invokes:

```text
/usr/local/bin/codex-start-init run --spec /run/codex-start/init/spec.json
```

A custom `image` or `[build]` must therefore provide a Linux binary at `/usr/local/bin/codex-start-init` built from the same codex-start release as the host launcher. The image must start as root so init can prepare mount ownership and then drop to the mapped host UID/GID. It must also provide `/home/codex`, the selected environment tools, and `codex` on `PATH` for `run`; the default `shell` command additionally expects `bash -l`.

If `forwarding.host_ssh` is enabled and allowlist networking is used, the image also needs the matching `/usr/local/bin/codex-start-host-ssh`. Browser and OAuth client-side functions are subcommands of `codex-start-init`. The allowlist egress proxy is a separate content-addressed sidecar image and need not be copied into the workload image. The shipped environment Dockerfile is the reference implementation of this contract.

## Image selection and updates

`assets/images.lock.toml` records the exact base-image platform digests, uv checksums, and toolchain versions embedded in the binary. Built-in environment builds install the current Codex npm release; the user lock timestamp is included in their build arguments so `env update` refreshes their content-addressed tags. Built-in local image tags hash the fully resolved environment, architecture, defining manifests, build context, and lock-derived arguments. The Rust egress sidecar has its own content-addressed tag derived from its Dockerfile, Cargo lock, proxy/core sources, and build arguments.

Normal behavior is:

- a build-backed environment is built only when its content tag is absent, or with `--rebuild` (which also disables the build cache);
- an `image` reference is pulled when absent, or every time with `--pull`;
- `--pull` on a shipped build-backed environment pulls `codex-start-NAME:v<codex-start-version>` from `CODEX_START_IMAGE_REGISTRY`, defaulting to `ghcr.io/cofob`;
- `--pull` is rejected for a custom build-backed environment; use `--rebuild` instead;
- the allowlist sidecar is built locally from the embedded bundle when its content tag is absent, or rebuilt with `--rebuild`.

`codex-start env update --check` compares `$XDG_CONFIG_HOME/codex-start/images.lock.toml` with the lock embedded in the installed binary. `codex-start env update` copies that embedded lock into the user configuration directory. It does not query registries or discover newer upstream releases; install a newer codex-start binary first when you want a newer embedded lock. Ordinary runs never rewrite this file.

Repository maintenance commands verify drift and calculate release fingerprints:

```console
cargo run --locked --package xtask -- validate
cargo run --locked --package xtask -- build-args
cargo run --locked --package xtask -- image-tag rust arm64
```
