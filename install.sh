#!/bin/sh
# Catenary installer — downloads the latest release binary for your platform.
set -e

REPO="TwoWells/Catenary"
INSTALL_DIR="${CATENARY_INSTALL_DIR:-/usr/local/bin}"

detect_platform() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)  os_name="linux" ;;
        Darwin) os_name="macos" ;;
        *)      echo "Unsupported OS: $os" >&2; exit 1 ;;
    esac

    case "$arch" in
        x86_64|amd64)  arch_name="amd64" ;;
        aarch64|arm64) arch_name="arm64" ;;
        *)             echo "Unsupported architecture: $arch" >&2; exit 1 ;;
    esac

    # No Intel-mac asset is published (arm-only ruling: macOS 27 is
    # Apple-silicon-exclusive; the shipped surface matches the proven one).
    if [ "$os_name" = "macos" ] && [ "$arch_name" = "amd64" ]; then
        echo "Unsupported platform: Intel Macs. Catenary ships Apple-silicon" >&2
        echo "macOS binaries only; build from source if you need Intel." >&2
        exit 1
    fi

    echo "catenary-${os_name}-${arch_name}"
}

main() {
    asset="$(detect_platform)"
    echo "Detected platform: ${asset}"

    # Get latest release tag
    tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' | head -1 | cut -d'"' -f4)"

    if [ -z "$tag" ]; then
        echo "Failed to determine latest release." >&2
        exit 1
    fi

    echo "Latest release: ${tag}"

    url="https://github.com/${REPO}/releases/download/${tag}/${asset}"
    echo "Downloading ${url}..."

    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    curl -fsSL -o "${tmpdir}/catenary" "$url"
    chmod +x "${tmpdir}/catenary"

    if [ -w "$INSTALL_DIR" ]; then
        mv "${tmpdir}/catenary" "${INSTALL_DIR}/catenary"
    else
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "${tmpdir}/catenary" "${INSTALL_DIR}/catenary"
    fi

    echo "Installed catenary to ${INSTALL_DIR}/catenary"
    "${INSTALL_DIR}/catenary" --version
}

main
