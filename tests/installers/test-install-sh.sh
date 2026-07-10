#!/bin/sh
# End-to-end fixture tests for install.sh. No network access is used.

set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/codex-start-installer-test.XXXXXXXX")
trap 'rm -rf -- "$TEST_ROOT"' EXIT HUP INT TERM

fail() {
    printf 'test-install-sh: %s\n' "$*" >&2
    exit 1
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    fi
}

case $(uname -s) in
    Linux)
        FIXTURE_OS=linux
        if (ldd --version 2>&1 || true) | grep -qi musl || ls /lib/ld-musl-*.so.1 >/dev/null 2>&1; then
            FIXTURE_LIBC=musl
        else
            FIXTURE_LIBC=gnu
        fi
        ;;
    Darwin)
        FIXTURE_OS=macos
        FIXTURE_LIBC=null
        ;;
    *)
        printf 'test-install-sh: skipped on unsupported host\n'
        exit 0
        ;;
esac

case $(uname -m) in
    x86_64|amd64) FIXTURE_ARCH=x86_64 ;;
    arm64|aarch64) FIXTURE_ARCH=aarch64 ;;
    *)
        printf 'test-install-sh: skipped on unsupported architecture\n'
        exit 0
        ;;
esac

VERSION=1.2.3
TAG=v$VERSION
RELEASE_DIR=$TEST_ROOT/releases/download/$TAG
STAGING=$TEST_ROOT/staging/codex-start-$VERSION-fixture
mkdir -p "$RELEASE_DIR" "$STAGING" "$TEST_ROOT/home" "$TEST_ROOT/data"

cat >"$STAGING/codex-start" <<'EOF'
#!/bin/sh
if [ "${1:-}" = config ] && [ "${2:-}" = set ]; then
    printf '%s\n' "${5:-missing}" >>"$CODEX_START_CONFIG_LOG"
    exit 0
fi
printf 'fixture codex-start 1.2.3\n'
EOF
chmod 755 "$STAGING/codex-start"
cp "$ROOT/README.md" "$ROOT/LICENSE" "$STAGING/"

ARTIFACT=codex-start-$VERSION-fixture.tar.gz
tar -czf "$RELEASE_DIR/$ARTIFACT" -C "$TEST_ROOT/staging" "$(basename "$STAGING")"
ARTIFACT_SIZE=$(wc -c <"$RELEASE_DIR/$ARTIFACT" | tr -d '[:space:]')
ARTIFACT_SHA=$(sha256_file "$RELEASE_DIR/$ARTIFACT")

cat >"$RELEASE_DIR/release-manifest.json" <<EOF
{
  "schema_version": 1,
  "version": "$VERSION",
  "tag": "$TAG",
  "artifacts": [
    {
      "kind": "archive",
      "os": "$FIXTURE_OS",
      "arch": "$FIXTURE_ARCH",
      "libc": $([ "$FIXTURE_LIBC" = null ] && printf null || printf '"%s"' "$FIXTURE_LIBC"),
      "filename": "$ARTIFACT",
      "size": $ARTIFACT_SIZE,
      "sha256": "$ARTIFACT_SHA",
      "bundle": "$ARTIFACT.bundle",
      "sbom": "$ARTIFACT.spdx.json"
    }
  ]
}
EOF
MANIFEST_SHA=$(sha256_file "$RELEASE_DIR/release-manifest.json")
cat >"$RELEASE_DIR/SHA256SUMS" <<EOF
$ARTIFACT_SHA  $ARTIFACT
$MANIFEST_SHA  release-manifest.json
EOF
printf 'fixture checksum bundle\n' >"$RELEASE_DIR/SHA256SUMS.bundle"
printf 'fixture artifact bundle\n' >"$RELEASE_DIR/$ARTIFACT.bundle"
printf '{"tag_name":"%s"}\n' "$TAG" >"$TEST_ROOT/latest.json"

INSTALL_DIR=$TEST_ROOT/bin-with-spaces/'codex start'
CONFIG_LOG=$TEST_ROOT/config.log
TEST_COSIGN=none
TEST_CONFIG_LOG=$CONFIG_LOG

run_installer() {
    run_install_dir=$1
    shift
    env \
        HOME="$TEST_ROOT/home" \
        XDG_DATA_HOME="$TEST_ROOT/data" \
        CODEX_START_INSTALL_DIR="$run_install_dir" \
        CODEX_START_LATEST_RELEASE_URL="file://$TEST_ROOT/latest.json" \
        CODEX_START_RELEASE_DOWNLOAD_BASE="file://$TEST_ROOT/releases/download" \
        CODEX_START_COSIGN="$TEST_COSIGN" \
        CODEX_START_CONFIG_LOG="$TEST_CONFIG_LOG" \
        sh "$ROOT/install.sh" "$@"
}

# Strict signature mode must fail closed when Cosign is unavailable.
if run_installer "$TEST_ROOT/strict-bin" --yes --require-signature >"$TEST_ROOT/strict.out" 2>"$TEST_ROOT/strict.err"; then
    fail '--require-signature succeeded without Cosign'
fi
[ ! -e "$TEST_ROOT/strict-bin/codex-start" ] || fail 'failed strict install wrote an executable'

# A present Cosign command must verify both signed blobs in strict mode.
FAKE_COSIGN=$TEST_ROOT/cosign
COSIGN_LOG=$TEST_ROOT/cosign.log
cat >"$FAKE_COSIGN" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >>"$COSIGN_LOG"
exit 0
EOF
chmod 755 "$FAKE_COSIGN"
TEST_COSIGN=$FAKE_COSIGN TEST_CONFIG_LOG=$TEST_ROOT/signed-config.log \
    run_installer "$TEST_ROOT/signed-bin" --yes --require-signature >"$TEST_ROOT/signed.out" 2>"$TEST_ROOT/signed.err"
[ -x "$TEST_ROOT/signed-bin/codex-start" ] || fail 'strict signed install did not write an executable'
[ "$(wc -l <"$COSIGN_LOG" | tr -d '[:space:]')" = 2 ] || fail 'Cosign did not verify both checksum and artifact bundles'
grep -q -- '--certificate-oidc-issuer https://token.actions.githubusercontent.com' "$COSIGN_LOG" || fail 'Cosign issuer constraint was not supplied'
if TEST_COSIGN=none TEST_CONFIG_LOG=$TEST_ROOT/signed-config.log \
    run_installer "$TEST_ROOT/signed-bin" --yes >"$TEST_ROOT/strict-upgrade.out" 2>"$TEST_ROOT/strict-upgrade.err"; then
    fail 'ordinary upgrade silently disabled the existing strict signature policy'
fi
TEST_COSIGN=none
TEST_CONFIG_LOG=$CONFIG_LOG

run_installer "$INSTALL_DIR" --yes >"$TEST_ROOT/first.out" 2>"$TEST_ROOT/first.err"
[ -x "$INSTALL_DIR/codex-start" ] || fail 'portable executable was not installed'
[ "$("$INSTALL_DIR/codex-start")" = 'fixture codex-start 1.2.3' ] || fail 'installed executable is not the fixture'
[ "$(cat "$CONFIG_LOG")" = true ] || fail 'fresh non-interactive install did not enable update checks'
RECEIPT=$TEST_ROOT/data/codex-start/install.json
[ -f "$RECEIPT" ] || fail 'installation receipt was not written'
grep -q '"method": "portable"' "$RECEIPT" || fail 'receipt method is incorrect'
CANONICAL_INSTALL_DIR=$(cd "$INSTALL_DIR" && pwd -P)
grep -Fq "\"executable\": \"$CANONICAL_INSTALL_DIR/codex-start\"" "$RECEIPT" || fail 'receipt executable is incorrect'

# An upgrade without an explicit option must preserve the existing preference.
run_installer "$INSTALL_DIR" --yes >"$TEST_ROOT/upgrade.out" 2>"$TEST_ROOT/upgrade.err"
[ "$(wc -l <"$CONFIG_LOG" | tr -d '[:space:]')" = 1 ] || fail 'upgrade rewrote the update preference'

# An explicit preference must override the preserved value.
run_installer "$INSTALL_DIR" --yes --no-auto-updates >"$TEST_ROOT/disable.out" 2>"$TEST_ROOT/disable.err"
[ "$(tail -n 1 "$CONFIG_LOG")" = false ] || fail '--no-auto-updates was not persisted'

# A corrupted artifact must fail before replacing the installed executable.
BEFORE_SHA=$(sha256_file "$INSTALL_DIR/codex-start")
printf 'tampered\n' >>"$RELEASE_DIR/$ARTIFACT"
if run_installer "$INSTALL_DIR" --yes >"$TEST_ROOT/tamper.out" 2>"$TEST_ROOT/tamper.err"; then
    fail 'tampered artifact was accepted'
fi
[ "$(sha256_file "$INSTALL_DIR/codex-start")" = "$BEFORE_SHA" ] || fail 'failed install changed the existing executable'

printf 'test-install-sh: all tests passed\n'
