# webhookr

A self-hosted webhook runner. Point a GitHub (or generic) webhook at it, and
when a webhook fires, webhookr does a `git pull` on the configured branch and
runs your deploy command — logging everything so you can tail it later.

Projects can also be cloned automatically from a repository URL and deployed
with built-in Docker Compose presets. An optional Cloudflare Tunnel can publish
the webhook listener at a real HTTPS hostname without opening port 9000.

## Install

Every merge to `master` builds release binaries and publishes them to the
`latest` GitHub release. Install with one line:

```sh
curl -fsSL https://github.com/kodin00/webhookr/releases/latest/download/install.sh | sh
```

The installer picks the right Linux binary for your architecture (x86_64 /
aarch64), verifies its checksum, installs it to `/usr/local/bin/webhookr`
(override with `WEBHOOKR_INSTALL_DIR`), and sets up a `webhookr` systemd
service that starts on boot and restarts on failure.

### Updating

Re-run the same command — it overwrites the installed binary with the latest
release, refreshes the systemd service, and restarts it. Your config
(`~/.config/webhookr/`) and run history are left untouched.

To build from source instead, see [Build](#build).

## How it works

- Each project maps a URL slug (`/hooks/<id>`) to a Git checkout plus a
  deployment preset or custom shell command.
- Incoming webhooks are authenticated against a per-project secret, then
  `git fetch` / `checkout` / `pull` runs before the command.
- Projects and the listen address live in one JSON config file, shared between
  the daemon and the management CLI/TUI.
- Run history and per-run logs are written to a separate state directory.

## Prerequisites

- Rust (installed via [mise](https://mise.jdx.dev/)) — `.mise.toml` pins the
  toolchain.
- Git, plus Docker with the Compose plugin for Compose presets.
- An installed `cloudflared` binary or Docker when using Cloudflare Tunnel.

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
  --repository https://github.com/me/my-site.git \
  --branch main \
  --preset compose_build \
  --compose-file compose.production.yaml

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
| `webhookr list` | Table of configured projects, deployment presets, and last-run state. |
| `webhookr add [...]` | Add a project from a local checkout or repository URL. |
| `webhookr edit --id <ID> [...]` | Update source, deployment preset, Compose file/profiles, or webhook settings. |
| `webhookr remove --id <ID> [--yes]` | Remove a project (prompts for confirmation unless `--yes`). |
| `webhookr key --id <ID> [--rotate]` | Show the project's secret (and webhook URL); `--rotate` generates a new one. |
| `webhookr logs --id <ID> [--lines <N>]` | Tail the latest run's log (default 50 lines). |
| `webhookr run --id <ID>` | Re-run deployment without touching the Git checkout. |
| `webhookr update --id <ID>` | Clone or fast-forward the source, then deploy it. |
| `webhookr cloudflare --hostname hooks.example.com` | Provision a Cloudflare Tunnel; reads `CLOUDFLARE_API_TOKEN` or `--api-token`. |

## Deployment presets

The add/edit wizard and CLI support four deployment modes:

| Preset | Behavior |
| --- | --- |
| `compose_build` | `docker compose -f <file> up -d --build --remove-orphans` |
| `compose_pull` | Pull images, then run Compose detached with orphan cleanup. |
| `compose_up` | Run Compose detached without forcing a pull or rebuild. |
| `custom` | Run the configured shell command from the project directory. |

Use `--compose-profile <name>` more than once to enable Compose profiles.
Compose files must be relative to the checkout and cannot use `..` to escape it.

If `--repository` is set and the project path does not exist, the first update
clones that branch into the path. Later updates fetch, check out, and fast-forward
the same branch. Existing configs continue to use the `custom` preset.

## TUI key bindings

| Key | Action |
| --- | --- |
| `j` / `↓` | Select next project |
| `k` / `↑` | Select previous project |
| `a` | Add a project |
| `e` | Edit the selected project |
| `d` | Delete the selected project |
| `r` | Run the selected project |
| `u` | Open Update app: clone/pull and deploy the selected project |
| `l` | View the selected project's log |
| `q` | Quit |
| `c` | Configure Cloudflare Tunnel |

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
(`webhookr add --verify-mode token`, or `webhookr edit --verify-mode token`).
Then send the secret in a header:

```
X-Webhookr-Key: <secret>
```

Any `POST` to the hook URL carrying the correct `X-Webhookr-Key` header
triggers the deploy.

## Cloudflare Tunnel

Choose `Cloudflare tunnel` on the main screen, or run:

```sh
export CLOUDFLARE_API_TOKEN='scoped-token'
webhookr cloudflare --hostname hooks.example.com
sudo systemctl restart webhookr
```

The token needs `Zone Read`, `DNS Write`, and `Cloudflare Tunnel Write` for the
target account/zone. webhookr uses it once to create or update a remotely
managed tunnel, its ingress rule, and a proxied CNAME. The API token is not
stored. Only the narrower runtime tunnel token is saved to
`~/.config/webhookr/cloudflare-credentials.json` with owner-only permissions.

At runtime webhookr starts an installed `cloudflared` binary, or falls back to
the official `cloudflare/cloudflared` Docker image with host networking. Restart
the daemon after changing tunnel configuration. The public webhook URL becomes
`https://hooks.example.com/hooks/<id>`.

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
