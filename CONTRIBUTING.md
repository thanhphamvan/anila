# Contributing

Changes and bug reports are welcome. Describe the behavior you observed,
expected behavior, operating system and versions, and a minimal reproduction.
Remove keys, cookies, pairing/enrollment codes, tokens and private terminal
output before sharing logs. Report vulnerabilities privately as described in
[SECURITY.md](SECURITY.md).

Use the README's build and validation commands. Wire changes begin in
`proto/` and update shared golden vectors and both peers together. Keep
`proto/src/client.rs` and `service/src/channel.rs` sans-IO. Fixes need a
regression test; poll conditions instead of relying on fixed sleeps.
Format only touched Rust files with `rustfmt --edition 2024 <files>`.
Preserve vendored xterm files and notices unless intentionally upgrading.

Use isolated tmux servers in tests. Preserve the PTY writer/reaper behavior:
dropping a portable-pty writer can send Enter and Ctrl-D into a pane.
Do not point automated tests at real sessions or production services.

By submitting a contribution, you agree to license your contribution under
this project's MIT License. Add required third-party notices for new assets
or dependencies and regenerate the dependency inventory after lockfile changes.
