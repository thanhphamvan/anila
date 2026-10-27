# Protocol and client optimizations (v0.0.3)

Status: built on the `v0.0.3` branch (T36 to T40, 2026-10-22). O1 cut the
median echo by about 11 ms; O4 measured and kept the codecs
(`bench/README.md`). O3's saving shows only in production. Background and the ideas we rejected
are in `bench/README.md` ("Next optimizations", "Why not gRPC").

Three changes, each shippable on its own, plus one measurement. None
changes the byte format of data frames, and none bumps `PROTO`.

| # | Change | Who feels it | Touches |
| - | ------ | ------------ | ------- |
| O1 | Echo without the batching wait | every keystroke: about 8 ms sooner | `service/` |
| O2 | Instant tab switches | switching tabs: no re-attach, scrollback kept | `frontend/` |
| O3 | The host room sleeps when nobody watches | Cloudflare bill, per machine | `proto/`, `worker/`, `service/` |
| O4 | Measure zstd decode cost on phones | decides whether tiny frames go uncompressed | `e2e/` (bench) |

Build order: O1, O2a, O3, O2b, O4. Tasks T36 to T40 at the end.

## O1. Echo without the batching wait

**Now.** `Coalescer::push` (`service/src/coalesce.rs`) opens a frame on a
channel's first chunk and holds it for the whole window
(`Timings::batch` in `service/src/link.rs`, 8 ms). A keystroke echo always arrives on a quiet
channel, so it always waits the full 8 ms.

**Change.** A chunk that arrives on a channel with nothing pending, and
whose previous frame went out at least one window ago, is sent at once.
Any chunk that follows within the window is batched exactly as today, with
its deadline anchored to that chunk. A burst therefore costs at most one
extra small frame (its first chunk), and a burst's frame rate stays bounded
by the window.

```text
quiet ──key──▶ echo sent now (was: +8 ms)
burst ──c1──▶ sent now
      ──c2,c3,…──▶ batched for 8 ms, as today
```

- `Coalescer` keeps `last_sent: Instant` per key, updated whenever `push`
  or `due` returns that key's bytes. It stays sans-IO; `now` is passed in.
- A zero window keeps its current meaning (no batching).
- No protocol, worker or browser change.

**Acceptance.**
- Unit tests in `coalesce.rs`: a chunk after quiet returns immediately; a
  second chunk within the window is held; a chunk exactly one window after
  the last send returns immediately; the size cap still splits; per-key
  independence.
- `anila-bench codecs` on the recorded workloads: frame count and wire
  bytes within +2 % of the current 8 ms row.
- `anila-bench net`: echo p50 on `lan` and `good4g` drops by about the
  window (≥ 5 ms); throughput unchanged within noise.

## O2. Instant tab switches

**Now.** One xterm and one channel exist at a time. Switching tabs closes
the channel (the service detaches tmux), disposes the xterm, and attaches
again: a round trip, a full tmux redraw, and the xterm scrollback is lost.
A tab on another machine also drops the socket and reconnects.

### O2a. Keep tabs open on the current machine

- A terminal pool in the frontend, keyed by (machine, session). Each entry
  owns its xterm, fit addon, its `<div>`, the channel and its last size.
- Every pooled terminal stays attached. Hidden ones keep receiving and
  rendering output (with the `hidden` attribute), so showing one is a pure
  UI change.
- Showing a terminal: unhide, `fit()`, send `resize` if the size changed,
  focus. Hidden terminals never call `fit()`, which cannot measure a
  hidden element.
- The event sink routes `Output`, `Reset`, `Exited` and `Error` events by
  channel to the matching pool entry, instead of to the one terminal.
- Cap: at most `MAX_CHANNELS_PER_TAB` (8) channels per machine, which the
  service enforces already. Opening a ninth closes the least recently
  shown terminal's channel and disposes its xterm. Its tab stays, and it
  re-attaches when it is next shown.
- Closing a tab closes its channel and disposes its xterm, as today.
- The banner, the paused-input notice and the key row belong to the shown
  terminal.

### O2b. One socket per machine with open tabs

- `Model.conn` becomes a map from machine to `Connection`, with
  `connected`, `agent` and the session list kept per machine. The tab bar
  shows each tab's machine status: the tab's text turns to the warning
  colour while that machine is offline or reconnecting.
- Switching to a tab on another machine reuses that machine's socket and
  pool, with no reconnect.
- Cap: sockets to at most 4 machines. Opening a fifth closes the least
  recently used machine's socket and pool. Its tabs stay and reconnect
  when shown.
- The Machines screen no longer drops the current socket. Sign-out,
  `4401` and `4403` close the affected sockets as today.

**Cost to accept.** Hidden tabs keep using data while their program
prints, for example a background `claude` run. The flow window (512 KiB
unacked) bounds bursts, not the total. If that matters on 4G, a later
change can detach tabs that stay hidden for a long time.

**Acceptance (browser suite).**
- Open two sessions on one machine, type in each, switch back and forth:
  no new tmux client attaches on a switch, and text typed before the
  switch is still in the xterm (scrollback kept).
- A hidden tab's program prints; showing the tab shows that output
  without a redraw round trip.
- Resize the window while a tab is hidden; on showing it, tmux gets the
  new size.
- Open nine sessions on one machine: the oldest is detached, and its tab
  re-attaches when clicked.
- O2b: tabs on two machines; switching between them opens no new socket
  (count sockets in the page), and stopping one machine's service marks
  only its tabs.

## O3. The host room sleeps when nobody watches

**Now.** The service sends `heartbeat` every 10 s. It is not an exact
auto-response text, so it wakes the room. The room's alarm also re-arms
every 15 s while any socket is open (`worker/src/room.rs`, `alarm` and
`Hop::Heartbeat`). The service is always connected, so each machine's room
stays in memory all day and bills Durable Object duration.

The ideal fix (send only auto-answered pings, and read the socket's
`getWebSocketAutoResponseTimestamp`) is blocked: workers-rs 0.8.7 does not
bind it, and `State` keeps the underlying JS object private. The design
below needs no new binding.

**Change: heartbeat only while watched.** The room tells the service
whether any browser is connected. While nobody watches, the service sends
the exact `PING_TEXT`, which the auto-response answers without waking the
room. That keeps the service's own dead-link detection. While watched, it
sends `heartbeat` as today, so the room can tell a hung service from a live
one.

Contract additions in `proto/` (additive; old peers ignore them):

- `Hop::Auth` gains `watch: bool` (`#[serde(default)]`): the service
  understands `watch` frames.
- New `Hop::Watch { on: bool }`, room → service.
- `limits::WATCH_PROBE_SECS = 5`: after `watch { on: true }`, how long the
  room waits for the first heartbeat.
- Golden vectors for both frames in `proto/vectors/`.

Service:
- `watched` starts as `true`, so a room that never sends `watch` gets
  heartbeats as today.
- On `watch { on }`, it sets `watched`. On `on: true` it sends a
  heartbeat at once.
- Each 10 s tick sends `heartbeat` if watched, otherwise `PING_TEXT`. Both
  get a `pong`, so link health checks are unchanged.

Room:
- After a service authenticates with `watch: true`, it sends
  `watch { on: <any browser connected> }`.
- When the first browser socket opens, it sends `watch { on: true }`,
  records `watch_since`, and sets the alarm for `WATCH_PROBE_SECS`. If no
  heartbeat has arrived since `watch_since` when that alarm fires, the
  service is hung: mark it offline, close it with `4408`, broadcast the
  status (as the current alarm does).
- When the last browser socket closes, it sends `watch { on: false }`.
- The alarm re-arms only while a browser socket is open, not while
  any socket is open. Session expiry and hung checks run only then, which
  is the only time they matter.

Compatibility:

| Service | Room | Result |
| ------- | ---- | ------ |
| old (no `watch`) | new | room never sends `watch`; heartbeats as today |
| new | old | never told; `watched` stays true; heartbeats as today |
| new | new | heartbeats only while a browser is connected |

**What a browser sees.** It connects and gets `online` from the
authenticated socket. If the service hung while nobody watched, the status
turns `offline` within `WATCH_PROBE_SECS` (5 s), sooner than today's worst
case of 25 + 15 s.

**Acceptance.**
- Unit: the vectors decode in every crate.
- `service/tests/watch.rs` (a fake room): the service offers `watch: true`;
  it heartbeats when never told; after `watch { on: false }` it sends only
  the exact `PING_TEXT`; after `watch { on: true }` a heartbeat arrives at
  once and heartbeats continue.
- `e2e/tests/watch.rs` (the test plays the service against the real
  worker):
  - With no browser, the room sends `watch { on: false }` and never closes
    the quiet service, even after `DEAD_AFTER_SECS + HUNG_AGENT_CHECK_SECS`.
  - A browser connects: `watch { on: true }`; heartbeats keep it online
    past the probe window. The last browser leaves: `watch { on: false }`.
  - A browser connects while the service is hung (no heartbeat): the room
    closes the service with `4408` and the browser sees `offline`, within
    `WATCH_PROBE_SECS` + 3 s.
  - A service without `watch` never gets a `watch` frame and stays online.
- The existing chaos test X7 (the host machine sleeps while watched) still
  passes, and the real service passes the functional and browser suites.
- Production check (by the owner, in the Cloudflare dashboard): HostRoom
  Durable Object duration (GB-s) per day before and after deploy, with no
  browser open overnight. Local wrangler does not really hibernate, so
  only production can show the saving.

## O4. Measure zstd decode cost on phones

**Question.** Every frame, even a one-byte echo, is compressed and decoded
as a zstd frame against a 32 KiB history dictionary. The codec benchmark
measured size. We do not know the CPU cost in a phone browser.

**Build.** `anila-bench phone-cpu`:
- Frames the recorded workloads the way production does (8 ms, 32 KiB) for
  `none`, `deflate_stream` and `zstd_stream`.
- Decodes them with the real wasm decoder in headless Chromium under CPU
  throttling of 4× and 6× (`Emulation.setCPUThrottlingRate`, roughly a
  mid-range phone).
- The decoder comes from a feature-gated bench build of the frontend
  (`--features bench`); the production bundle does not change.
- Reports p50/p95 decode time per frame, split into frames under 64 bytes
  and the rest, and total CPU ms per MB.
- Writes `bench/results/<date>-<commit>-phone-cpu.{jsonl,md}`.

**Decision rule, recorded in `bench/README.md`.** If, at 4×, zstd's p95 for
frames under 64 bytes exceeds 1 ms, or its CPU per MB is more than twice
deflate's, propose sending small frames uncompressed. That needs a new
codec id, because both sides must then add raw frames to the zstd history,
and an old browser would not. Otherwise, keep the current codecs.

## Not doing

- Varint headers or binary acks: a few bytes per packet, at the cost of a
  `PROTO` bump (`bench/README.md`).
- gRPC, protobuf or WebTransport (`bench/README.md`).

## Tasks

The offline Codex agents can build and unit-test these. The reviewer runs
the e2e and bench parts.

| Task | Scope | Files | Reviewer runs |
| ---- | ----- | ----- | ------------- |
| T36 | O1 coalescer | `service/src/coalesce.rs`, `service/src/link.rs` | `bench` codecs + net |
| T37 | O2a terminal pool | `frontend/src/shared/app.rs`, `frontend/src/shared/tabs.rs`, new `frontend/src/shared/pool.rs`, `frontend/style.css` | `browser` |
| T38 | O3 watch | `proto/src/{control,limits}.rs`, `proto/vectors/`, `worker/src/room.rs`, `service/src/link.rs` | `tenancy`, `chaos`, `versions`, new watch checks |
| T39 | O2b socket per machine | `frontend/src/{app,conn,tabs,hosts}.rs` | `browser` |
| T40 | O4 phone CPU bench | `e2e/src/bin/anila-bench*`, `frontend` bench feature | `phone-cpu` |

Ship O1 to O3 together as v0.0.3, once released and deployed when the
owner asks. O4's outcome may add a v0.0.4 codec.
