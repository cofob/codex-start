# Release procedure

Releases are reproducible from a clean signed `v*` tag whose version exactly matches the Cargo workspace version. Release tags may contain a SemVer prerelease but not `+build` metadata, because the same version is used as an OCI tag. CI publishes GNU Linux, static musl Linux, macOS, and Windows archives for x86_64 and ARM64. It also produces DEB/RPM packages from the GNU binaries and APK packages from the musl binaries with nFPM 2.47.0.

The final release job runs only after all binaries, packages, and the five GHCR image indexes succeed. Every archive, package, and installer has an SPDX JSON SBOM and a keyless Sigstore bundle. `release-manifest.json` records the exact platform mapping, byte size, SHA-256 digest, bundle, and SBOM for each artifact. `SHA256SUMS`, its bundle, the signed manifest, and GitHub build-provenance attestations cover the published asset set.

GitHub assets are uploaded to a draft release and the draft is published only after every upload succeeds. A published tag is treated as immutable; a rerun may resume an incomplete draft but refuses to rewrite an already visible release.

The release workflow derives `SOURCE_DATE_EPOCH` from the tagged commit and packages staging directories with `xtask release-archive`. The command selects tar/gzip or ZIP from the output suffix. It sorts paths, normalizes ownership and modes, fixes all archive timestamps, and rejects unsafe paths; ZIP archives reject symlinks entirely. Executable files must be named explicitly, so their mode is independent of the build host:

```console
SOURCE_DATE_EPOCH="$(git show --no-patch --format=%ct v1.2.3)" \
  cargo run --locked --package xtask -- release-archive \
  staging/codex-start-1.2.3-x86_64-unknown-linux-gnu \
  codex-start-1.2.3-x86_64-unknown-linux-gnu.tar.gz \
  --prefix codex-start-1.2.3-x86_64-unknown-linux-gnu \
  --executable codex-start
```

Windows uses the same command with a `.zip` output and `--executable codex-start.exe`. Before publication, the final job validates the complete wire manifest:

```console
cargo run --locked --package xtask -- release-manifest \
  dist --version 1.2.3 --output dist/release-manifest.json
cargo run --locked --package xtask -- validate-release-manifest \
  dist/release-manifest.json --base dist
```

Manifest validation is intentionally strict: a release must contain all eight portable archives, both architectures of DEB/RPM/APK, and both installer entrypoints, each with the declared checksum, `<artifact>.bundle`, and `<artifact>.spdx.json`. The manifest uses `os: "posix"` for `install.sh`, which is shared by Linux and macOS, and `os: "windows"` for `install.ps1`.

## Maintainer checklist

1. Update dependencies and `assets/images.lock.toml` deliberately. Verify upstream OCI digests and release checksums.
2. Run `cargo run --locked --package xtask -- validate`, the full test suite, rustfmt, Clippy pedantic, cargo-deny, and unused-dependency checks.
3. Build every environment and sidecar with Docker and rootless Podman on Linux amd64; inspect the resulting image, run the no-credential smoke tests, and verify that a launcher workload creates a host-owned file in a writable bind-mounted checkout.
4. Run the platform smoke matrix below and attach its results to the release issue.
5. Use `cargo release VERSION` to create the signed commit/tag, then push the tag. The release workflow invokes the reusable image workflow and will not create the GitHub Release until every image gate passes.
6. Verify `SHA256SUMS`, `release-manifest.json`, one artifact bundle, each OCI index signature, each SPDX SBOM, and both the GitHub artifact and OCI provenance predicates before publishing release notes.

Each environment and sidecar is one `linux/amd64,linux/arm64` OCI index. CI requires exactly one manifest for each platform, smoke-runs both variants under QEMU where needed, scans each platform digest, verifies the BuildKit provenance/SBOM predicates, then signs and verifies the index digest.

Image builds first push a run-scoped staging tag. CI signs the index digest before the remaining gates and promotes it to `edge` or a commit tag only after all five images pass inspection, smoke tests, and vulnerability scans. Immutable `v*` tags are held until the release barrier has also received every binary and package and completed release-asset signing and attestation. A rerun reuses an existing release tag only after its expected workflow signature and every gate verify; it never overwrites that tag with a mutable rebuild.

## Required platform smoke matrix

| Platform | Runtime | Architectures |
| --- | --- | --- |
| Linux | Docker 27+ | amd64, arm64 |
| Linux | rootless Podman 5.4+ | amd64, arm64 |
| macOS | Docker Desktop or OrbStack | Intel, Apple silicon |
| macOS | Podman Machine 5.4+ | Intel, Apple silicon |

For each available combination, exercise `doctor`; all four environment builds; interactive run/shell and signal propagation; UID/GID file writes; managed and direct host homes; worktree create/reuse/retain/cleanup; merge-agent branch/worktree sources, conflict resolution, model override, dry-run, and preserved failure state; loopback ports; offline/allowlist/bridge/host networking; fake SSH/GPG agents; host SSH; OAuth callback; redacted secret injection; Ollama/LM Studio tunnels; and Codex `--version`, help, config/profile loading, stdio/HTTP MCP, skills, plugins, hooks, and exit-code passthrough.

No smoke test requires paid API traffic. Authentication-dependent tests use temporary fixtures and a local HTTP/MCP service.
