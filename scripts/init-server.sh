#!/bin/sh
# Sets up a Linux server as an anila machine: creates the service user,
# installs tmux and anila-service, pairs the machine with your account, and
# runs the service under systemd. Terminal sessions open as that user;
# people then switch to their own account with `su - <user>`.
#
# In the web app choose "Add machine" to get a pairing code, then on the
# server, as root:
#
#   sh init-server.sh --worker https://term.example.com --code XXXX-XXXX --binary ./anila-service
#   ANILA_REPO=owner/repo sh init-server.sh --worker https://term.example.com --code XXXX-XXXX --version v0.0.3
#
# Options:
#   --code CODE      pairing code from "Add machine" (needed for the first run)
#   --binary PATH    install this anila-service binary, or
#   --version TAG    download that release from GitHub, checked against its
#                    SHA256SUMS (set ANILA_REPO=owner/repo; public downloads need no token)
#   --worker URL     your Worker URL (required for a new machine or --code)
#   --name NAME      machine name shown in the web app (default: hostname)
#   --user USER      service user (default cloud-user)
#   --no-start       install and pair, but do not enable or start the service
#
# Behind Cloudflare Access, pass a service token that Access allows on
# /api/agent/ and /ws/agent in the environment (not as arguments, so it
# stays out of shell history and the process list):
#
#   ANILA_ACCESS_CLIENT_ID=<id>.access ANILA_ACCESS_CLIENT_SECRET=<secret> \
#     sh init-server.sh --code XXXX-XXXX --binary ./anila-service
#
# Safe to run again: it keeps the user, replaces the binary and the unit,
# and pairs again only when given a new --code.
set -eu

repo=${ANILA_REPO:-}
worker=
user=cloud-user
code=
binary=
version=
name=
start=true

fail() { printf 'init-server: %s\n' "$1" >&2; exit 1; }
say() { printf '==> %s\n' "$1"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --code) code=${2:?--code needs a value}; shift 2 ;;
    --binary) binary=${2:?--binary needs a path}; shift 2 ;;
    --version) version=${2:?--version needs a tag}; shift 2 ;;
    --worker) worker=${2:?--worker needs a URL}; shift 2 ;;
    --name) name=${2:?--name needs a value}; shift 2 ;;
    --user) user=${2:?--user needs a name}; shift 2 ;;
    --no-start) start=false; shift ;;
    -h | --help) sed -n '2,35p' "$0"; exit 0 ;;
    *) fail "unknown option $1 (try --help)" ;;
  esac
done

# Check configuration before installing packages or changing the machine.
[ -z "$code" ] || [ -n "$worker" ] || fail "--worker URL is required with --code"
[ -z "$version" ] || [ -n "$repo" ] || fail "set ANILA_REPO=owner/repo for release downloads"
service_home=$(getent passwd "$user" | cut -d: -f6 || true)
[ -n "$worker" ] || [ -f "$service_home/.config/anila/service.toml" ] ||
  fail "--worker URL is required for a new machine"

# 1. Root, Linux, systemd.
[ "$(id -u)" -eq 0 ] || fail "run as root (sudo sh $0 ...)"
[ "$(uname -s)" = Linux ] || fail "this script sets up Linux servers"
if [ "$start" = true ]; then
  command -v systemctl >/dev/null || fail "systemd is required (or pass --no-start)"
fi
if [ -n "$worker" ]; then
  case "$worker" in https://* | http://localhost:* | http://127.0.0.1:*) ;; *) fail "--worker must be an https:// URL (or loopback for development)" ;; esac
  case "$worker" in *[!A-Za-z0-9.:/_-]*) fail "--worker contains invalid URL characters" ;; esac
fi
[ -n "$binary" ] && [ -n "$version" ] && fail "pass --binary or --version, not both"
[ -n "$binary" ] || [ -n "$version" ] || [ -x /usr/local/bin/anila-service ] ||
  fail "pass --binary PATH or --version TAG to install anila-service"

case "$(uname -m)" in
  x86_64 | amd64) target=x86_64-unknown-linux-gnu ;;
  aarch64 | arm64) target=aarch64-unknown-linux-gnu ;;
  *) fail "no anila-service build for $(uname -m)" ;;
esac

# Release builds link against glibc 2.34 or newer.
if command -v ldd >/dev/null; then
  glibc=$(ldd --version 2>&1 | head -n1 | grep -oE '[0-9]+\.[0-9]+$' || true)
  if [ -n "$glibc" ]; then
    major=${glibc%.*}; minor=${glibc#*.}
    if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 34 ]; }; then
      fail "glibc $glibc is too old for the release build (needs 2.34+, e.g. Ubuntu 22.04, Debian 12)"
    fi
  fi
fi

# 2. Packages: tmux, plus curl and a JSON reader for --version.
install_packages() {
  if command -v apt-get >/dev/null; then
    DEBIAN_FRONTEND=noninteractive apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null
  elif command -v dnf >/dev/null; then dnf install -y -q "$@"
  elif command -v yum >/dev/null; then yum install -y -q "$@"
  elif command -v zypper >/dev/null; then zypper -q install -y "$@"
  else fail "install $* yourself (no apt-get, dnf, yum or zypper found)"
  fi
}
need=
command -v tmux >/dev/null || need="$need tmux"
if [ -n "$version" ]; then
  command -v curl >/dev/null || need="$need curl"
  command -v python3 >/dev/null || need="$need python3"
fi
if [ -n "$need" ]; then
  say "installing$need"
  # shellcheck disable=SC2086 # word splitting is intended
  install_packages $need
fi

# 3. The service user.
if id "$user" >/dev/null 2>&1; then
  say "user $user exists"
else
  say "creating user $user"
  useradd --create-home --shell /bin/bash "$user"
fi
service_home=$(getent passwd "$user" | cut -d: -f6)
[ -d "$service_home" ] || fail "home directory of $user not found"

# 4. The binary.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
if [ -n "$version" ]; then
  github_get() {
    accept=$1
    shift
    if [ -n "${GITHUB_TOKEN:-}" ]; then
      curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: $accept" "$@"
    else
      curl -fsSL -H "Accept: $accept" "$@"
    fi
  }
  archive="anila-service-${version#v}-$target.tar.gz"
  say "downloading $archive from $repo $version"
  api="https://api.github.com/repos/$repo"
  github_get application/vnd.github+json "$api/releases/tags/$version" > "$work/release.json" || fail "release $version not found in $repo"
  for asset in "$archive" SHA256SUMS; do
    url=$(python3 -c '
import json, sys
release = json.load(open(sys.argv[1]))
print(next(a["url" if len(sys.argv) > 3 else "browser_download_url"] for a in release["assets"] if a["name"] == sys.argv[2]))' "$work/release.json" "$asset" ${GITHUB_TOKEN:+authenticated}) ||
      fail "$asset is not in release $version"
    github_get application/octet-stream -o "$work/$asset" "$url"
  done
  (cd "$work" && grep "  $archive\$" SHA256SUMS > check.sha256 && sha256sum -c check.sha256) ||
    fail "checksum mismatch for $archive; not installing it"
  tar -xzf "$work/$archive" -C "$work"
  binary="$work/${archive%.tar.gz}/anila-service"
fi
if [ -n "$binary" ]; then
  [ -f "$binary" ] || fail "$binary not found"
  "$binary" --version >/dev/null 2>&1 || fail "$binary does not run on this machine"
  install -m 0755 "$binary" /usr/local/bin/anila-service.new
  mv -f /usr/local/bin/anila-service.new /usr/local/bin/anila-service
  say "installed $(/usr/local/bin/anila-service --version)"
fi

as_user() { runuser -u "$user" -- env HOME="$service_home" "$@"; }

# 5. Sessions land as the service user, who then switches to their own
# account with `su`. The service user gets no password and no sudo.
config="$service_home/.config/anila/service.toml"
if [ ! -f "$config" ]; then
  say "writing $config"
  install -d -m 0700 -o "$user" -g "$(id -gn "$user")" "$service_home/.config" "$service_home/.config/anila"
  cat > "$config" <<EOF
# anila-service config, written by init-server.sh. \`anila-service pair\`
# fills in host.
host   = "unpaired"
worker = "$worker"

[[preset]]
id    = "shell"
label = "Shell ($user, then su - <your user>)"
dir   = "~"
EOF
  chown "$user:$(id -gn "$user")" "$config"
  chmod 0600 "$config"
fi
id_env=${ANILA_ACCESS_CLIENT_ID:-}
secret_env=${ANILA_ACCESS_CLIENT_SECRET:-}
if [ -n "$id_env" ] || [ -n "$secret_env" ]; then
  [ -n "$id_env" ] && [ -n "$secret_env" ] ||
    fail "set both ANILA_ACCESS_CLIENT_ID and ANILA_ACCESS_CLIENT_SECRET, or neither"
  case "$id_env$secret_env" in
    *[!A-Za-z0-9._-]*) fail "the Access service token has unexpected characters" ;;
  esac
  say "saving the Cloudflare Access service token in $config"
  awk -v id="$id_env" -v secret="$secret_env" '
    /^access_client_(id|secret)[[:space:]]*=/ { next }
    { print }
    /^worker[[:space:]]*=/ && !done {
      print "access_client_id = \"" id "\""
      print "access_client_secret = \"" secret "\""
      done = 1
    }' "$config" > "$config.new"
  chown "$user:$(id -gn "$user")" "$config.new"
  chmod 0600 "$config.new"
  mv -f "$config.new" "$config"
fi
passwd -l "$user" >/dev/null 2>&1 || true
banner='# anila: su hint'
if ! grep -qF "$banner" "$service_home/.bashrc" 2>/dev/null; then
  cat >> "$service_home/.bashrc" <<EOF

$banner
if [ -n "\${TMUX:-}" ] && [ "\$(id -un)" = "$user" ]; then
  printf '\\nThis is the shared %s shell. Switch to your own account:\\n  su - <your-user>\\n\\n' "$user"
fi
EOF
  chown "$user:$(id -gn "$user")" "$service_home/.bashrc"
fi

# 6. Pair the machine with your account.
if [ -n "$code" ]; then
  say "pairing with $worker"
  set -- pair "$code" --worker "$worker"
  [ -n "$name" ] && set -- "$@" --name "$name"
  as_user /usr/local/bin/anila-service "$@"
elif ! grep -q '^host *= *"unpaired"' "$config"; then
  say "already paired ($config); pass --code to pair again"
else
  fail "not paired yet: in the web app choose \"Add machine\" and pass its code with --code"
fi
as_user /usr/local/bin/anila-service check-config >/dev/null || fail "$config is not valid"

# 7. Run it under systemd.
if [ "$start" = false ]; then
  say "done (not started: --no-start)"
  exit 0
fi
cat > /etc/systemd/system/anila.service <<EOF
[Unit]
Description=anila host service (serves tmux sessions to the anila web app)
After=network-online.target
Wants=network-online.target

[Service]
User=$user
Group=$(id -gn "$user")
WorkingDirectory=$service_home
Environment=HOME=$service_home
Environment=LANG=C.UTF-8
Environment=RUST_LOG=info
ExecStart=/usr/local/bin/anila-service run
Restart=always
RestartSec=5
# tmux servers started by the service must survive a service restart.
KillMode=process

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable anila.service >/dev/null 2>&1
systemctl restart anila.service
attempt=0
while ! systemctl is-active --quiet anila.service && [ "$attempt" -lt 30 ]; do
  attempt=$((attempt + 1))
  sleep 1
done
if systemctl is-active --quiet anila.service; then
  say "anila.service is running as $user; it should show as online in the web app"
  say "logs: journalctl -u anila -f"
else
  journalctl -u anila --no-pager -n 20 >&2 || true
  fail "anila.service did not start"
fi
