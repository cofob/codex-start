#!/bin/sh
# Install codex-start from a signed GitHub Release.

set -eu

REPOSITORY=${CODEX_START_REPOSITORY:-cofob/codex-start}
GITHUB_API=${CODEX_START_GITHUB_API_URL:-https://api.github.com}
DOWNLOAD_BASE=${CODEX_START_RELEASE_DOWNLOAD_BASE:-https://github.com/$REPOSITORY/releases/download}
LATEST_RELEASE_URL=${CODEX_START_LATEST_RELEASE_URL:-$GITHUB_API/repos/$REPOSITORY/releases/latest}
MAX_METADATA_BYTES=10485760
MAX_ARTIFACT_BYTES=1073741824
MAX_EXECUTABLE_BLOCKS=1048576
VERSION=
INSTALL_DIR=${CODEX_START_INSTALL_DIR:-}
SYSTEM=0
ASSUME_YES=0
REQUIRE_SIGNATURE=0
FORCE=0
AUTO_UPDATES=preserve
AUTO_UPDATES_EXPLICIT=0
TEMP_DIR=
STAGED_DESTINATION=

say() {
    printf '%s\n' "$*"
}

warn() {
    printf 'codex-start installer: warning: %s\n' "$*" >&2
}

die() {
    printf 'codex-start installer: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Install the latest stable codex-start release.

Usage: install.sh [OPTIONS]

Options:
  --version VERSION         Install an explicit version (for example 1.2.3 or v1.2.3)
  --install-dir DIRECTORY   Override the portable installation directory
  --system                  Use the native Linux package manager or /usr/local/bin on macOS
  --auto-updates            Enable automatic update checks
  --no-auto-updates         Disable automatic update checks
  --yes                     Accept defaults without prompting
  --require-signature       Require Cosign and valid keyless Sigstore bundles
  --force                   Replace an existing non-executable regular destination
  -h, --help                Show this help

Environment seams for mirrors and tests:
  CODEX_START_GITHUB_API_URL, CODEX_START_LATEST_RELEASE_URL,
  CODEX_START_RELEASE_DOWNLOAD_BASE, CODEX_START_INSTALL_DIR
EOF
}

cleanup() {
    if [ -n "$STAGED_DESTINATION" ]; then
        rm -f -- "$STAGED_DESTINATION" 2>/dev/null || true
    fi
    if [ -n "$TEMP_DIR" ]; then
        rm -rf -- "$TEMP_DIR" 2>/dev/null || true
    fi
}
trap cleanup EXIT HUP INT TERM

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || die "--version requires a value"
            VERSION=$2
            shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || die "--install-dir requires a value"
            INSTALL_DIR=$2
            shift 2
            ;;
        --system)
            SYSTEM=1
            shift
            ;;
        --auto-updates)
            [ "$AUTO_UPDATES_EXPLICIT" -eq 0 ] || die "choose only one auto-update option"
            AUTO_UPDATES=true
            AUTO_UPDATES_EXPLICIT=1
            shift
            ;;
        --no-auto-updates)
            [ "$AUTO_UPDATES_EXPLICIT" -eq 0 ] || die "choose only one auto-update option"
            AUTO_UPDATES=false
            AUTO_UPDATES_EXPLICIT=1
            shift
            ;;
        --yes|-y)
            ASSUME_YES=1
            shift
            ;;
        --require-signature)
            REQUIRE_SIGNATURE=1
            shift
            ;;
        --force)
            FORCE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            [ "$#" -eq 0 ] || die "unexpected positional arguments: $*"
            ;;
        *)
            die "unknown option: $1 (use --help)"
            ;;
    esac
done

[ "$SYSTEM" -eq 0 ] || [ -z "$INSTALL_DIR" ] || die "--system and --install-dir cannot be combined"

need_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

file_size() {
    wc -c <"$1" | tr -d '[:space:]'
}

download_file() {
    download_url=$1
    download_output=$2
    download_limit=$3
    download_part=$download_output.part
    rm -f -- "$download_part"

    case "$download_url" in
        https://*|file://*) ;;
        *) die "refusing non-HTTPS download URL: $download_url" ;;
    esac

    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --silent --show-error \
            --proto '=https,file' --tlsv1.2 --connect-timeout 10 --max-time 300 \
            --max-filesize "$download_limit" \
            --header 'Accept: application/vnd.github+json' \
            --header 'X-GitHub-Api-Version: 2022-11-28' \
            --user-agent 'codex-start-installer' \
            --output "$download_part" "$download_url" || {
                rm -f -- "$download_part"
                die "download failed: $download_url"
            }
    elif command -v wget >/dev/null 2>&1; then
        wget -q -T 30 -t 2 -U 'codex-start-installer' \
            -O "$download_part" "$download_url" || {
                rm -f -- "$download_part"
                die "download failed: $download_url"
            }
    else
        die "curl or wget is required"
    fi

    download_size=$(file_size "$download_part")
    case "$download_size" in *[!0-9]*|'') die "could not determine download size" ;; esac
    [ "$download_size" -le "$download_limit" ] || {
        rm -f -- "$download_part"
        die "download exceeds the $download_limit byte limit: $download_url"
    }
    mv -f -- "$download_part" "$download_output"
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print tolower($1)}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print tolower($1)}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print tolower($NF)}'
    else
        die "sha256sum, shasum, or openssl is required"
    fi
}

checksum_for() {
    checksum_manifest=$1
    checksum_name=$2
    awk -v wanted="$checksum_name" '
        BEGIN { count = 0 }
        /^[0-9A-Fa-f][0-9A-Fa-f]*[[:space:]]/ {
            name = $2
            sub(/^\*/, "", name)
            if (name == wanted && length($1) == 64) {
                print tolower($1)
                count++
            }
        }
        END { if (count != 1) exit 1 }
    ' "$checksum_manifest"
}

verify_checksum() {
    verify_file=$1
    verify_name=$2
    verify_sums=$3
    verify_expected=$(checksum_for "$verify_sums" "$verify_name") || \
        die "SHA256SUMS must contain exactly one valid entry for $verify_name"
    verify_actual=$(sha256_file "$verify_file")
    [ "$verify_actual" = "$verify_expected" ] || die "SHA-256 mismatch for $verify_name"
    VERIFIED_SHA256=$verify_actual
}

json_string() {
    json_file=$1
    json_key=$2
    awk -v wanted="$json_key" '
        { document = document $0 "\n" }
        END {
            marker = "\"" wanted "\""
            start = index(document, marker)
            if (!start) exit 1
            rest = substr(document, start + length(marker))
            sub(/^[[:space:]]*:[[:space:]]*/, "", rest)
            if (substr(rest, 1, 1) != "\"") exit 1
            rest = substr(rest, 2)
            finish = index(rest, "\"")
            if (!finish) exit 1
            value = substr(rest, 1, finish - 1)
            if (value ~ /\\/) exit 1
            print value
        }
    ' "$json_file"
}

json_number() {
    json_file=$1
    json_key=$2
    awk -v wanted="$json_key" '
        { document = document $0 "\n" }
        END {
            marker = "\"" wanted "\""
            start = index(document, marker)
            if (!start) exit 1
            rest = substr(document, start + length(marker))
            sub(/^[[:space:]]*:[[:space:]]*/, "", rest)
            if (!match(rest, /^[0-9]+/)) exit 1
            print substr(rest, 1, RLENGTH)
        }
    ' "$json_file"
}

select_manifest_artifact() {
    select_manifest=$1
    select_kind=$2
    select_os=$3
    select_arch=$4
    select_libc=$5
    tr '}\n' '\n ' <"$select_manifest" | awk \
        -v wanted_kind="$select_kind" \
        -v wanted_os="$select_os" \
        -v wanted_arch="$select_arch" \
        -v wanted_libc="$select_libc" '
        function field(name,    marker, start, rest, finish, value) {
            marker = "\"" name "\""
            start = index(record, marker)
            if (!start) return "__missing__"
            rest = substr(record, start + length(marker))
            sub(/^[[:space:]]*:[[:space:]]*/, "", rest)
            if (substr(rest, 1, 1) == "\"") {
                rest = substr(rest, 2)
                finish = index(rest, "\"")
                if (!finish) return "__invalid__"
                value = substr(rest, 1, finish - 1)
                if (value ~ /\\/) return "__invalid__"
                return value
            }
            if (match(rest, /^(null|[0-9]+)/)) return substr(rest, 1, RLENGTH)
            return "__invalid__"
        }
        {
            record = $0
            while ((brace = index(record, "{")) != 0) record = substr(record, brace + 1)
            kind = field("kind")
            if (kind != wanted_kind || field("os") != wanted_os || field("arch") != wanted_arch) next
            if (field("libc") != wanted_libc) next
            filename = field("filename")
            size = field("size")
            sha = field("sha256")
            bundle = field("bundle")
            sbom = field("sbom")
            if (filename ~ /^[A-Za-z0-9][A-Za-z0-9._+-]*$/ &&
                size ~ /^[0-9]+$/ && size > 0 &&
                sha ~ /^[0-9A-Fa-f]+$/ && length(sha) == 64 &&
                bundle ~ /^[A-Za-z0-9][A-Za-z0-9._+-]*$/ &&
                sbom ~ /^[A-Za-z0-9][A-Za-z0-9._+-]*$/) {
                print filename "|" size "|" tolower(sha) "|" bundle "|" sbom
                matches++
            }
        }
        END { if (matches != 1) exit 1 }
    '
}

find_cosign() {
    case ${CODEX_START_COSIGN:-} in
        none) return 1 ;;
        '') command -v cosign 2>/dev/null || return 1 ;;
        *) [ -x "$CODEX_START_COSIGN" ] || die "CODEX_START_COSIGN is not executable"; printf '%s\n' "$CODEX_START_COSIGN" ;;
    esac
}

verify_sigstore() {
    sig_file=$1
    sig_bundle=$2
    sig_tag=$3
    sig_identity=${CODEX_START_CERTIFICATE_IDENTITY:-https://github.com/$REPOSITORY/.github/workflows/release.yml@refs/tags/$sig_tag}
    "$COSIGN" verify-blob \
        --bundle "$sig_bundle" \
        --certificate-identity "$sig_identity" \
        --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
        "$sig_file" >/dev/null || die "Sigstore verification failed for $(basename "$sig_file")"
}

run_privileged() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    elif command -v sudo >/dev/null 2>&1; then
        sudo -- "$@"
    else
        die "this operation requires root privileges; install sudo or run as root"
    fi
}

absolute_directory() {
    absolute_input=$1
    if [ -d "$absolute_input" ]; then
        (cd "$absolute_input" && pwd -P)
        return
    fi
    absolute_parent=$(dirname "$absolute_input")
    absolute_name=$(basename "$absolute_input")
    [ -d "$absolute_parent" ] || return 1
    absolute_resolved=$(cd "$absolute_parent" && pwd -P) || return 1
    printf '%s/%s\n' "$absolute_resolved" "$absolute_name"
}

json_escape() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

receipt_data_directory() {
    if [ -n "${CODEX_START_INSTALLER_TEST_DATA_DIR:-}" ]; then
        printf '%s\n' "$CODEX_START_INSTALLER_TEST_DATA_DIR"
    elif [ -n "${XDG_DATA_HOME:-}" ]; then
        printf '%s\n' "$XDG_DATA_HOME/codex-start"
    else
        printf '%s\n' "$HOME/.local/share/codex-start"
    fi
}

preserve_signature_policy() {
    planned_executable=$1
    policy_data_dir=$(receipt_data_directory)
    policy_receipt=$policy_data_dir/install.json
    [ -e "$policy_receipt" ] || return 0
    [ ! -L "$policy_receipt" ] && [ -f "$policy_receipt" ] || \
        die "existing installation receipt is not a regular file: $policy_receipt"
    planned_json=$(json_escape "$planned_executable")
    grep -Fq "\"executable\": \"$planned_json\"" "$policy_receipt" || return 0
    policy_count=$(grep -Ec '^[[:space:]]*"require_signature"[[:space:]]*:[[:space:]]*(true|false)[[:space:]]*,?[[:space:]]*$' "$policy_receipt" || true)
    [ "$policy_count" -eq 1 ] || die "existing installation receipt has an invalid signature policy"
    policy_value=$(grep -E '^[[:space:]]*"require_signature"[[:space:]]*:' "$policy_receipt" \
        | sed -E 's/.*:[[:space:]]*(true|false).*/\1/')
    if [ "$policy_value" = true ]; then
        REQUIRE_SIGNATURE=1
    fi
}

write_receipt() {
    receipt_method=$1
    receipt_target=$2
    receipt_executable=$3
    if printf '%s' "$receipt_executable" | LC_ALL=C grep -q '[[:cntrl:]]'; then
        die "installation path contains control characters and cannot be recorded safely"
    fi
    receipt_data_dir=$(receipt_data_directory)
    [ ! -L "$receipt_data_dir" ] || die "refusing symlinked application data directory: $receipt_data_dir"
    mkdir -p -- "$receipt_data_dir"
    chmod 700 "$receipt_data_dir" 2>/dev/null || true
    receipt_file=$receipt_data_dir/install.json
    [ ! -L "$receipt_file" ] || die "refusing symlinked installation receipt: $receipt_file"
    receipt_temp=$receipt_file.tmp.$$
    receipt_signature=false
    [ "$REQUIRE_SIGNATURE" -eq 0 ] || receipt_signature=true
    receipt_executable_json=$(json_escape "$receipt_executable")
    umask 077
    printf '{\n  "schema_version": 1,\n  "method": "%s",\n  "target": "%s",\n  "executable": "%s",\n  "require_signature": %s\n}\n' \
        "$receipt_method" "$receipt_target" "$receipt_executable_json" "$receipt_signature" >"$receipt_temp"
    chmod 600 "$receipt_temp" 2>/dev/null || true
    mv -f -- "$receipt_temp" "$receipt_file"
}

configure_auto_updates() {
    configure_executable=$1
    configure_value=$2
    [ "$configure_value" != preserve ] || return 0
    if [ -n "${CODEX_START_CONFIG_COMMAND:-}" ]; then
        configure_command=$CODEX_START_CONFIG_COMMAND
    else
        configure_command=$configure_executable
    fi
    "$configure_command" config set --global updates.enabled "$configure_value" || \
        die "codex-start was installed, but its auto-update preference could not be saved"
}

prompt_auto_updates() {
    if [ "$ASSUME_YES" -eq 1 ]; then
        AUTO_UPDATES=true
        return
    fi
    if [ -r /dev/tty ] && [ -w /dev/tty ]; then
        printf 'Enable automatic update checks? [Y/n] ' >/dev/tty
        IFS= read -r prompt_answer </dev/tty || prompt_answer=
        case "$prompt_answer" in
            ''|y|Y|yes|YES|Yes) AUTO_UPDATES=true ;;
            n|N|no|NO|No) AUTO_UPDATES=false ;;
            *) die "please answer yes or no" ;;
        esac
    else
        AUTO_UPDATES=true
    fi
}

os_name=$(uname -s 2>/dev/null || true)
case "$os_name" in
    Linux) PLATFORM_OS=linux ;;
    Darwin) PLATFORM_OS=macos ;;
    *) die "unsupported operating system: ${os_name:-unknown}; use install.ps1 on Windows" ;;
esac

machine=$(uname -m 2>/dev/null || true)
case "$machine" in
    x86_64|amd64) PLATFORM_ARCH=x86_64 ;;
    arm64|aarch64) PLATFORM_ARCH=aarch64 ;;
    *) die "unsupported architecture: ${machine:-unknown}; supported architectures are x86_64 and ARM64" ;;
esac

PACKAGE_KIND=
if [ "$PLATFORM_OS" = linux ]; then
    PLATFORM_LIBC=gnu
    if (ldd --version 2>&1 || true) | grep -qi musl || ls /lib/ld-musl-*.so.1 >/dev/null 2>&1; then
        PLATFORM_LIBC=musl
    fi
    if [ "$SYSTEM" -eq 1 ]; then
        if command -v apk >/dev/null 2>&1; then
            PACKAGE_KIND=apk
            PLATFORM_LIBC=musl
        elif command -v apt-get >/dev/null 2>&1; then
            PACKAGE_KIND=deb
            PLATFORM_LIBC=gnu
        elif command -v dnf >/dev/null 2>&1 || command -v yum >/dev/null 2>&1 || command -v rpm >/dev/null 2>&1; then
            PACKAGE_KIND=rpm
            PLATFORM_LIBC=gnu
        else
            die "--system requires apt, dnf/yum/rpm, or apk"
        fi
    fi
else
    PLATFORM_LIBC=null
fi

case "$PLATFORM_OS/$PLATFORM_ARCH/$PLATFORM_LIBC" in
    linux/x86_64/gnu) TARGET=x86_64-unknown-linux-gnu ;;
    linux/aarch64/gnu) TARGET=aarch64-unknown-linux-gnu ;;
    linux/x86_64/musl) TARGET=x86_64-unknown-linux-musl ;;
    linux/aarch64/musl) TARGET=aarch64-unknown-linux-musl ;;
    macos/x86_64/null) TARGET=x86_64-apple-darwin ;;
    macos/aarch64/null) TARGET=aarch64-apple-darwin ;;
    *) die "this OS, architecture, and libc combination is not supported" ;;
esac

if [ -n "$PACKAGE_KIND" ]; then
    PLANNED_DESTINATION=/usr/bin/codex-start
else
    if [ "$SYSTEM" -eq 1 ]; then
        planned_install_dir=/usr/local/bin
    elif [ -n "$INSTALL_DIR" ]; then
        planned_install_dir=$INSTALL_DIR
    else
        planned_install_dir=$HOME/.local/bin
    fi
    if [ -d "$planned_install_dir" ]; then
        planned_install_dir=$(cd "$planned_install_dir" && pwd -P)
        PLANNED_DESTINATION=$planned_install_dir/codex-start
    else
        PLANNED_DESTINATION=
    fi
fi
[ -z "$PLANNED_DESTINATION" ] || preserve_signature_policy "$PLANNED_DESTINATION"

TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/codex-start-install.XXXXXXXX") || die "could not create a temporary directory"

if [ -n "$VERSION" ]; then
    case "$VERSION" in v*) TAG=$VERSION; RELEASE_VERSION=${VERSION#v} ;; *) TAG=v$VERSION; RELEASE_VERSION=$VERSION ;; esac
else
    download_file "$LATEST_RELEASE_URL" "$TEMP_DIR/latest.json" "$MAX_METADATA_BYTES"
    TAG=$(json_string "$TEMP_DIR/latest.json" tag_name) || die "latest release metadata has no valid tag_name"
    RELEASE_VERSION=${TAG#v}
fi

printf '%s\n' "$TAG" | grep -Eq \
    '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?(\+[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$' || \
    die "release tag is not a v-prefixed semantic version: $TAG"
case "$TAG" in *[!A-Za-z0-9.v+-]*) die "release tag contains unsafe characters: $TAG" ;; esac
[ -n "$RELEASE_VERSION" ] || die "release version is empty"

RELEASE_URL=$DOWNLOAD_BASE/$TAG
download_file "$RELEASE_URL/SHA256SUMS" "$TEMP_DIR/SHA256SUMS" "$MAX_METADATA_BYTES"

if COSIGN=$(find_cosign); then
    download_file "$RELEASE_URL/SHA256SUMS.bundle" "$TEMP_DIR/SHA256SUMS.bundle" "$MAX_METADATA_BYTES"
    verify_sigstore "$TEMP_DIR/SHA256SUMS" "$TEMP_DIR/SHA256SUMS.bundle" "$TAG"
elif [ "$REQUIRE_SIGNATURE" -eq 1 ]; then
    die "--require-signature was used, but cosign is not installed"
else
    warn "cosign is unavailable; continuing with mandatory SHA-256 verification"
fi

download_file "$RELEASE_URL/release-manifest.json" "$TEMP_DIR/release-manifest.json" "$MAX_METADATA_BYTES"
verify_checksum "$TEMP_DIR/release-manifest.json" release-manifest.json "$TEMP_DIR/SHA256SUMS"

manifest_schema=$(json_number "$TEMP_DIR/release-manifest.json" schema_version) || die "invalid release manifest schema"
[ "$manifest_schema" = 1 ] || die "unsupported release manifest schema: $manifest_schema"
manifest_version=$(json_string "$TEMP_DIR/release-manifest.json" version) || die "release manifest has no valid version"
manifest_tag=$(json_string "$TEMP_DIR/release-manifest.json" tag) || die "release manifest has no valid tag"
[ "$manifest_version" = "$RELEASE_VERSION" ] || die "release manifest version does not match $TAG"
[ "$manifest_tag" = "$TAG" ] || die "release manifest tag does not match $TAG"

if [ -n "$PACKAGE_KIND" ]; then
    ARTIFACT_KIND=$PACKAGE_KIND
else
    ARTIFACT_KIND=archive
fi

selection=$(select_manifest_artifact "$TEMP_DIR/release-manifest.json" "$ARTIFACT_KIND" "$PLATFORM_OS" "$PLATFORM_ARCH" "$PLATFORM_LIBC") || \
    die "release manifest does not contain exactly one artifact for $ARTIFACT_KIND/$PLATFORM_OS/$PLATFORM_ARCH/$PLATFORM_LIBC"
old_ifs=$IFS
IFS='|'
set -- $selection
IFS=$old_ifs
[ "$#" -eq 5 ] || die "release manifest artifact entry is malformed"
ARTIFACT_NAME=$1
ARTIFACT_SIZE=$2
ARTIFACT_SHA256=$3
ARTIFACT_BUNDLE=$4
ARTIFACT_SBOM=$5
case "$ARTIFACT_SIZE" in *[!0-9]*|'') die "release manifest artifact size is invalid" ;; esac
[ "$ARTIFACT_SIZE" -le "$MAX_ARTIFACT_BYTES" ] || die "release artifact exceeds the $MAX_ARTIFACT_BYTES byte safety limit"

download_file "$RELEASE_URL/$ARTIFACT_NAME" "$TEMP_DIR/$ARTIFACT_NAME" "$ARTIFACT_SIZE"
[ "$(file_size "$TEMP_DIR/$ARTIFACT_NAME")" = "$ARTIFACT_SIZE" ] || die "downloaded size does not match release manifest for $ARTIFACT_NAME"
verify_checksum "$TEMP_DIR/$ARTIFACT_NAME" "$ARTIFACT_NAME" "$TEMP_DIR/SHA256SUMS"
[ "$VERIFIED_SHA256" = "$ARTIFACT_SHA256" ] || die "release manifest and SHA256SUMS disagree for $ARTIFACT_NAME"

if [ -n "${COSIGN:-}" ]; then
    download_file "$RELEASE_URL/$ARTIFACT_BUNDLE" "$TEMP_DIR/$ARTIFACT_BUNDLE" "$MAX_METADATA_BYTES"
    verify_sigstore "$TEMP_DIR/$ARTIFACT_NAME" "$TEMP_DIR/$ARTIFACT_BUNDLE" "$TAG"
fi

if [ -n "$PACKAGE_KIND" ]; then
    DESTINATION=/usr/bin/codex-start
    if [ -e "$DESTINATION" ] && [ -x "$DESTINATION" ]; then FRESH_INSTALL=0; else FRESH_INSTALL=1; fi
    case "$PACKAGE_KIND" in
        deb) run_privileged apt-get install -y "$TEMP_DIR/$ARTIFACT_NAME" ;;
        rpm)
            if command -v dnf >/dev/null 2>&1; then
                run_privileged dnf install -y "$TEMP_DIR/$ARTIFACT_NAME"
            elif command -v yum >/dev/null 2>&1; then
                run_privileged yum install -y "$TEMP_DIR/$ARTIFACT_NAME"
            else
                run_privileged rpm -Uvh "$TEMP_DIR/$ARTIFACT_NAME"
            fi
            ;;
        apk) run_privileged apk add --allow-untrusted "$TEMP_DIR/$ARTIFACT_NAME" ;;
    esac
    [ -x "$DESTINATION" ] || die "$PACKAGE_KIND reported success but $DESTINATION was not installed"
    RECEIPT_METHOD=$PACKAGE_KIND
else
    need_command tar
    archive_member=$(tar -tzf "$TEMP_DIR/$ARTIFACT_NAME" | awk '
        /^[A-Za-z0-9._+-]+\/codex-start$/ { print; count++ }
        END { if (count != 1) exit 1 }
    ') || die "archive must contain exactly one safe codex-start executable path"
    mkdir -p -- "$TEMP_DIR/extract"
    (
        ulimit -f "$MAX_EXECUTABLE_BLOCKS"
        tar -xzf "$TEMP_DIR/$ARTIFACT_NAME" -C "$TEMP_DIR/extract" "$archive_member"
    ) || die "archive extraction failed or exceeded the executable size limit"
    EXTRACTED=$TEMP_DIR/extract/$archive_member
    [ -f "$EXTRACTED" ] && [ ! -L "$EXTRACTED" ] || die "archive executable is not a regular file"
    chmod 755 "$EXTRACTED"

    if [ "$SYSTEM" -eq 1 ]; then
        INSTALL_DIR=/usr/local/bin
    elif [ -z "$INSTALL_DIR" ]; then
        INSTALL_DIR=$HOME/.local/bin
    fi

    if [ "$SYSTEM" -eq 1 ] && [ ! -d "$INSTALL_DIR" ]; then
        run_privileged mkdir -p -- "$INSTALL_DIR"
    else
        [ ! -L "$INSTALL_DIR" ] || die "refusing symlinked installation directory: $INSTALL_DIR"
        mkdir -p -- "$INSTALL_DIR"
    fi
    INSTALL_DIR=$(absolute_directory "$INSTALL_DIR") || die "could not resolve installation directory: $INSTALL_DIR"
    DESTINATION=$INSTALL_DIR/codex-start
    [ ! -L "$DESTINATION" ] || die "refusing to replace symlinked destination: $DESTINATION"
    if [ -e "$DESTINATION" ]; then
        [ -f "$DESTINATION" ] || die "destination is not a regular file: $DESTINATION"
        FRESH_INSTALL=0
        [ -x "$DESTINATION" ] || [ "$FORCE" -eq 1 ] || die "destination is not executable; use --force to replace it"
    else
        FRESH_INSTALL=1
    fi

    STAGED_DESTINATION=$DESTINATION.tmp.$$
    if [ -w "$INSTALL_DIR" ]; then
        cp -- "$EXTRACTED" "$STAGED_DESTINATION"
        chmod 755 "$STAGED_DESTINATION"
        mv -f -- "$STAGED_DESTINATION" "$DESTINATION"
    else
        run_privileged cp -- "$EXTRACTED" "$STAGED_DESTINATION"
        run_privileged chmod 755 "$STAGED_DESTINATION"
        run_privileged mv -f -- "$STAGED_DESTINATION" "$DESTINATION"
    fi
    STAGED_DESTINATION=
    RECEIPT_METHOD=portable
fi

if [ "$AUTO_UPDATES" = preserve ] && [ "$FRESH_INSTALL" -eq 1 ]; then
    prompt_auto_updates
fi

write_receipt "$RECEIPT_METHOD" "$TARGET" "$DESTINATION"
configure_auto_updates "$DESTINATION" "$AUTO_UPDATES"

say "Installed codex-start $RELEASE_VERSION to $DESTINATION"
if [ "$RECEIPT_METHOD" = portable ] && [ "$SYSTEM" -eq 0 ]; then
    case :${PATH:-}: in
        *:"$INSTALL_DIR":*) ;;
        *) warn "$INSTALL_DIR is not on PATH; add: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
    esac
fi
