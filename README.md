# webhookr

A self-hosted webhook runner. Point a GitHub (or generic) webhook at it, and
when a webhook fires, webhookr does a `git pull` on the configured branch and
runs your deploy command — logging everything so you can tail it later.

## Install

Every merge to `master` builds a release binary and publishes it to the
`latest` GitHub release. Install it in one line (Linux x86_64 / aarch64):

```sh
curl -fsSL https://github.com/kodin00/webhookr/releases/latest/download/webhookr-linux-$(uname -m) -o webhookr && chmod +x webhookr && sudo mv webhookr /usr/local/bin/webhookr
```

Or use the installer script, which normalizes the architecture and verifies the
checksum before installing:

```sh
curl -fsSL https://github.com/kodin00/webhookr/releases/latest/download/install.sh | bash
```

The binary lands in `/usr/local/bin/webhookr` (override with
`WEBHOOKR_INSTALL_DIR`). To build from source instead, see [Build](#build).

## How it works

- Each project maps a URL slug (`/hooks/<id>`) to a local git checkout plus a
  shell command.
- Incoming webhooks are authenticated against a per-project secret, then
  `git fetch` / `checkout` / `pull` runs before the command.
- Projects and the listen address live in one JSON config file, shared between
  the daemon and the management CLI/TUI.
- Run history and per-run logs are written to a separate state directory.

## Prerequisites

- Rust (installed via [mise](https://mise.jdx.dev/)) — `.mise.toml` pins the
  toolchain.
- A writable git checkout for each project you want to deploy.

## Build

```sh
mise exec -- cargo build --release
```

The binary is `target/release/webhookr`.

## Quick start

```sh
# 1. Register a project (prompts for anything you don't pass)
webhookr add \
  --name "My Site" \
  --path /srv/my-site \
  --branch main \
  --command "./deploy.sh"

# 2. Get the secret + webhook URL (printed on add, or via:)
webhookr key --id my-site

# 3. Start the daemon in the foreground (systemd recommended for production)
webhookr serve
# ...or launch the interactive TUI
webhookr
```

`webhookr add` prints the generated secret once; use `webhookr key --id <id>`
to show it again, and `--rotate` to roll it.

## CLI reference

Run `webhookr --help` for the same list.

| Command | Description |
| --- | --- |
| `webhookr` | Launch the interactive TUI (blocks until you quit). |
| `webhookr serve [-p, --port <PORT>]` | Run the daemon in the foreground; `--port` overrides the configured port. |
| `webhookr status` | Show the listen address, whether the daemon is up, and every project with its webhook URL and last-run status. |
| `webhookr list` | Table of configured projects (id, name, branch, command, verify_mode, last run). |
| `webhookr add [--name --id --path --branch --command --verify_mode]` | Add a project, prompting for missing fields. Defaults: `branch=main`, `verify_mode=github`. |
| `webhookr edit --id <ID> [--name --path --branch --command --verify_mode]` | Update fields of an existing project. |
| `webhookr remove --id <ID> [--yes]` | Remove a project (prompts for confirmation unless `--yes`). |
| `webhookr key --id <ID> [--rotate]` | Show the project's secret (and webhook URL); `--rotate` generates a new one. |
| `webhookr logs --id <ID> [--lines <N>]` | Tail the latest run's log (default 50 lines). |
| `webhookr run --id <ID>` | Manually trigger the project's pull + command. |

## TUI key bindings

| Key | Action |
| --- | --- |
| `j` / `↓` | Select next project |
| `k` / `↑` | Select previous project |
| `a` | Add a project |
| `e` | Edit the selected project |
| `d` | Delete the selected project |
| `r` | Run the selected project |
| `l` | View the selected project's log |
| `q` | Quit |

## Configuring a GitHub webhook

In your repo: **Settings → Webhooks → Add webhook**:

- **Payload URL**: `http://<vps-ip>:9000/hooks/<id>`
- **Content type**: `application/json`
- **Secret**: the project's secret (`webhookr key --id <id>`)

webhookr verifies the `X-Hub-Signature-256` header against the project's
secret before running anything, so only requests signed with the secret are
accepted.

## `token` verify mode

For non-GitHub senders, set the project's `verify_mode` to `token`
(`webhookr add --verify_mode token`, or `webhookr edit --verify_mode token`).
Then send the secret in a header:

```
X-Webhookr-Key: <secret>
```

Any `POST` to the hook URL carrying the correct `X-Webhookr-Key` header
triggers the deploy.

## Files

- **Config**: `~/.config/webhookr/config.json` on Linux (override with
  `WEBHOOKR_CONFIG` or `WEBHOOKR_CONFIG_DIR`).
- **Logs / run history**: `~/.local/share/webhookr/` on Linux (override with
  `WEBHOOKR_STATE_DIR`). Per-run logs live under
  `~/.local/share/webhookr/logs/runs/<run-id>.log`, indexed by
  `~/.local/share/webhookr/runs.json`.

## Deploying with systemd

```sh
./deploy/install.sh
```

The script builds the release binary, installs it to `/usr/local/bin/webhookr`,
installs the `webhookr.service` unit, and enables/starts it. The daemon runs in
the foreground under systemd with `Restart=always`. Before starting, edit
`deploy/webhookr.service` to uncomment and set `User=`/`Group=` to the account
that owns the project checkouts (they must be writable by that user).
