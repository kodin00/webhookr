#!/bin/sh
# Install webhookr from the latest GitHub release.
#
# Detects the host OS and architecture, downloads the matching binary, verifies
# its checksum, and installs it to /usr/local/bin (or $WEBHOOKR_INSTALL_DIR).
#
# The release workflow bakes the real repo into the __REPO__ placeholder; run
# locally from a checkout with: REPO=owner/repo ./install.sh
set -eu

REPO="${REPO:-__REPO__}"
INSTALL_DIR="${WEBHOOKR_INSTALL_DIR:-/usr/local/bin}"
BIN="$INSTALL_DIR/webhookr"

# --- detect platform ---
case "$(uname -s)" in
  Linux)  os="linux" ;;
  Darwin) os="darwin" ;;
  *) echo "webhookr: unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64) arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *) echo "webhookr: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

asset="webhookr-$os-$arch"
base="https://github.com/$REPO/releases/latest/download"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "webhookr: downloading $asset ..."
curl -fsSL "$base/$asset" -o "$tmp/$asset"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"

# Verify the download against the published checksum. Portable across Linux and
# macOS: prefer sha256sum, fall back to shasum.
expected="$(awk -v a="$asset" '$2 == a { print $1; exit }' "$tmp/SHA256SUMS")"
if [ -n "$expected" ]; then
  actual=""
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
  fi
  if [ -z "$actual" ]; then
    echo "webhookr: warning: no sha256sum/shasum available; skipping checksum" >&2
  elif [ "$expected" != "$actual" ]; then
    echo "webhookr: checksum mismatch for $asset (corrupt download?)" >&2
    exit 1
  fi
else
  echo "webhookr: warning: no checksum entry for $asset; skipping verification" >&2
fi

echo "webhookr: installing to $BIN ..."
if [ -w "$INSTALL_DIR" ]; then
  install -m 0755 "$tmp/$asset" "$BIN"
else
  sudo install -m 0755 "$tmp/$asset" "$BIN"
fi

echo "webhookr: installed. $("$BIN" --version 2>/dev/null || echo ok)"
