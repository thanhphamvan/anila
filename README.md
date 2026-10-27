# anila

Your tmux sessions, from any browser. Run a Rust service on your Linux or
macOS machine and deploy the Rust/WebAssembly web app and Worker into your
own Cloudflare account. The service connects outward, so your machine does
not need a public IP or an inbound firewall port.

Passkeys protect sign-in. Accounts own machines and can invite other people
or share a machine. Byte-offset replay resumes terminal output after short
network interruptions while the service's replay buffer retains it.

**[Deploy on your own machine and Cloudflare account →](docs/self-hosting.md)**

## Architecture

| Folder | Runs on | Purpose |
| --- | --- | --- |
| `service/` | Linux or macOS | Native service, tmux/PTYS, replay buffers, ed25519 machine authentication |
| `worker/` | Cloudflare Workers | HTTP API, WebAuthn, SQLite-backed AuthStore and HostRoom Durable Objects, static assets |
| `frontend/` | Browser | Rust/wasm Leptos app with vendored xterm.js |
| `proto/` | Native and wasm | Shared protocol, codecs, limits and sans-IO client state machine |
| `e2e/` | Development machine | Functional, browser, fault-injection and benchmark harnesses |

The frontend and Worker deploy together as one Worker with static assets;
Cloudflare Pages, D1, R2, a separate database server and an external identity
provider are not required. Login uses passkeys only.

## Local development

Install stable Rust with the `wasm32-unknown-unknown` target, tmux, a supported
Node.js LTS with npm, and Trunk. Python 3.11+ is needed by deployment tooling.

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk --version 0.22.0 --locked
cargo install worker-build --version 0.8.7 --locked
cargo build --locked -p anila-service
mkdir -p ~/.config/anila
cp service/service.example.toml ~/.config/anila/service.toml
```

Set `worker = "http://localhost:8787"` in that service config and remove any
presets you do not use. Then generate this machine's signing key:

```sh
target/debug/anila-service keygen
```

Create `worker/.dev.vars` (ignored by Git):

```dotenv
ORIGIN="http://localhost:8787"
RP_ID="localhost"
DEFAULT_HOST="my-mac"
HOST_KEYS='{"my-mac":"<PUBLIC key printed by keygen>"}'
```

Build and start the Worker:

```sh
(cd frontend && trunk build --release)
(cd worker && npx -y wrangler@4.148.0 dev --port 8787)
```

In another terminal:

```sh
target/debug/anila-service run
```

And in a third:

```sh
target/debug/anila-service enroll
```

Open `http://localhost:8787`, select **First time on this device?**, and enter
that one-time code to create the first administrator account and passkey.
Never use the private signing key as `HOST_KEYS`; that map contains public
keys only. Keep the generated key private and back it up securely.

For isolated experiments, `DEV_INSECURE="1"` bypasses authentication only
when the configured origin is localhost. The end-to-end harness supplies
its own isolated configuration; move `.dev.vars` aside before running it.

## Accounts and trust

The first enrollment creates the deployment administrator. Administrators
invite accounts; owners pair more machines and share them with members.
Account settings can enroll another device. Sensitive actions require a
fresh passkey ceremony.

Everyone with access to a machine can attach to **every tmux session on
that machine**, including a session where someone ran `su - alice`.
Attaching there types as Alice. Share machines only among people who trust
each other; use separate machines for isolation between untrusted users.

This public edition supports fresh installations. It refuses the private
IdP-era auth store (schema 3) and unknown newer schemas. Use a new Worker
name and Durable Object namespaces; see the deployment guide before moving
an existing installation. The terminal protocol version has not changed.

## Validation and benchmarks

```sh
cargo fmt --all --check
cargo test --workspace --exclude anila-frontend --exclude anila-e2e --locked
cargo clippy --workspace --all-targets --exclude anila-frontend --locked -- -D warnings
cargo clippy -p anila-worker -p anila-frontend --target wasm32-unknown-unknown --locked -- -D warnings
python3 -m unittest discover -s scripts -p 'test_*.py'
node --test frontend/tests/sw.test.cjs
cargo test -p anila-e2e --test functional --test tenancy --test passkey_only --test upgrade -- --nocapture --test-threads=1
cargo test -p anila-e2e --test chaos -- --nocapture
cargo test -p anila-e2e --test browser -- --nocapture
```

The end-to-end suites need tmux, worker-build, pinned Wrangler, and free
loopback ports. Browser suites also need Chrome or Chromium; set
`ANILA_BROWSER=/path/to/browser` if automatic discovery fails. They use
private tmux servers and test data. `upgrade` tests persistence and refusal
of unsupported storage without requiring private Git tags.

[Benchmark commands and historical measurements](bench/README.md) and the
[open-source benchmark specification](bench/OPEN_SOURCE_BENCHMARK_PLAN.md)
explain latency, codec, recovery and phone measurements and their limits.
Historical measurements do not certify this edition or prove a language
comparison. [Multi-account design](docs/multi-tenant.md),
[optimization decisions](docs/optimizations.md) and the
[phone checklist](docs/phone-checklist.md) provide implementation context.

## License and contributions

Copyright © 2026 Van-Thanh Pham. Released under the [MIT License](LICENSE).
Dependencies retain their own licenses; see [third-party notices](THIRD_PARTY_NOTICES.md).

See [CONTRIBUTING.md](CONTRIBUTING.md) for changes and [SECURITY.md](SECURITY.md)
for private vulnerability reports. Contact: [code@thanhpv.me](mailto:code@thanhpv.me)
or [phamthanh_cis60@outlook.com](mailto:phamthanh_cis60@outlook.com).
