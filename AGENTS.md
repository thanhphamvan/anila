# Working on anila

Read README.md, CONTRIBUTING.md and docs/self-hosting.md. This repository is
the MIT, passkey-only edition. Do not restore external IdP login or private
production defaults. Use the public edition's own Git history and tests.

- Wire changes start in proto/ and update shared vectors and both peers.
- Keep client/channel state machines sans-IO; add regression tests for fixes.
- Format touched Rust files only: rustfmt --edition 2024 <files>.
- Poll test conditions; use isolated tmux servers. Never type into real panes.
- Preserve service/src/pty.rs writer/reaper semantics.
- Never print or commit secrets, session cookies or provisioning codes.
- Preserve frontend/vendor/xterm/ and its licenses unless upgrading on purpose.
- Run the README checks and relevant end-to-end suites; report unavailable checks.
- Deployment, publication and release tags require an explicit user request.
