#!/bin/sh
# Install webhookr from the latest GitHub release (Linux x86_64).
# Installs the binary and, when systemd is present, configures a webhookr
# service so the daemon starts on boot and restarts on failure.
#
# The release workflow bakes the real repo into the __REPO__ placeholder; run
# locally from a checkout with: REPO=owner/repo ./install.sh
set -eu

REPO="${REPO:-__REPO__}"
INSTALL_DIR="${WEBHOOKR_INSTALL_DIR:-/usr/local/bin}"
BIN="$INSTALL_DIR/webhookr"
# Run the service as the invoking user so it reads the same config
# (~/.config/webhookr/config.json). SUDO_USER covers `curl ... | sudo sh`.
run_user="${SUDO_USER:-${USER:-$(id -un)}}"

case "$(uname -s)" in
  Linux) ;;
  *) echo "webhookr: unsupported OS: $(uname -s) (Linux only)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64) arch="x86_64" ;;
  *) echo "webhookr: unsupported architecture: $(uname -m) (x86_64 only)" >&2; exit 1 ;;
esac

asset="webhookr-linux-$arch"
base="https://github.com/$REPO/releases/latest/download"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "webhookr: downloading $asset ..."
curl -fsSL "$base/$asset" -o "$tmp/$asset"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"

# Verify the download against the published checksum.
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

# Set up (or refresh) the systemd service when systemd is available.
if command -v systemctl >/dev/null 2>&1; then
  echo "webhookr: configuring systemd service ..."
  sudo tee /etc/systemd/system/webhookr.service >/dev/null <<EOF
[Unit]
Description=webhookr webhook runner
After=network-online.target
Wants=network-online.target

[Service]
User=${run_user}
ExecStart=${BIN} serve
# always, not on-failure: a self-update exits deliberately so the new binary is
# the one that comes back, and on-failure would stop the service instead.
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
  sudo systemctl daemon-reload
  sudo systemctl enable webhookr
  sudo systemctl restart webhookr
  echo "webhookr: service running on http://127.0.0.1:9000"
else
  echo "webhookr: systemd not detected; run '${BIN} serve' under your own supervisor" >&2
fi

echo "webhookr: installed. $("$BIN" --version 2>/dev/null || echo ok)"
