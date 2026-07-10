# Release procedure

Releases are reproducible from a clean signed tag. CI publishes macOS/Linux archives for amd64/arm64 and multi-architecture environment/sidecar images to GHCR. Binary archives and their SPDX SBOMs receive GitHub artifact provenance attestations; archives also receive SHA-256 manifests and keyless Sigstore bundles. Images receive provenance, SPDX SBOM attestations, vulnerability scans, and keyless signatures.

The release workflow derives `SOURCE_DATE_EPOCH` from the tagged commit and packages staging directories with `xtask release-archive`. That command sorts paths, normalizes tar ownership and modes, fixes the tar and gzip timestamps and gzip operating-system field, and rejects absolute or archive-escaping symlinks. Executable files must be named explicitly, so their mode is independent of the build host:

```console
SOURCE_DATE_EPOCH="$(git show --no-patch --format=%ct v1.2.3)" \
  cargo run --locked --package xtask -- release-archive \
  staging/codex-start-1.2.3-x86_64-unknown-linux-gnu \
  codex-start-1.2.3-x86_64-unknown-linux-gnu.tar.gz \
  --prefix codex-start-1.2.3-x86_64-unknown-linux-gnu \
  --executable codex-start
```

## Maintainer checklist

1. Update dependencies and `assets/images.lock.toml` deliberately. Verify upstream OCI digests, npm integrity, and release checksums.
2. Run `cargo run --locked --package xtask -- validate`, the full test suite, rustfmt, Clippy pedantic, cargo-deny, and unused-dependency checks.
3. Build every environment and sidecar with Docker and rootless Podman on Linux amd64; inspect the resulting image, run the no-credential smoke tests, and verify that a launcher workload creates a host-owned file in a writable bind-mounted checkout.
4. Run the platform smoke matrix below and attach its results to the release issue.
5. Use `cargo release VERSION` to create the signed commit/tag, then push the tag. Watch the release and image workflows through signing and attestations.
6. Verify `SHA256SUMS`, one binary signature bundle, each OCI signature, each SPDX SBOM, and both the GitHub artifact and OCI provenance predicates before publishing release notes.

## Required platform smoke matrix

| Platform | Runtime | Architectures |
| --- | --- | --- |
| Linux | Docker 27+ | amd64, arm64 |
| Linux | rootless Podman 5.4+ | amd64, arm64 |
| macOS | Docker Desktop or OrbStack | Intel, Apple silicon |
| macOS | Podman Machine 5.4+ | Intel, Apple silicon |

For each available combination, exercise `doctor`; all four environment builds; interactive run/shell and signal propagation; UID/GID file writes; managed and direct host homes; worktree create/reuse/retain/cleanup; loopback ports; offline/allowlist/bridge/host networking; fake SSH/GPG agents; host SSH; OAuth callback; redacted secret injection; Ollama/LM Studio tunnels; and Codex `--version`, help, config/profile loading, stdio/HTTP MCP, skills, plugins, hooks, and exit-code passthrough.

No smoke test requires paid API traffic. Authentication-dependent tests use temporary fixtures and a local HTTP/MCP service.
