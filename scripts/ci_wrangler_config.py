"""Writes a deploy config: worker/wrangler.toml with its [vars] replaced by
values from the environment (set by .github/workflows/deploy.yml).

When ANILA_ORIGIN is not a workers.dev URL, the config also claims that
hostname as the Worker's custom domain and turns off the workers.dev and
preview URLs, so the custom domain (and any Cloudflare Access policy on it)
is the only way in.

usage: ci_wrangler_config.py <wrangler.toml> <output.toml> [--from-toml]
"""

import json
import os
from pathlib import Path
import sys
import tomllib
from urllib.parse import urlsplit

REQUIRED = {
    "ORIGIN": "ANILA_ORIGIN",
    "RP_ID": "ANILA_RP_ID",
    "DEFAULT_HOST": "ANILA_DEFAULT_HOST",
    "HOST_KEYS": "ANILA_HOST_KEYS",
}


def toml_string(value: str) -> str:
    return json.dumps(value)  # a JSON string is a valid TOML basic string


def main(src: str, dst: str, from_toml: bool = False) -> None:
    if from_toml:
        with open(src, "rb") as config:
            values = tomllib.load(config).get("vars", {})
        for var, env in REQUIRED.items():
            os.environ[env] = values.get(var, "")
        os.environ["ANILA_VAPID_SUBJECT"] = values.get("VAPID_SUBJECT", "")
    missing = [env for env in REQUIRED.values() if not os.environ.get(env)]
    if missing:
        sys.exit(f"missing environment variables: {', '.join(missing)}")
    try:
        keys = json.loads(os.environ["ANILA_HOST_KEYS"])
    except json.JSONDecodeError as e:
        sys.exit(f"ANILA_HOST_KEYS is not valid JSON: {e}")
    if not isinstance(keys, dict) or not keys:
        sys.exit("ANILA_HOST_KEYS must be a JSON object with at least one host")

    origin = os.environ["ANILA_ORIGIN"]
    parsed = urlsplit(origin)
    host = parsed.hostname or ""
    if (parsed.scheme != "https" or not host or parsed.username or parsed.password
            or parsed.port is not None or parsed.path or parsed.query or parsed.fragment):
        sys.exit("ANILA_ORIGIN must be an HTTPS origin with no path, port or credentials")
    if host == "term.example.com":
        sys.exit("replace the example hostname with your own Worker hostname")
    if os.environ["ANILA_RP_ID"] != host:
        sys.exit("ANILA_RP_ID must equal the hostname of ANILA_ORIGIN")
    # Top-level keys must come before the first table, so they go first.
    out = []
    if not host.endswith(".workers.dev"):
        out += [
            "# Added by ci_wrangler_config.py for a custom domain.",
            "workers_dev = false",
            "preview_urls = false",
            f"routes = [{{ pattern = {toml_string(host)}, custom_domain = true }}]",
            "",
        ]
    in_vars = False
    for line in Path(src).read_text(encoding="utf-8").splitlines():
        stripped = line.split("#", 1)[0].strip()
        if stripped.startswith("["):
            in_vars = stripped == "[vars]"
        if not in_vars:
            out.append(line)

    out.append("")
    out.append("[vars]")
    for var, env in REQUIRED.items():
        out.append(f"{var} = {toml_string(os.environ[env])}")
    out.append(f"VAPID_SUBJECT = {toml_string(os.environ.get('ANILA_VAPID_SUBJECT') or 'mailto:admin@localhost')}")
    with open(dst, "w", encoding="utf-8") as f:
        f.write("\n".join(out) + "\n")
    print(f"wrote {dst} for {os.environ['ANILA_ORIGIN']}")


if __name__ == "__main__":
    if len(sys.argv) not in (3, 4) or (len(sys.argv) == 4 and sys.argv[3] != "--from-toml"):
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2], from_toml=len(sys.argv) == 4)
