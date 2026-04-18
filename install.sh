#!/bin/sh
set -eu

REPO="${FERRUS_INSTALL_REPO:-RomanEmreis/ferrus}"
VERSION="${FERRUS_INSTALL_VERSION:-latest}"
TARGET=""
ARCHIVE=""

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: required command not found: $1" >&2
        exit 1
    fi
}

detect_platform() {
    os="$(uname -s)"
    arch="$(uname -m)"

    if [ "$os" != "Linux" ]; then
        echo "error: this installer currently supports Linux only (detected ${os})" >&2
        exit 1
    fi

    case "$arch" in
        x86_64|amd64)
            TARGET="x86_64-unknown-linux-gnu"
            ;;
        aarch64|arm64)
            TARGET="aarch64-unknown-linux-gnu"
            ;;
        *)
            echo "error: this installer currently supports x86_64 and aarch64 Linux only (detected ${arch})" >&2
            exit 1
            ;;
    esac

    ARCHIVE="ferrus-${TARGET}.tar.gz"
}

resolve_url() {
    case "$VERSION" in
        latest)
            RELEASE_URL="https://github.com/${REPO}/releases/latest/download/${ARCHIVE}"
            ;;
        *)
            RELEASE_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"
            ;;
    esac
}

pick_install_dir() {
    if [ -n "${FERRUS_INSTALL_DIR:-}" ]; then
        INSTALL_DIR="$FERRUS_INSTALL_DIR"
        return
    fi

    if [ -n "${XDG_BIN_HOME:-}" ]; then
        INSTALL_DIR="$XDG_BIN_HOME"
        return
    fi

    INSTALL_DIR="${HOME}/.local/bin"
}

download_archive() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$RELEASE_URL" -o "$TMP_DIR/$ARCHIVE"
        return
    fi

    if command -v wget >/dev/null 2>&1; then
        wget -qO "$TMP_DIR/$ARCHIVE" "$RELEASE_URL"
        return
    fi

    echo "error: either curl or wget is required" >&2
    exit 1
}

install_binary() {
    mkdir -p "$INSTALL_DIR"
    tar -xzf "$TMP_DIR/$ARCHIVE" -C "$TMP_DIR"
    install "$TMP_DIR/ferrus-${TARGET}/ferrus" "$INSTALL_DIR/ferrus"
}

print_success() {
    echo "installed ferrus to ${INSTALL_DIR}/ferrus"

    case ":$PATH:" in
        *:"$INSTALL_DIR":*)
            ;;
        *)
            echo "warning: ${INSTALL_DIR} is not on PATH" >&2
            echo "add this to your shell profile:" >&2
            echo "  export PATH=\"${INSTALL_DIR}:\$PATH\"" >&2
            ;;
    esac
}

main() {
    need_cmd uname
    need_cmd tar
    need_cmd mktemp
    need_cmd install

    detect_platform
    resolve_url
    pick_install_dir

    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

    download_archive
    install_binary
    print_success
}

main "$@"
