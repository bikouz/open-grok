#!/usr/bin/env bash

set -euo pipefail

readonly REPOSITORY="mweinbach/open-grok"
readonly ARTIFACT_NAME="open-grok-macos-aarch64"

usage() {
    cat >&2 <<'EOF'
Usage: install.sh [VERSION]

Install the latest Open Grok release, or VERSION when supplied. VERSION may
optionally start with "v".

Environment:
  OPENGROK_HOME               Runtime home (default: $HOME/.opengrok)
  OPEN_GROK_BIN_DIR           Installation directory override
  OPEN_GROK_RELEASE_BASE_URL  Direct URL containing the release assets
EOF
}

if [[ $# -gt 1 ]]; then
    usage
    exit 2
fi

requested_version="${1:-}"
version="${requested_version#v}"
if [[ -n "$requested_version" ]] &&
    [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$ ]]; then
    echo "Error: invalid version '$requested_version'." >&2
    exit 2
fi

os="$(uname -s)"
arch="$(uname -m)"
if [[ "$os" != "Darwin" ]] || [[ "$arch" != "arm64" && "$arch" != "aarch64" ]]; then
    echo "Error: prebuilt Open Grok releases currently require Apple Silicon macOS." >&2
    echo "Detected: ${os} ${arch}. Build from source on unsupported platforms." >&2
    exit 1
fi

if command -v curl >/dev/null 2>&1; then
    downloader="curl"
elif command -v wget >/dev/null 2>&1; then
    downloader="wget"
else
    echo "Error: curl or wget is required." >&2
    exit 1
fi

download() {
    local url="$1"
    local output="$2"
    if [[ "$downloader" == "curl" ]]; then
        curl -fsSL --retry 3 --retry-delay 1 -o "$output" "$url"
    else
        wget -q -O "$output" "$url"
    fi
}

if [[ -n "${OPEN_GROK_RELEASE_BASE_URL:-}" ]]; then
    release_url="${OPEN_GROK_RELEASE_BASE_URL%/}"
elif [[ -n "$version" ]]; then
    release_url="https://github.com/${REPOSITORY}/releases/download/v${version}"
else
    release_url="https://github.com/${REPOSITORY}/releases/latest/download"
fi

if [[ -n "${OPEN_GROK_BIN_DIR:-}" ]]; then
    bin_dir="$OPEN_GROK_BIN_DIR"
else
    if [[ -z "${OPENGROK_HOME:-}" && -z "${HOME:-}" ]]; then
        echo "Error: HOME or OPENGROK_HOME must be set." >&2
        exit 1
    fi
    open_grok_home="${OPENGROK_HOME:-$HOME/.opengrok}"
    bin_dir="${open_grok_home}/bin"
fi

case "$bin_dir" in
    /*) ;;
    *)
        echo "Error: the Open Grok bin directory must be absolute: $bin_dir" >&2
        exit 1
        ;;
esac

mkdir -p "$bin_dir"
stage_dir="$(mktemp -d "${bin_dir}/.open-grok-install.XXXXXX")"
cleanup() {
    rm -rf "$stage_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

binary_tmp="${stage_dir}/${ARTIFACT_NAME}"
checksum_tmp="${stage_dir}/${ARTIFACT_NAME}.sha256"

echo "Downloading Open Grok ${version:-latest} for Apple Silicon macOS..." >&2
download "${release_url}/${ARTIFACT_NAME}" "$binary_tmp"
download "${release_url}/${ARTIFACT_NAME}.sha256" "$checksum_tmp"

expected_sha="$(awk 'NR == 1 { print $1 }' "$checksum_tmp" | tr '[:upper:]' '[:lower:]')"
if [[ ${#expected_sha} -ne 64 || "$expected_sha" == *[!0-9a-f]* ]]; then
    echo "Error: release checksum is not a valid SHA-256 digest." >&2
    exit 1
fi

if command -v shasum >/dev/null 2>&1; then
    actual_sha="$(shasum -a 256 "$binary_tmp" | awk '{ print $1 }')"
elif command -v sha256sum >/dev/null 2>&1; then
    actual_sha="$(sha256sum "$binary_tmp" | awk '{ print $1 }')"
else
    echo "Error: shasum or sha256sum is required." >&2
    exit 1
fi

if [[ "$actual_sha" != "$expected_sha" ]]; then
    echo "Error: SHA-256 verification failed; Open Grok was not installed." >&2
    exit 1
fi

chmod 0755 "$binary_tmp"
mv -f "$binary_tmp" "${bin_dir}/open-grok"

echo "Installed Open Grok at ${bin_dir}/open-grok" >&2
case ":${PATH:-}:" in
    *":${bin_dir}:"*) ;;
    *)
        printf 'Add Open Grok to PATH:\n  export PATH="%s:$PATH"\n' "$bin_dir" >&2
        ;;
esac
echo "Run 'open-grok' to get started." >&2
