#!/bin/sh
# PocketVeto POSIX install script.
#
# Downloads the right release binary for the host OS/arch from GitHub Releases,
# puts it on PATH (/usr/local/bin with sudo, or ~/.local/bin as a fallback),
# and runs `pocket-veto init`.
#
# Usage:
#   curl -fsSL https://github.com/pocket-veto/pocket-veto/releases/latest/download/install.sh | sh
#
# Or to skip the interactive init:
#   curl -fsSL .../install.sh | sh -s -- --skip-bt

set -eu

REPO="pocket-veto/pocket-veto"
BIN_NAME="pocket-veto"

# Determine the download target.
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)
        # musl static binary runs on any glibc/musl Linux.
        TARGET="x86_64-unknown-linux-musl"
        ;;
    Darwin)
        TARGET="aarch64-apple-darwin"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        echo "install.sh: on Windows, use install.ps1 instead." >&2
        exit 1
        ;;
    *)
        echo "install.sh: unsupported OS '$OS'." >&2
        exit 1
        ;;
esac

case "$ARCH" in
    x86_64|amd64) : ;;  # only x86_64 Linux and aarch64 macOS are published
    arm64|aarch64)
        [ "$OS" = "Darwin" ] || { echo "install.sh: aarch64 Linux binary not published." >&2; exit 1; }
        ;;
    *)
        echo "install.sh: unsupported arch '$ARCH' for $OS." >&2
        exit 1
        ;;
esac

ASSET="${BIN_NAME}-${TARGET}"
URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT
DEST="$TMPDIR/$BIN_NAME"

echo "Downloading $URL"
if ! curl -fsSL "$URL" -o "$DEST"; then
    echo "install.sh: download failed for $URL" >&2
    exit 1
fi

chmod +x "$DEST"
if ! "$DEST" --version >/dev/null 2>&1; then
    echo "install.sh: downloaded binary is not executable / wrong arch." >&2
    exit 1
fi

# Install to /usr/local/bin if we can sudo, otherwise ~/.local/bin.
if [ -w /usr/local/bin ]; then
    INSTALL_DIR="/usr/local/bin"
    mv "$DEST" "$INSTALL_DIR/$BIN_NAME"
elif sudo -n true 2>/dev/null; then
    INSTALL_DIR="/usr/local/bin"
    sudo mv "$DEST" "$INSTALL_DIR/$BIN_NAME"
else
    INSTALL_DIR="$HOME/.local/bin"
    mkdir -p "$INSTALL_DIR"
    mv "$DEST" "$INSTALL_DIR/$BIN_NAME"
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) : ;;
        *) echo "install.sh: add $INSTALL_DIR to your PATH to use pocket-veto." ;;
    esac
fi

echo "Installed $BIN_NAME to $INSTALL_DIR"

# Run init with any args passed through (e.g. --skip-bt, --devcontainer).
exec "$INSTALL_DIR/$BIN_NAME" init "$@"
