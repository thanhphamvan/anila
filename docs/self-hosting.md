# Self-host anila

Get the source from [thanhphamvan/anila](https://github.com/thanhphamvan/anila).
Deploy into your own Cloudflare account and run the host service on a machine
you control. Authentication uses passkeys, with no external login provider.
This guide covers a new installation; see the existing-installation section
before reusing any Worker or authentication storage.

## 1. Prerequisites

You need Linux or macOS with tmux, stable Rust, Node.js/npm, Trunk, worker-build,
and Python 3.11+ for deployment configuration. For native herdr sessions
instead of or alongside tmux, follow [the herdr setup guide](herdr.md). Install Rust using
[rustup](https://rustup.rs/). On Ubuntu 24.04, install build tools with:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config tmux python3
```

On macOS install the Xcode command-line tools and tmux, for example using
Homebrew. Use a supported Node.js LTS version with npm; see Cloudflare's
[Wrangler requirements](https://developers.cloudflare.com/workers/wrangler/install-and-update/).
From a checkout of your copy of this repository:

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk --version 0.22.0 --locked
cargo install worker-build --version 0.8.7 --locked
cargo install --path service --locked
```

Ensure `~/.cargo/bin` is on PATH. The service is built from source here, so
no upstream binary release or private repository access is needed.

You also need a Cloudflare account with Workers enabled. The configuration
creates two SQLite-backed Durable Object namespaces and deploys static assets
with the Worker. There is no separate database to provision. SQLite-backed
Durable Objects are available on Free and Paid plans, with different quotas;
review current [Durable Object pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/)
and [limits](https://developers.cloudflare.com/durable-objects/platform/limits/)
before deploying. Availability of a free tier does not guarantee that this
build and your workload fit its script, CPU, storage or request limits.

Choose a stable HTTPS hostname before enrolling passkeys:

- Default: `https://anila.<your-workers-subdomain>.workers.dev`. Find or
  configure the account's Workers subdomain in the Cloudflare dashboard.
- Custom domain: e.g. `https://terminal.example.net`, on an active Cloudflare
  DNS zone in the same account. The deploy script adds the custom-domain
  route, disables `workers.dev` and disables preview URLs. Review Cloudflare's
  [custom-domain requirements](https://developers.cloudflare.com/workers/configuration/routing/custom-domains/).

Changing the hostname affects passkeys. `ORIGIN` must be the exact HTTPS
origin without a trailing slash or path; `RP_ID` must be its hostname.

## 2. Prepare the first host

For a personal Linux or macOS machine, run these commands as the Unix user
whose tmux sessions you want to expose:

```sh
mkdir -p ~/.config/anila
chmod 700 ~/.config/anila
cp service/service.example.toml ~/.config/anila/service.toml
chmod 600 ~/.config/anila/service.toml
```

Edit the config: set `host = "first-host"` and `worker` to your chosen HTTPS
origin. Keep only presets you use. `dir` must exist on the host. Then:

```sh
anila-service keygen
anila-service check-config
```

`keygen` creates `~/.config/anila/service.key` and prints a **public** key
and a `HOST_KEYS` map. Keep the private key on this host with mode 0600.
Never reuse another machine's signing key. `check-config` redacts Access
secrets. Do not dump configuration or keys into public logs.

### First host on a shared Linux server

Use a dedicated locked account on a server. Build the binary from the
checkout, then as an administrator:

```sh
cargo build --release --locked -p anila-service
sudo install -m 0755 target/release/anila-service /usr/local/bin/anila-service
sudo useradd --create-home --shell /bin/bash cloud-user
sudo passwd -l cloud-user
sudo -H -u cloud-user mkdir -p /home/cloud-user/.config/anila
sudo chmod 700 /home/cloud-user/.config/anila
sudo install -m 0600 -o cloud-user -g cloud-user service/service.example.toml /home/cloud-user/.config/anila/service.toml
sudoedit /home/cloud-user/.config/anila/service.toml
```

If `cloud-user` already exists, inspect it and skip creation. Keep it without
sudo or a usable password. Set `host`, `worker` and a single shell preset
with `dir = "~"`; sessions must start as `cloud-user`. Generate its own key:

```sh
sudo -H -u cloud-user /usr/local/bin/anila-service keygen
```

For the remaining host commands in this guide, use
`sudo -H -u cloud-user /usr/local/bin/anila-service` in place of
`anila-service`. People authenticate with `su - <their-user>` after opening
a terminal. This is useful only among trusted machine members: anyone who
can attach can also use a session already switched to another Unix user.

## 3. Configure and deploy the Cloudflare stack

Edit the public `[vars]` in `worker/wrangler.toml`:

```toml
RP_ID = "terminal.example.net"
ORIGIN = "https://terminal.example.net"
DEFAULT_HOST = "first-host"
HOST_KEYS = '{"first-host":"<PUBLIC key printed by keygen>"}'
VAPID_SUBJECT = "mailto:you@example.net"
```

For `workers.dev`, use your actual full Worker hostname in both origin and
RP ID. The default Worker name is `anila`; changing `name` changes its
`workers.dev` hostname. Use a new name for this installation. Keep the
Durable Object bindings and initial migration intact. Do not put private
keys, API tokens or Access secrets in Wrangler vars.

Authenticate from your deployment workstation and deploy:

```sh
(cd worker && npx -y wrangler@4.148.0 login)
sh scripts/deploy.sh
```

The script validates the origin, RP ID and bootstrap public-key map, builds
the frontend, and deploys the Worker and assets with pinned Wrangler. It uses
the same generated routing configuration as the Actions workflow. Choose
the intended Cloudflare account when logging in; for unattended use set
`CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` in the environment using
an account-scoped deployment token. Do not commit them. Custom-domain
creation also needs permission for the relevant zone.

A missing VAPID key only disables push. After deployment, a signed-out
`GET /api/me` should return 401 and the root page should serve the app:

```sh
curl -I https://terminal.example.net/
curl -s -o /dev/null -w '%{http_code}\n' https://terminal.example.net/api/me
```

If you enabled Cloudflare Access, a browser redirect to Access is expected
until sign-in. Configure machine paths as described below before starting
the service.

## 4. Start the host and enroll the administrator

In one host terminal:

```sh
anila-service run
```

Look for `connected to host room`. In another host terminal:

```sh
anila-service enroll
```

Open your deployment's HTTPS origin, choose **First time on this device?**,
and enter the one-time enrollment code, valid for 10 minutes. This creates
the first administrator account, binds the bootstrap machine to it, and
registers a passkey. Do not share or save the code in repository files.
Create a terminal and check input/output before configuring automatic start.

`HOST_KEYS` bootstraps this first machine only. Later machines use pairing;
do not copy the first host's private key or repeatedly edit `HOST_KEYS`.

## 5. Run automatically

### Linux server with systemd

For the dedicated `cloud-user` host above, create `/etc/systemd/system/anila.service`:

```ini
[Unit]
Description=anila host service
After=network-online.target
Wants=network-online.target

[Service]
User=cloud-user
Group=cloud-user
WorkingDirectory=/home/cloud-user
Environment=HOME=/home/cloud-user
Environment=LANG=C.UTF-8
Environment=RUST_LOG=info
ExecStart=/usr/local/bin/anila-service run
Restart=always
RestartSec=5
KillMode=process

[Install]
WantedBy=multi-user.target
```

Stop the manually started service, then:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now anila
sudo systemctl status anila
sudo journalctl -u anila -f
```

`KillMode=process` lets tmux sessions survive service restarts. A personal
Linux installation can instead use a systemd user unit for its own account;
keep the same process-survival behavior and use the actual binary path.

### macOS with launchd

Create `~/Library/LaunchAgents/me.anila.service.plist`, replacing the
executable path with the absolute result of `command -v anila-service`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>me.anila.service</string>
  <key>ProgramArguments</key><array>
    <string>/Users/YOUR_USER/.cargo/bin/anila-service</string><string>run</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>AbandonProcessGroup</key><true/>
  <key>StandardErrorPath</key><string>/tmp/anila-service.log</string>
</dict></plist>
```

Include the directory containing tmux in that PATH. Stop the manually
started service, then load the job with
`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/me.anila.service.plist`.
Keep `AbandonProcessGroup` so tmux survives service restarts.

## 6. Add machines, accounts and devices

In the web app choose **Add machine**, complete a fresh passkey ceremony,
and redeem its one-time code on the new machine:

```sh
anila-service pair <PAIRING-CODE> --worker https://terminal.example.net --name laptop
anila-service run
```

For an additional shared Linux server, the installer creates `cloud-user`,
pairs its own key and starts systemd:

```sh
sudo sh scripts/init-server.sh --worker https://terminal.example.net --code <PAIRING-CODE> --binary ./target/release/anila-service
```

This installer adds a machine to an **existing account**. It cannot create
the first account; use steps 2–4 to bootstrap that account. With a published
release, `ANILA_REPO=owner/repository` and `--version vX.Y.Z` can replace
`--binary`; public downloads need no GitHub token and checksums are verified.
Release binaries require a compatible glibc (at least 2.34); build from
source on other supported systems. Re-running the installer on an existing
machine without a code retains its pairing and configuration.

Administrators use **Invite someone** to create accounts. Owners use machine
settings to share machines; members gain access to all sessions on that
machine. Account → **Add another device** enrolls another passkey for the
same account. Removing a member cuts off their access.

## Optional Cloudflare Access and push

Access is an additional perimeter; anila still verifies passkeys and machine
signatures. Configure the browser hostname with an Access Allow policy and
the machine paths `/api/agent/*` and `/ws/agent` with a Service Auth policy
that includes your machine's service token. A machine cannot complete an
interactive Access login. Have the administrator enter the token directly
in the host's 0600 config, before any `[[preset]]` table:

```toml
access_client_id = "<Client ID>"
access_client_secret = "<Client Secret>"
```

`ANILA_ACCESS_CLIENT_ID` and `ANILA_ACCESS_CLIENT_SECRET` override the file.
Restart the service after changing it. Look for `Cloudflare Access service
token: set` and then `connected to host room`. Give the Actions smoke test
the corresponding repository secrets if using Access. Do not paste secrets
into bug reports.

For push notifications run `anila-service vapid-keygen` in a private terminal,
then set the private key using the interactive Wrangler secret prompt:

```sh
(cd worker && npx -y wrangler@4.148.0 secret put VAPID_PRIVATE_KEY)
```

Enable notifications in the app. On iPhone first install the app on the
Home Screen and open it there. Send a test with
`anila-service notify --title "Test" --body "Your host is ready"`.

## GitHub Actions and releases

Once your repository is on GitHub, the Deploy workflow accepts these settings:

| Setting | Kind | Purpose |
| --- | --- | --- |
| `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID` | Secret | Your Cloudflare account and deployment permissions |
| `ANILA_HOST_KEYS`, `ANILA_DEFAULT_HOST` | Variable | First host's public-key map and host ID |
| `ANILA_ORIGIN`, `ANILA_RP_ID` | Variable | Optional explicit origin and RP ID; otherwise resolves the default `anila` workers.dev URL |
| `ANILA_VAPID_SUBJECT` | Variable | Optional notification contact |
| `VAPID_PRIVATE_KEY` | Secret | Optional push key |
| `ANILA_ACCESS_CLIENT_ID`, `ANILA_ACCESS_CLIENT_SECRET` | Secret | Optional machine-path Access token for smoke tests |

Run Deploy manually or push a `deploy/**` branch. It runs CI before deploying
into your account. Review permissions and configure the `production`
environment as appropriate. Your Cloudflare secrets are not needed by fork
pull requests or local unit tests.

The Release workflow builds service binaries for Linux and macOS on arm64
and x86_64. A version tag must match Cargo.toml and publishes archives with
checksums and licenses. Do not tag until the publication checks pass. The
release verifier requires `ANILA_REPO=your-owner/your-repository` so downloads
always target the repository you chose. It uses the GitHub CLI; `--binary`
installation and source builds do not require it.

## Operations, recovery and existing installations

Keep an extra passkey on a separate device. Back up machine config/signing
keys privately and protect your Cloudflare account, deployment credentials
and authentication storage. Durable Object authentication data is separate
from local tmux processes. Restoring a Cloudflare deployment is not achieved
by backing up service.toml alone; establish and test your storage backup and
recovery procedure before depending on it. There is no email/password reset.
An owner who retains a registered host can use `anila-service enroll` for
its owner's account. `anila-service revoke-all` revokes the machine owner's
browser sessions and closes terminal sockets on that machine; it is not a substitute for rotating a compromised host key.

This edition is intended for a new deployment. Use a different Worker name
and fresh namespaces when moving from the private version. It rejects schema
3 (IdP-era) and unknown newer auth stores instead of downgrading their schema.
IdP identities and ownership are not migrated. Re-enroll passkeys and pair
each machine deliberately in the new installation. Existing tmux sessions
stay on their host; stop the old service before starting another service
against those same sessions. Never delete old storage as a deployment step.

After changes run the tests, deploy with the same Worker name and namespaces,
and verify sign-in, host online state and terminal input/output. Preserve
migration entries; changing the Worker name creates a different installation.

| Symptom | Check |
| --- | --- |
| Cloudflare Access redirect/rejection | Machine-path Service Auth policy, token expiry, token in the correct TOML section, service restart |
| `this machine is not paired` | Correct Worker origin and host ID; pair using a fresh code if the owner requests it |
| `bad signature` | This machine's paired key; do not copy another machine's key |
| Pairing code wrong/expired | Request a new one-time code from the owner; codes last 10 minutes |
| Passkey ceremony fails | HTTPS origin and RP ID, browser support, correct hostname and available passkey |
| Auth store request fails after moving old state | Unsupported IdP-era storage; use a fresh Worker/namespace |
| Service offline | Host logs, outbound connectivity, config, tmux and binary paths in systemd/launchd |

Verify the [real-phone checklist](phone-checklist.md) on a physical device.
The automated network harness does not replace testing a real radio.
