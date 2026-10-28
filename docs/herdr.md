# Native herdr sessions

Install [herdr](https://github.com/herdrdev/herdr) separately on the host,
as the same Unix user that runs `anila-service`. Anila invokes its executable;
it does not embed herdr or require a tmux session around it. The integration
is tested against herdr v0.9.5 on Linux. Both projects' source code remains
under their respective licenses (Anila MIT, herdr Apache-2.0).

Add `herdr_binary` near the top of `~/.config/anila/service.toml`, before
any `[[preset]]` section. Use an absolute executable path if the systemd or
launchd service PATH does not contain herdr:

```toml
herdr_binary = "/usr/local/bin/herdr"

[[preset]]
id = "herdr"
label = "Herdr workspace"
backend = "herdr"
dir = "~/code"
```

Keep your existing tmux presets to offer both backends on the same machine.
Omitting `backend` means tmux. A herdr-only machine needs no tmux executable;
the standard server installer still installs tmux for its default preset.
Restart `anila-service`, refresh the web session list, choose **New session**,
and select the Herdr preset. Give it a native name using 1–32 ASCII letters,
digits, underscores or hyphens. Do not include `herdr:` in the creation form.

Herdr presets use herdr's own configuration and workspace commands for shells
and agents; Anila rejects `cmd` on a herdr preset rather than silently ignoring
it. `dir` seeds a new herdr workspace. Herdr's first-run onboarding, keybindings,
and terminal UI are displayed in the browser as they are in a local terminal.

Session identifiers use `herdr:<native-name>` in lists, tabs and the wire
protocol. Plain identifiers continue to mean tmux, so the same native name
can exist in both backends without tab, resume, or attachment collisions.
This namespace cannot collide with a valid tmux name (tmux disallows colons).
Herdr sessions outside Anila's supported native-name alphabet/length are not
listed. Running and stopped saved named herdr sessions are listed; opening a
stopped session lets herdr restore its saved layout. The default session is
listed only while running, since herdr always reports it even before creation.
Herdr does not expose tmux window/attachment/activity metrics in this CLI;
the UI labels these sessions without inventing those measurements.

Closing a browser tab or expiring Anila's reconnect grace period kills only
the herdr client. The herdr server and pane processes keep running; attaching
again reconnects to that server. A host reboot or stopping the herdr server
ends those processes; herdr may restore layout and supported agent state.
Run `herdr session list --json` or `anila-service sessions` as the service user
to inspect sessions. Use `herdr session stop <name>` to stop one deliberately.

Upgrade the service, Worker and frontend together before enabling herdr.
Older builds do not understand namespaced session identifiers. The Cloudflare
Worker continues to relay the same terminal frames; herdr runs only on the
host, and the existing account/machine authorization applies to both backends.
Anyone authorized for the machine can access its herdr sessions too.

Run the real-binary lifecycle regression without touching personal sessions:

```sh
ANILA_HERDR_BINARY=/absolute/path/to/herdr cargo test -p anila-service --test herdr -- --nocapture
```

The test creates a subprocess with temporary HOME and XDG directories,
starts only its own server, exercises creation/duplicates, input, resize,
detach and reattach, and stops that server on success or assertion failure.
Without `ANILA_HERDR_BINARY`, the native integration test reports it is skipped;
protocol, adapter and configuration unit tests still run.

In the public checkout, the browser regression additionally creates a same-named tmux session and
checks the creation form, terminal input routing and shell state after reload:

```sh
ANILA_HERDR_BINARY=/absolute/path/to/herdr cargo test -p anila-e2e --test herdr -- --nocapture
```

It requires the usual local Worker and browser test tools. CI downloads the
pinned herdr binary and checks its SHA-256 before running both regressions.
