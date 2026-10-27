# Validation of the public edition

Run on 2026-10-27 in the new local repository. The source origin was
`9d47c6b`; this edition removes the IdP feature and changes licensing,
deployment tooling, documentation and authentication-store guards.

## Passed

- `cargo fmt --all --check` and staged whitespace checks.
- `cargo test --workspace --exclude anila-frontend --exclude anila-e2e --locked`:
  native unit/contract suites, including real PTY/tmux and link tests. The
  Worker suite was rerun after adding the store-schema guard and its test.
- Both CI clippy passes: native workspace/all targets except frontend, and
  Worker/frontend on `wasm32-unknown-unknown`, with `-D warnings`.
- `(cd frontend && trunk build --release)`.
- Five Python deployment/configuration/installer guard tests; shell syntax
  checks of deploy, installer and release-verification scripts.
- Three service-worker tests with Node's test runner.
- Local links in seven new/updated guides and workflow YAML syntax checks.
- End-to-end `functional`, `tenancy`, `passkey_only`, `upgrade`,
  `harness_proxy`, `harness_probe`, `proxy_bandwidth`, `proxy_latency` and
  benchmark-helper suites. The benchmark template-regeneration test remains
  intentionally ignored, as in the original repository.
- Third-party service runtime notice collection for all four release targets:
  aarch64/x86_64 macOS and aarch64/x86_64 Linux. This validates notice
  collection, not cross-platform compilation or execution.
- A targeted scan of prospective source/documentation files found no matches
  for private-key blocks, GitHub tokens, AWS access-key IDs or literal Access
  secrets. Values were suppressed. This limited pattern scan is not a
  comprehensive secret or security audit.

## Browser and chaos verification

The real-browser suite passed enrollment, terminal input, reconnect/reload,
PWA/offline behavior, additional-machine pairing, sharing and account settings.
The chaos suite passed connection hangs/cuts, latency, service sleep/crash,
tmux-client/server failure and the seeded randomized fault schedule.
The first Chromium launch failed before application interaction because
Ubuntu restricted its sandbox. The retry uses an isolated test-only wrapper
with `--no-sandbox`, against this local app and generated test data. Product
and harness code do not add that flag by default.

## Environment limitations and unrun checks

The privileged systemd-container installer test was attempted with a local
Linux service build. Its Ubuntu systemd container exited before the test
could create its test user or invoke the installer. A separate container
startup reproduced the exit. The installer integration therefore remains
unverified here; the existing dedicated GitHub workflow runs it on a hosted
Ubuntu runner. Configuration guards and shell syntax passed locally.

No production deployment, independent-account Cloudflare smoke test,
macOS/launchd test, physical-phone run or new performance measurement was
performed. See the deployment guide and benchmark specification for those
procedures. Ten upstream crates lack full-text license notices in the
available source/supplements; see THIRD_PARTY_NOTICES.md before distributing
binaries that include them, especially frontend wasm.

## Shared frontend change

The common modules now live in `frontend/src/shared/`, with a passkey-only
adapter at `frontend/src/edition.rs`. Downstream private adapters can consume
these modules and assets without maintaining a second copy of the UI.

For this refactor, both public and private frontend wasm clippy checks passed
with warnings denied. Both release frontend builds passed. The public browser
suite and `passkey_only` removed-route test passed; the private consumer's
IdP browser, passkey browser and IdP HTTP suites also passed. Four Python
consumer tests cover live shared edits, the preserved private adapter, an
independent CI checkout, the local HTML build context and source-revision
validation. GitHub workflow changes in the consumer passed actionlint 1.7.7.
No live deployment or remote workflow run was performed.

## Terminal clipboard fix

The browser regression selects rendered terminal text with real pointer
events and reads the browser clipboard. With the old frontend, Ctrl+Shift+C
left the clipboard sentinel unchanged despite a visible terminal selection.
With the fix it passes Ctrl+Shift+C, Ctrl+C with selection, Cmd+C, Shift-drag
selection while tmux mouse reporting is active, clipboard-denial feedback,
Ctrl+Shift+C with no selection, native Ctrl+Shift+V paste, and Ctrl+C with no
selection interrupting the isolated cat process. All waits poll conditions.

Both frontend variants pass wasm clippy with warnings denied; the native e2e
harness clippy, formatting, service-worker tests and workflow lint also pass.
The new browser test is included in public CI. Vendored xterm is unchanged.
The private consumer derives the additional KeyboardEvent feature from the
shared manifest. No running deployment was updated during this fix.
