#!/bin/sh
# Stripe CLI installer for the credentialed smoke workflow: download one pinned archive, verify
# its digest, and place the executable in the runner's temporary tool directory.
#
# Keep this separate from the workflow so the download and checksum gate have one auditable path.
# It is POSIX sh because the surrounding repository installers deliberately stay portable.

set -eu

: "${STRIPE_CLI_BIN_DIR:?STRIPE_CLI_BIN_DIR must name the install directory}"

say() { printf '%s\n' "$*" >&2; }
fail() {
  say "error: $*"
  exit 1
}

# Pin the CLI release and archive digest so the credentialed smoke job never executes a floating
# download. Update both values together when the Stripe CLI is upgraded.
version=1.45.0
archive="stripe_${version}_linux_x86_64.tar.gz"
url="https://github.com/stripe/stripe-cli/releases/download/v${version}/${archive}"
sha256=c3145c65dc6a0e1951bdf918b44dfa19555d0f6b616a6b142862e66e1520def9
temp_dir=$(mktemp -d)

cleanup() {
  rm -rf "$temp_dir"
}
trap cleanup EXIT INT TERM

mkdir -p "$STRIPE_CLI_BIN_DIR"
curl -fsSL "$url" -o "$temp_dir/$archive" || fail "download failed: $url"
printf '%s  %s\n' "$sha256" "$temp_dir/$archive" |
  sha256sum -c - >/dev/null || fail "checksum verification FAILED - refusing to install"
tar -xzf "$temp_dir/$archive" -C "$temp_dir" || fail "could not extract Stripe CLI archive"
test -x "$temp_dir/stripe" || fail "Stripe CLI archive did not contain an executable"
install -m 0755 "$temp_dir/stripe" "$STRIPE_CLI_BIN_DIR/stripe" ||
  fail "could not install Stripe CLI into $STRIPE_CLI_BIN_DIR"
