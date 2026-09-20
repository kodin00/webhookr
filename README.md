# webhookr

A self-hosted webhook runner. Point a GitHub (or generic) webhook at it, and
when a webhook fires, webhookr does a `git pull` on the configured branch and
runs your deploy command — logging everything so you can tail it later.

Projects can be cloned automatically from a repository URL, deployed with
built-in Docker Compose presets, and published on a real HTTPS hostname via an
optional Cloudflare Tunnel. Manage it from a terminal UI, a CLI, or an optional
browser dashboard.

## Install

```sh
curl -fsSL https://github.com/kodin00/webhookr/releases/latest/download/install.sh | sh
```

Installs the Linux x86_64 binary to `/usr/local/bin/webhookr` (override with
`WEBHOOKR_INSTALL_DIR`), verifies its checksum, and sets up a `webhookr`
systemd service that starts on boot and restarts on failure.

### Updating

From the app: **Settings → Version → Update and restart** in the web UI, or
`Update webhookr` (`v`) in the TUI. From a shell:

```sh
webhookr version       # what is running, and what is published
webhookr self-update   # replace this binary with the published build
```

Re-running the install one-liner also works — it overwrites the binary,
rewrites the systemd unit, and restarts the service. Run it from the same
account as before: the unit's `User=` is set to whoever invoked the script,
and a different `$HOME` means a different `config.json` (every project appears
to vanish). `releases/latest/download` serves whatever `latest` points at
right now, so if you push and install within a couple of minutes you can
silently reinstall the old build — `webhookr version` is the check.

Self-update needs write access to the installed binary. The daemon does not
run as root, so a root-owned binary must be updated via the install one-liner
(which uses `sudo`). Updating from the web UI or `serve` exits the process on
purpose so the service manager starts the new binary — under systemd it comes
back; in a terminal it does not.

```sh
$ webhookr version
running:   0.2.1 (73deef9)
published: 0.2.1 (73deef9) (up to date)
```

`(local build)` instead of a commit means the binary was built from a checkout
rather than installed from a release.

## How it works

- Each project maps a URL slug (`/hooks/<id>`) to a Git checkout plus a
  deployment preset or custom shell command.
- Incoming webhooks are authenticated against a per-project secret, then
  `git fetch` / `checkout` / `pull` runs before the command.
- Projects and the listen address live in one JSON config file, shared between
  the daemon and the management CLI/TUI.
- Run history and per-run logs are written to a separate state directory. Logs
  stream to disk as commands produce them, so a long `docker compose build` can
  be followed while it runs.
- Each run records what triggered it — `push by <user>`, `merge by <user>`,
  `workflow_dispatch by <user>`, `webhook`, `web UI`, or `cli` — shown on run
  pages, dashboard cards, and in Telegram messages.
- Only one run per project at a time; a trigger arriving while that project is
  already deploying is refused rather than queued, so two pushes cannot race
  `git pull` on the same checkout.

## Prerequisites

- Rust (via [mise](https://mise.jdx.dev/)) — `.mise.toml` pins the toolchain.
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

# 3. Start the daemon (systemd recommended for production)
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
| `webhookr serve [-p, --port <PORT>] [--web] [--no-web] [--web-port <PORT>]` | Run the daemon in the foreground; `--port` overrides the configured port. `--web` starts the admin UI for this run only. |
| `webhookr web enable [--addr <ADDR>] [--hostname <HOST>]` | Turn the browser admin UI on (persisted). |
| `webhookr web disable` / `webhookr web status` | Turn it off / show its status. |
| `webhookr status` | Show the listen address, whether the daemon is up, and every project with its webhook URL and last-run status. |
| `webhookr list` | Table of configured projects, deployment presets, and last-run state. |
| `webhookr add [...]` | Add a project from a local checkout or repository URL. |
| `webhookr edit --id <ID> [...]` | Update source, deployment preset, Compose file/profiles, or webhook settings. |
| `webhookr remove --id <ID> [--yes]` | Remove a project (prompts unless `--yes`). |
| `webhookr key --id <ID> [--rotate]` | Show the project's secret (and webhook URL); `--rotate` generates a new one. |
| `webhookr logs --id <ID> [--lines <N>]` | Tail the latest run's log (default 50 lines). |
| `webhookr run --id <ID> [--no-pull]` | Pull the latest source, then run the deployment; `--no-pull` deploys the checkout as-is. |
| `webhookr update --id <ID>` | Clone or fast-forward the source, then deploy it. |
| `webhookr cloudflare [--hostname hooks.example.com] [--admin-hostname deploy.example.com]` | Provision a Cloudflare Tunnel; at least one hostname required. Reads `CLOUDFLARE_API_TOKEN` or `--api-token`. |
| `webhookr version [--offline]` | Show the running build and compare it with the published one. |
| `webhookr self-update [--check]` | Replace this binary with the published build; `--check` only reports. |

## Deployment presets

| Preset | Behavior |
| --- | --- |
| `compose_build` | `docker compose -f <file> up -d --build --remove-orphans` |
| `compose_pull` | Pull images, then run Compose detached with orphan cleanup. |
| `compose_up` | Run Compose detached without forcing a pull or rebuild. |
| `custom` | Run the configured shell command from the project directory. |

Use `--compose-profile <name>` more than once to enable Compose profiles.
Compose files must be relative to the checkout and cannot use `..` to escape it.

### Private repositories

Set an **access token** on the project (web UI, `Source` section) to clone and
pull private repos over HTTPS — a GitHub personal access token with read access
to the repository. It is handed to git through a credential helper that reads
an environment variable, so it never appears in the process list and is never
written into the checkout's `.git/config`.

If `--repository` is set and the project path does not exist, the first update
clones that branch into the path. Later updates fetch, check out, and
fast-forward the same branch.

## Web admin UI

An optional browser dashboard that does everything the TUI does — add, edit and
remove projects, reveal and rotate secrets, trigger deploys, watch run logs
stream live, and configure the Cloudflare Tunnel — with web forms instead of a
terminal wizard.

> **It has no login.** Anyone who can reach it can set a project's deploy
> command and run it as the daemon's user, and can replace the webhookr binary.
> Put Cloudflare Access (or an equivalent) in front of it before exposing it,
> and set `User=` in the systemd unit so that user is not root.

Off by default. Turn it on with:

```sh
webhookr web enable --addr 127.0.0.1:9001
sudo systemctl restart webhookr
```

The UI runs inside the same `webhookr serve` process as the webhook listener.
Its port serves **both** surfaces: the dashboard at `/`, and webhooks at
`/webhook/<id>` (with `/hooks/<id>` kept as an alias) — so one hostname is
enough for everything.

### Reaching it

The default bind is `127.0.0.1`, which still works through a Cloudflare Tunnel:
`cloudflared` runs on the same host and connects to `http://127.0.0.1:<port>`.
Binding to `0.0.0.0` would additionally expose the panel on the LAN and the
server's public IP, where Access cannot protect it.

To publish it on one hostname, provision the tunnel with only an admin hostname
— the admin port already serves webhooks too:

```sh
export CLOUDFLARE_API_TOKEN='scoped-token'
webhookr cloudflare --admin-hostname deploy.example.com
sudo systemctl restart webhookr
```

That gives you `https://deploy.example.com` for the dashboard and
`https://deploy.example.com/webhook/<id>` for GitHub.

> **Access needs a path exception.** GitHub cannot complete an Access login, so
> a policy covering the whole hostname will silently break every delivery. In
> the Access application for `deploy.example.com`, add a **Bypass** policy for
> the path `/webhook/*` (and `/hooks/*` if you use the alias) ahead of your
> normal policy. Those paths stay protected by the per-project HMAC/token check.

To keep them apart, pass both hostnames and Access only the admin host:

```sh
webhookr cloudflare --hostname hooks.example.com --admin-hostname deploy.example.com
```

For a quick look without persisting anything or exposing it at all:

```sh
webhookr serve --web --web-port 9001    # then browse http://127.0.0.1:9001
```

### Pages

| Page | What it does |
| --- | --- |
| `/` | Project cards with live status badges, and one-click deploy buttons. |
| `/projects/new`, `/projects/{id}/edit` | The whole project form on one page, with a server-side directory picker for the checkout path. |
| `/projects/{id}` | Config, webhook URL, reveal-on-click secret, recent runs. |
| `/runs`, `/runs/{id}` | Run history and a log view that streams while a deploy is running, then stops polling by itself. |
| `/settings`, `/settings/cloudflare` | Listen addresses, the running build with an update button, and tunnel provisioning. |
| `/webhook/{id}` | The webhook receiver, on this same port. Not behind the CSRF or Access-header checks — it authenticates with the project secret instead. |

Requests that change anything are rejected unless the browser reports them as
same-origin, so a page on another site cannot drive the panel using your Access
session. Set `require_access_header` in the config to additionally reject any
request that did not arrive through Cloudflare Access.

There is no build step and no CDN: the UI is server-rendered HTML, and htmx plus
the stylesheet are compiled into the binary.

## TUI key bindings

| Key | Action |
| --- | --- |
| `j` / `↓` | Select next project |
| `k` / `↑` | Select previous project |
| `a` | Add a project |
| `e` | Edit the selected project |
| `d` | Delete the selected project |
| `r` | Run the selected project |
| `u` | Clone/pull and deploy the selected project |
| `l` | View the selected project's log |
| `c` | Configure Cloudflare Tunnel |
| `w` | Toggle the web admin UI on or off |
| `v` | Update webhookr to the published build |
| `q` | Quit |

## Configuring a GitHub webhook

In your repo: **Settings → Webhooks → Add webhook**:

- **Payload URL**: `http://<vps-ip>:9000/hooks/<id>`
- **Content type**: `application/json`
- **Secret**: the project's secret (`webhookr key --id <id>`)

webhookr verifies the `X-Hub-Signature-256` header against the project's secret
before running anything.

### Deployment triggers

By default a project deploys on **every** signed delivery GitHub sends. Narrow
it per project by picking which events trigger a deploy:

- **Push** — a `push` to the configured branch, including the merge commit
  pushed after a PR merge.
- **Merge** — a `pull_request` event that was closed and merged. Needs the
  **`pull_request`** event enabled in your GitHub webhook settings (the default
  `push`-only webhook will not fire it).

Leave both unticked to keep the default (every delivery). A `ping` delivery
never triggers a deploy. If both are selected, a PR merge fires two deliveries;
the second is refused by webhookr's one-run-at-a-time lock, so only one deploy
starts.

Set triggers in any of the three surfaces — web admin UI (**Deployment
triggers** on the project form), CLI (`webhookr add --trigger-event push
--trigger-event merge`, or omit on `edit` to leave unchanged), or the TUI
wizard's **Triggers** step.

## GitHub commit status

webhookr can report each deploy back to GitHub as a **commit status**, so the
commit you pushed shows the pending dot, green check or red X on the repository
page, the commits list and any pull request that contains it. **Details**
opens that run's log. Off by default; turn it on per project in the web admin
UI under `GitHub commit status`:

| Setting | What it does |
| --- | --- |
| Report deploy results | The master switch for this project. |
| Status token | A token that can **write commit statuses**. Leave blank to reuse the project's access token. |
| Context | The label GitHub shows beside the status. Defaults to `webhookr/<id>`. |

The token needs `repo:status` on a classic personal access token, or
**Commit statuses: write** on a fine-grained one scoped to the single
repository. `repo:status` is deliberately narrow — it grants no access to the
repository's code. A fine-grained token scoped only to read contents (the right
scope for cloning) **cannot** write statuses, which is why this is a separate
field; when one token covers both, leave it blank and the access token is used.
The default context is per project, so two webhookr projects deploying from
one repository do not overwrite each other's indicator.

What gets posted: `pending` when the deploy starts; `success` when it finishes
(e.g. `deployed in 47s`); `failure` when it exits non-zero (described with the
last line of its output); `error` when the source could not be fetched at all.
The status lands on the commit named in the push payload when the push was to
the project's branch, and on `git rev-parse HEAD` otherwise — including manual
deploys, so a redeploy after a fix clears a stale red X. If the pull moves HEAD
past the announced commit, both get the final status so nothing is left
`pending` forever. A push that arrives while a project is already deploying is
refused and gets a terminal `error`; the in-flight run's own `git pull` picks
that commit up anyway and supersedes the `error`.

Posting a status can never fail or delay a deploy — anything that goes wrong is
written into the run log as a `# github status:` line and otherwise ignored.

The **Details** link points at `/runs/<run-id>` on the admin UI, using the
tunnel's admin hostname. With only a loopback address the link is omitted
rather than pointing at `127.0.0.1`; the webhook hostname is never used for it.
Following the link hits Cloudflare Access normally and needs no bypass policy.

### GitHub Enterprise

Derived from the repository URL, with nothing to configure: a `github.com`
remote uses `api.github.com`, and any other host is treated as GitHub
Enterprise Server at `https://<host>/api/v3`.

## Telegram notifications

webhookr can post each deploy to a **Telegram** chat through a bot you own:
`🚀` when a run starts, `✅` when it succeeds, `❌` when it fails — quoting the
tail of the run log on a failure. Off by default, and global: one bot and one
chat for every project, configured under **Settings** in the web admin UI.

| Setting | What it does |
| --- | --- |
| Notify a Telegram chat about deploys | The master switch. |
| Bot token | A token from [@BotFather](https://t.me/BotFather). Stored like the other secrets: the form never echoes it back, blank keeps the stored one. |
| Chat id | The chat to post to. Group chats have negative ids; `@channelname` also works. |

To find a group's chat id: create the bot with @BotFather, **add it to the
group**, send any message in the group, then open
`https://api.telegram.org/bot<token>/getUpdates` and read `message.chat.id` —
it will start with `-100`.

The `started` message carries the project, run, commit, and trigger
(`push by alice`, `merge by alice`, `web UI`, …); the finished messages carry
the duration and result (or the error and tail of the log on failure), with the
trigger beside the commit. When the admin UI has a public hostname, each
message ends with a link to that run's log page; with only a loopback address
the link is omitted. Posting a message can never fail or delay a deploy —
anything that goes wrong is written into the run log as a `# telegram:` line
and otherwise ignored. The final message is retried once; the `started`
message is not, since the final one supersedes it minutes later anyway.

The bot token lives in `config.json` alongside the other secrets, in a file
webhookr keeps owner-only.

## `token` verify mode

For non-GitHub senders, set the project's `verify_mode` to `token`
(`webhookr add --verify-mode token`, or `webhookr edit --verify-mode token`)
and send the secret in a header:

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
stored; only the narrower runtime tunnel token is saved to
`~/.config/webhookr/cloudflare-credentials.json` with owner-only permissions.

At runtime webhookr starts an installed `cloudflared` binary, or falls back to
the official `cloudflare/cloudflared` Docker image with host networking.
Restart the daemon after changing tunnel configuration. The public webhook URL
becomes `https://hooks.example.com/hooks/<id>`.

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
