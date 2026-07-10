# Installation and updates

Official releases provide portable archives for Linux, macOS, and Windows on x86-64 and ARM64. Linux releases also provide DEB, RPM, and APK packages. The installers select the exact artifact from the release's signed `release-manifest.json`; they do not guess a filename from untrusted API data.

## Installer scripts

On Linux or macOS, review and run the POSIX installer:

```console
curl --proto '=https' --tlsv1.2 -fsSLo install.sh \
  https://github.com/cofob/codex-start/releases/latest/download/install.sh
sh install.sh
```

It installs to `${CODEX_START_INSTALL_DIR:-$HOME/.local/bin}` by default. The installer prints the required `PATH` export when that directory is not already on `PATH`; it does not edit a shell profile. `--system` uses apt, dnf/yum/rpm, or apk on Linux and `/usr/local/bin` on macOS.

On Windows, run the PowerShell installer:

```powershell
$installer = Join-Path $env:TEMP install-codex-start.ps1
Invoke-WebRequest https://github.com/cofob/codex-start/releases/latest/download/install.ps1 -OutFile $installer
& $installer
```

The default destination is `%LOCALAPPDATA%\Programs\codex-start\bin`, which is added once to the user `PATH`. `-System` installs below `%ProgramFiles%`, adds that directory once to the machine `PATH`, and requires an elevated PowerShell session.

Both installers support the same policy controls:

| POSIX | PowerShell | Effect |
| --- | --- | --- |
| `--version 1.2.3` | `-Version 1.2.3` | Install an explicit stable or prerelease version instead of the latest stable release. |
| `--install-dir DIR` | `-InstallDir DIR` | Choose a portable destination. |
| `--system` | `-System` | Use the platform's system installation mode. |
| `--auto-updates` | `-AutoUpdates` | Explicitly enable automatic update checks. |
| `--no-auto-updates` | `-NoAutoUpdates` | Explicitly disable automatic update checks. |
| `--yes` | `-Yes` | Accept fresh-install defaults without prompting. |
| `--require-signature` | `-RequireSignature` | Require Cosign and successful Sigstore verification. |
| `--force` | `-Force` | Replace an unusual existing regular destination, while still rejecting links. |

A fresh interactive install asks whether to enable automatic update checks and defaults to Yes. A fresh non-interactive install also enables checks unless the disable option is supplied. An upgrade preserves the existing choice unless an explicit enable/disable option overrides it.

## Verification model

The installers always:

1. fetch `SHA256SUMS` and `release-manifest.json` from one release tag;
2. verify the manifest against its one exact `SHA256SUMS` entry;
3. select one exact OS, architecture, libc, and artifact-kind entry;
4. enforce the manifest's download size and verify the artifact against both recorded SHA-256 values; and
5. extract only the expected executable path or pass the verified local package to the system package manager.

If `cosign` is on `PATH`, the installers also verify `SHA256SUMS.bundle` and the selected artifact bundle against the tagged release workflow identity and GitHub Actions' OIDC issuer. Signature verification is mandatory with `--require-signature` or `-RequireSignature`; once enabled for an installation, ordinary upgrades preserve that strict policy. Without Cosign, the default mode warns and continues only after mandatory checksum verification.

To verify a downloaded release manually, first verify the checksum manifest's keyless signature:

```console
cosign verify-blob \
  --bundle SHA256SUMS.bundle \
  --certificate-identity "https://github.com/cofob/codex-start/.github/workflows/release.yml@refs/tags/v1.2.3" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS
sha256sum --check SHA256SUMS
```

On macOS, replace the last command with `shasum -a 256 --check SHA256SUMS`. Verify an individual artifact's adjacent `.bundle` with the same `cosign verify-blob` identity constraints.

Successful installs record their method, Rust target, executable path, and signature policy in `install.json` below the codex-start application data directory. The updater uses this receipt to choose portable replacement or the correct Linux package manager; it never overwrites a package-owned binary as though it were portable.

System updates retain the privilege boundary. Linux package updates and root-owned portable Unix installs invoke the package manager or atomic install through `sudo`. A Windows installation below `%ProgramFiles%` must be updated from an elevated terminal; writability is checked before release assets are downloaded. Windows executable replacement is staged until the running process exits, and automatic updates then restart the original command with its terminal attached.

## Self-update behavior

Check without changing the installation:

```console
codex-start update --check
```

Run an explicit update and confirm its prompt:

```console
codex-start update
```

For a non-interactive update, use `codex-start update --yes`. Add `--require-signature` to require Cosign even when the installation receipt did not. Explicit updates select the latest stable release, never downgrade, and remain available when automatic checks are disabled.

Eligible interactive commands check GitHub for a newer stable release at most once per configured interval. A prompt can install now, defer, skip that version, or disable future checks. Automatic network failures never fail the command the user originally requested.

Configure the policy globally:

```console
codex-start config set --global updates.enabled false
codex-start config set --global updates.check_interval_hours 24
codex-start config set --global updates.require_signature true
```

The equivalent environment override for disabling checks is `CODEX_START__UPDATES__ENABLED=false`. Update policy is host-global and is not accepted from project or environment manifests.

## Building from source

Building from source requires Rust 1.88 or newer:

```console
cargo install --locked --path crates/codex-start-host
```

Source installations do not create an installer receipt. An explicit update can replace a writable, unambiguous portable executable; otherwise it reports the appropriate manual installation command.
