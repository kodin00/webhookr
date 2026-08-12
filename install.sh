#!/bin/sh
# Install webhookr from the latest GitHub release (Linux x86_64 / aarch64).
#
# The release workflow bakes the real repo into the __REPO__ placeholder; run
# locally from a checkout with: REPO=owner/repo ./install.sh
set -eu

REPO="${REPO:-__REPO__}"
INSTALL_DIR="${WEBHOOKR_INSTALL_DIR:-/usr/local/bin}"
BIN="$INSTALL_DIR/webhookr"

case "$(uname -s)" in
  Linux) ;;
  *) echo "webhookr: unsupported OS: $(uname -s) (Linux only)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64) arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *) echo "webhookr: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

asset="webhookr-linux-$arch"
base="https://github.com/$REPO/releases/latest/download"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "webhookr: downloading $asset ..."
curl -fsSL "$base/$asset" -o "$tmp/$asset"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"

# Verify the download against the published checksum (sha256sum ships with
# coreutils on every Linux server).
expected="$(awk -v a="$asset" '$2 == a { print $1; exit }' "$tmp/SHA256SUMS")"
actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
if [ -n "$expected" ] && [ "$expected" != "$actual" ]; then
  echo "webhookr: checksum mismatch for $asset (corrupt download?)" >&2
  exit 1
fi

echo "webhookr: installing to $BIN ..."
if [ -w "$INSTALL_DIR" ]; then
  install -m 0755 "$tmp/$asset" "$BIN"
else
  sudo install -m 0755 "$tmp/$asset" "$BIN"
fi

echo "webhookr: installed. $("$BIN" --version 2>/dev/null || echo ok)"
