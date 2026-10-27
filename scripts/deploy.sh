#!/bin/sh
# Builds the web app and deploys the worker to your Cloudflare account.
# Refuses to deploy while worker/wrangler.toml still has placeholder values.
# Usage: scripts/deploy.sh            (run `npx wrangler login` once first)
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
toml="$root/worker/wrangler.toml"

fail() { printf 'deploy: %s\n' "$1" >&2; exit 1; }

[ -f "$root/worker/.dev.vars" ] && printf 'deploy: note: worker/.dev.vars is ignored by deploys (local only)\n'
command -v python3 >/dev/null || fail "Python 3.11+ is required"
command -v trunk >/dev/null || fail "trunk is not installed (cargo install trunk --version 0.22.0 --locked)"
# Use the same validation and custom-domain routing as GitHub Actions.
python3 "$root/scripts/ci_wrangler_config.py" "$toml" "$root/worker/.deploy.toml" --from-toml
trap 'rm -f "$root/worker/.deploy.toml"' EXIT
(cd "$root/worker" && npx -y wrangler@4.148.0 whoami)
(cd "$root/frontend" && trunk build --release)
(cd "$root/worker" && npx -y wrangler@4.148.0 deploy -c .deploy.toml)

if ! (cd "$root/worker" && npx -y wrangler@4.148.0 secret list 2>/dev/null | grep -q VAPID_PRIVATE_KEY); then
  printf '\nPush notifications are off until you set the VAPID key:\n'
  printf '  anila-service vapid-keygen\n  (cd worker && npx -y wrangler@4.148.0 secret put VAPID_PRIVATE_KEY)\n'
fi
printf '\nDeployed. Next on the host machine: anila-service run, then anila-service enroll.\n'
