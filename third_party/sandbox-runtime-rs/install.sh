#!/bin/sh
# install.sh
set -e

# --- CONFIGURATION ---
REPO="wangyedev/sandbox-runtime-rs"
BINARY_NAME="srt"
INSTALL_DIR="/usr/local/bin"
# ---------------------

# 1. Detect OS & Arch
OS="$(uname -s)"
ARCH="$(uname -m)"
PLATFORM=""

case "$OS" in
    Linux)  PLATFORM="linux" ;;
    Darwin) PLATFORM="macos" ;;
    *) echo "Error: Unsupported OS '$OS'"; exit 1 ;;
esac

# Normalize Arch to match our Release Asset naming convention
case "$ARCH" in
    x86_64)       ARCH="x86_64" ;;
    arm64|aarch64) ARCH="aarch64" ;;
    *) echo "Error: Unsupported Architecture '$ARCH'"; exit 1 ;;
esac

echo "Detected: $PLATFORM ($ARCH)"

# 2. Find Latest Version
echo "Fetching latest version tag..."
LATEST_URL="https://api.github.com/repos/$REPO/releases/latest"
TAG_NAME=$(curl -s "$LATEST_URL" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')

if [ -z "$TAG_NAME" ]; then
    echo "Error: Could not find a release for $REPO."
    echo "Check if the repository is public and has at least one release."
    exit 1
fi

echo "Latest version: $TAG_NAME"

# 3. Construct Download URL
# Matches the naming convention in the GitHub Action: srt-{suffix}.tar.gz
ASSET_NAME="srt-${PLATFORM}-${ARCH}.tar.gz"
DOWNLOAD_URL="https://github.com/$REPO/releases/download/$TAG_NAME/$ASSET_NAME"

# 4. Download and Install
TEMP_DIR=$(mktemp -d)
echo "Downloading $DOWNLOAD_URL..."

if ! curl -fsSL "$DOWNLOAD_URL" -o "$TEMP_DIR/$ASSET_NAME"; then
    echo "Error: Failed to download release asset."
    exit 1
fi

echo "Extracting..."
tar -xzf "$TEMP_DIR/$ASSET_NAME" -C "$TEMP_DIR"

# Determine install location
if [ -w "$INSTALL_DIR" ]; then
    # Can write directly to /usr/local/bin
    :
elif command -v sudo >/dev/null 2>&1; then
    # Use sudo for /usr/local/bin
    :
else
    # Fall back to ~/.local/bin
    INSTALL_DIR="$HOME/.local/bin"
    mkdir -p "$INSTALL_DIR"
fi

echo "Installing '$BINARY_NAME' to $INSTALL_DIR..."

if [ -w "$INSTALL_DIR" ]; then
    mv "$TEMP_DIR/$BINARY_NAME" "$INSTALL_DIR/"
else
    sudo mv "$TEMP_DIR/$BINARY_NAME" "$INSTALL_DIR/"
fi

# 5. Cleanup
rm -rf "$TEMP_DIR"

echo "------------------------------------------------"
echo "Success! '$BINARY_NAME' is installed to $INSTALL_DIR"

# Remind user to add ~/.local/bin to PATH if needed
if [ "$INSTALL_DIR" = "$HOME/.local/bin" ]; then
    case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *) echo "Note: Add ~/.local/bin to your PATH to run '$BINARY_NAME'" ;;
    esac
fi

echo "Run '$BINARY_NAME --help' to get started."
echo "------------------------------------------------"