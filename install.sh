#!/bin/sh
# Sotto installer: download the latest release for this machine, verify its checksum (and its
# Sigstore signature when `cosign` is installed), and install the `sotto` binary.
#
#   curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh | sh
#
# Options (environment variables):
#   SOTTO_INSTALL_DIR  install directory        (default: ~/.local/bin)
#   SOTTO_VERSION      tag to install, e.g. v0.1.0  (default: latest release)
#
# The script is POSIX sh, does nothing as root, and touches only the install directory.

set -eu

REPO="getsotto/sotto"
INSTALL_DIR="${SOTTO_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*" >&2; }
fail() {
    say "error: $*"
    exit 1
}

# --- pick the release target for this machine ---------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
Darwin/arm64) target="aarch64-apple-darwin" ;;
Darwin/x86_64) target="x86_64-apple-darwin" ;;
Linux/x86_64) target="x86_64-unknown-linux-gnu" ;;
Linux/aarch64) target="aarch64-unknown-linux-gnu" ;;
*) fail "no prebuilt binary for $os/$arch - build from source: cargo build --release -p sotto-cli" ;;
esac

# --- resolve the version -------------------------------------------------------------------------
version="${SOTTO_VERSION:-}"
if [ -z "$version" ]; then
    version="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
        grep '"tag_name"' | head -n1 | cut -d'"' -f4)"
    [ -n "$version" ] || fail "could not determine the latest release (set SOTTO_VERSION=vX.Y.Z)"
fi

asset="sotto-$version-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$version"
say "installing sotto $version ($target)"

# --- download + verify ---------------------------------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/$asset" "$base/$asset" || fail "download failed: $base/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || fail "download failed: $base/SHA256SUMS"

(
    cd "$tmp"
    grep " $asset\$" SHA256SUMS >asset.sum || fail "$asset is not listed in SHA256SUMS"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c asset.sum >/dev/null
    else
        shasum -a 256 -c asset.sum >/dev/null
    fi
) || fail "checksum verification FAILED - refusing to install"
say "checksum verified"

# Signature check: keyless Sigstore signatures bind the artefact to the release workflow's
# identity. Opportunistic - run when cosign is available; SECURITY.md has the manual steps.
if command -v cosign >/dev/null 2>&1; then
    if curl -fsSL -o "$tmp/$asset.sigstore.json" "$base/$asset.sigstore.json"; then
        cosign verify-blob \
            --bundle "$tmp/$asset.sigstore.json" \
            --certificate-identity-regexp "^https://github.com/$REPO/.github/workflows/release.yml@refs/tags/v" \
            --certificate-oidc-issuer https://token.actions.githubusercontent.com \
            "$tmp/$asset" >/dev/null 2>&1 || fail "Sigstore verification FAILED - refusing to install"
        say "signature verified (Sigstore)"
    else
        say "note: no signature bundle found for this release; checksum-only install"
    fi
else
    say "note: cosign not installed; skipping signature verification (see SECURITY.md)"
fi

# --- install -------------------------------------------------------------------------------------
tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/sotto-$version-$target/sotto" "$INSTALL_DIR/sotto"

say "installed $INSTALL_DIR/sotto"
case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*) say "note: $INSTALL_DIR is not on your PATH - add: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
say "shell completions: sotto completions bash|zsh|fish (also bundled in the release tarball)"
say "get started: sotto init"
