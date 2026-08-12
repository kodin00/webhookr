#!/usr/bin/env bash
# Install webhookr from the latest GitHub release.
# The release workflow bakes the real repo into the __REPO__ placeholder;
# run locally from a checkout with: REPO=owner/repo ./install.sh
set -euo pipefail

REPO="${REPO:-__REPO__}"
INSTALL_DIR="${WEBHOOKR_INSTALL_DIR:-/usr/local/bin}"
BIN="$INSTALL_DIR/webhookr"

case "$(uname -s)" in
  Linux)
    case "$(uname -m)" in
      x86_64|amd64)  arch="linux-x86_64" ;;
      aarch64|arm64) arch="linux-aarch64" ;;
      *) echo "webhookr: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "webhookr: unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

base="https://github.com/$REPO/releases/latest/download"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "webhookr: downloading webhookr-$arch ..."
curl -fsSL "$base/webhookr-$arch" -o "$tmp/webhookr"

curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"
(cd "$tmp" && sha256sum -c SHA256SUMS --ignore-missing)

echo "webhookr: installing to $BIN ..."
if [ -w "$INSTALL_DIR" ]; then
  install -m 0755 "$tmp/webhookr" "$BIN"
else
  sudo install -m 0755 "$tmp/webhookr" "$BIN"
fi

echo "webhookr: installed. $("$BIN" --version 2>/dev/null || echo ok)"
