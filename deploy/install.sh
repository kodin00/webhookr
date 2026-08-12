#!/usr/bin/env bash
# Install webhookr as a systemd service.
# Assumes cargo (via mise) is on PATH.
set -euo pipefail

cd "$(dirname "$0")/.."

# 1. Build the release binary.
cargo build --release

# 2. Install the binary.
sudo install -m 0755 target/release/webhookr /usr/local/bin/webhookr

# 3. Install the systemd unit.
sudo cp deploy/webhookr.service /etc/systemd/system/webhookr.service

# 4. Reload systemd and start the daemon.
sudo systemctl daemon-reload && sudo systemctl enable --now webhookr

echo "webhookr is installed and running."
echo "Next steps:"
echo "  webhookr status   # confirm the daemon is up"
echo "  webhookr add      # register a project"
echo "  webhookr key --id <id>   # get the project's webhook secret"
