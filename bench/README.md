# Terminal codec and framing benchmark

This first stage measures recorded terminal bytes offline. It compares `none`,
per-frame LZ4 and zstd level 3, persistent zstd level 3 and DEFLATE level 6
streams, and zstd frames with prefix history. Each workload runs through all
combinations of 0/4/16/32 ms batching and 8/32 KiB maximum raw frame sizes
(six codecs, 48 results per workload).

See [the open-source benchmark specification](OPEN_SOURCE_BENCHMARK_PLAN.md)
for reproducibility requirements, measurement limits and proposed release
acceptance gates. Checked-in historical results measure earlier builds.

From the workspace root, with Cargo's existing dependencies available locally:

```sh
export CARGO_NET_OFFLINE=true
cargo run -q -p anila-e2e --bin anila-bench -- synth
cargo run -q -p anila-e2e --bin anila-bench -- codecs
```

`synth` writes deterministic ticker, colored compiler-output, 80×24 TUI redraw,
and pseudorandom workloads to `bench/workloads/`. It overwrites only its four
named workloads. The fixed xorshift seed makes random bytes reproducible; they
are test data, not cryptographic randomness. Short versions drive the tests.

## Record real terminal output

```sh
cargo run -q -p anila-e2e --bin anila-bench -- record build -- cargo build --offline
cargo run -q -p anila-e2e --bin anila-bench -- record claude --cols 120 --rows 40 -- claude
# If the binary is on PATH:
anila-bench record claude -- claude
```

The child runs in the service's PTY. Output is echoed and every output event is
written as `{"t_ms":123,"b64":"..."}` on its own line. Timestamps use a monotonic
clock, in milliseconds since spawn started, sampled when the recorder receives
the PTY read event. Recording ends after the final output/exit event, and reports
bytes and duration. Names accept letters, digits, hyphens and underscores;
recording again with the same name overwrites that file. Stdin is forwarded in
its existing terminal mode (normally line-buffered); raw-mode key handling and
live terminal resize are not implemented. For a full interactive TUI session,
attach to an existing tmux session in the recording PTY and operate it from a
second terminal.

## Results and interpretation

```sh
anila-bench codecs --workloads bench/workloads --out bench/results
cargo test -p anila-e2e --test bench
cargo clippy -p anila-e2e --all-targets
```

The command writes `<UTC-date>-<short-commit>-codecs.jsonl` and a matching
Markdown table, sorted by workload and increasing wire/raw ratio. The first
three combinations per workload are marked ★; ties retain matrix order.
Running again on the same date/commit replaces these results. Each JSON object
includes the workload, codec, batching and size limits, raw and wire bytes,
ratio, frame count, encode/decode microseconds per decimal MB (1,000,000 bytes),
separate Ruzstd decode cost for zstd, Git commit and Unix UTC timestamp.

A frame's deadline is anchored to its first chunk; a chunk arriving at or after
the deadline begins another frame. A zero window preserves individual reads,
except that reads exceeding the cap are split. Empty reads create no frames.
The wire estimate adds 19 OUT-header bytes, approximately 4 WebSocket bytes and
29 TLS bytes to each compressed payload. The cap applies before compression;
incompressible data can expand beyond the current protocol's payload cap. This
is an offline size experiment, not a production packetizer. A production codec
will need headroom or an uncompressed fallback.

Streaming encoders preserve history across frames and explicitly flush each
boundary. Zstd's final frame also includes the stream-end marker, charged to
wire bytes; DEFLATE uses raw sync-flushed blocks throughout. C zstd and miniz
must recover exactly the current frame before receiving the next. Ruzstd is
also fed incrementally, but its API withholds a history window until it can
release output; its entire output is verified at stream end. Its native Rust
runtime is only a stand-in for browser wasm cost, not a measurement of browser
latency. No future compressed bytes are fed to the primary decoder to make a
frame pass.

Timing covers codec calls and frame buffer allocation/copies, excludes framing,
JSON/file I/O and equality checks, and excludes initial persistent codec setup and the reusable Ruzstd output buffer.
These are single-pass measurements; use a release build and repeated runs on an
idle machine before choosing production settings. The default commands use a
debug build, so their timing values are diagnostic rather than deployment
estimates. Ratio and frame counts are deterministic; timings are not.

The offline stage neither uses the network nor chooses a production codec or
batching policy. Stage 2 below measures the real stack through the fault proxy.

## Stage 2: real-stack network benchmark

```sh
CARGO_NET_OFFLINE=true cargo run -p anila-e2e --bin anila-bench -- net
anila-bench net --profiles lan,good4g,poor4g,train --codecs none,deflate_stream,zstd_stream --out bench/results
```

Run from a checkout with the harness prerequisites installed: tmux, the Rust
build dependencies and the pinned wrangler used by `e2e/src/stack.rs`. This is
an integration benchmark, not an offline codec experiment. The command starts
one development worker (`dev_insecure: true`), private tmux server and host
service for the entire matrix. The service connects directly to the worker;
only the browser probe goes through the fault proxy. Probes send no cookie and
offer exactly the selected codec. Defaults cover all twelve combinations,
normally taking roughly five minutes after stack startup/builds; unusually slow
startup or timeouts can exceed ten minutes. No extra crates are required.

| Profile | Delay each direction | Bytes/second each direction | Scheduled outage |
|---|---:|---:|---|
| lan | 0 ms | unlimited | none |
| good4g | 25 ms | 2,500,000 | none |
| poor4g | 75 ms | 125,000 | none |
| train | 200 ms | 32,000 | 3 seconds every 30 seconds |

MB and KB here are decimal. The proxy uses an independent token bucket for
each direction of each TCP connection. Buckets start empty, accumulate at most
16 KiB, and reset when the rate changes. `set_bandwidth(None)` removes the
limit, including on existing connections; `Some(0)` holds writes until changed.
The existing latency is a delay per forwarded chunk (up to 16 KiB), so these
profiles approximate network conditions rather than implementing packet-level
propagation delay. The service leg is never shaped.

Each profile/codec gets fresh sessions and a fresh probe:

- **Echo p50/p95/max (milliseconds):** open `cat`, schedule 50 distinct
  eight-character markers 300 ms apart, followed by newlines. Measure from
  the first `type_str` attempt until `screen()` contains that marker. Samples
  overlap, so RTTs longer than 300 ms do not slow the schedule. When a dropout
  pauses input, retry until accepted; that waiting time is included. Percentiles
  use nearest rank (samples 25 and 48 of the sorted 50). These measure the
  probe's accumulated decoded output, not browser paint or display latency.
- **Throughput milliseconds and down bytes:** create/open a preset running
  `sh -c 'seq 1 20000'`; stop when the probe screen contains `20000`. The time
  includes session creation and attach. A private tmux server retains exited
  panes so a fast command's final screen can still be attached. Consequently,
  tmux may coalesce output or provide a final screen snapshot; this measures
  user-visible completion, not guaranteed delivery of all 20,000 lines.
  Down bytes are the proxy's downstream counter difference over that same
  interval, including WebSocket/control/acknowledgement responses that occur
  during it. They exclude TCP/IP headers, retransmissions and TLS (local
  wrangler uses plain HTTP/WebSocket).
- **Resume milliseconds:** open `sh -c 'while :; do date; sleep 0.05; done'`,
  wait for output, blackhole the browser connection for five seconds, restore,
  then wait for both an increased `resumes` count and additional decoded output.
  Replay counts as additional output. Probes use 500 ms pings, a 2,000 ms
  dead-link timeout and normal reconnect backoff. These timer overrides are
  recorded in JSON; the production 25-second timeout would not detect a
  five-second blackhole and therefore would not exercise resume.

The train schedule starts at profile entry and spans its codecs. Scheduled
faults are suspended during each explicit five-second resume experiment, so
that its outage duration is controlled. The clock keeps running during those
experiments. Short subsets may finish before a scheduled 30-second outage.
A blackhole holds data in order and applies backpressure; it does not discard
TCP packets. **TCP loss cannot be emulated by a TCP proxy.** Use an OS-level
network emulator for packet loss and actual TCP retransmission/congestion
measurements.

The command prints a Markdown table and writes one JSON object per completed
pair to `<out>/<UTC-date>-<short-commit>-net.jsonl`. Each line includes all
metrics, the Git commit, Unix UTC timestamp and timer overrides. Results are
flushed per pair; rerunning on the same date/commit replaces the file. Errors
stop the run with profile/codec context, preserving already completed lines.
Session, proxy and child-process owners clean up on exit. Echo input/output,
connection, open and recovery waits have 30-second bounds.

Validation without starting wrangler:

```sh
CARGO_NET_OFFLINE=true cargo test -p anila-e2e --lib net_arguments
CARGO_NET_OFFLINE=true cargo test -p anila-e2e --test proxy_bandwidth --test harness_proxy
CARGO_NET_OFFLINE=true cargo clippy -p anila-e2e --all-targets
```


## Decisions

**Shipped: 8 ms batching and `deflate_stream`.** Batching removes almost all
framing overhead (a recorded `ls -laR`: 1,765 frames at 1.48x raw unbatched,
7 frames at 1.00x with 16 ms). Streaming deflate cuts wire bytes 6-29x on
terminal output, is pure Rust on both ends (`miniz_oxide`), and every
sync-flushed piece decodes on arrival.

**Shipped later: `zstd_stream` as prefix frames.** A single long zstd
stream cannot be decoded on arrival in the browser: `ruzstd` 0.8 keeps the
last window of decoded bytes until the stream ends (a flushed 23-byte piece
yields nothing). The shipped scheme makes every piece a complete zstd frame,
compressed with the previous 32 KiB of output as a raw prefix; the browser
decodes it with `ruzstd` using that same history as a forced dictionary, so
each piece decodes fully on arrival. Measured at 16 ms / 32 KiB it beats
`deflate_stream` on build output (30,621 vs 32,097 wire bytes) and TUI
redraws (22,237 vs 26,095), and loses on tiny ticker lines (42,490 vs
37,229), so it is the preferred codec with deflate as the fallback. The C
zstd encoder is behind the `zstd-encode` feature, which only the service
enables; the browser build stays pure Rust.

**Stage 2 confirms the codec on shaped networks** (`bench/results/*-8d09b27-net.md`,
measured after fixing the proxy to apply latency as a parallel propagation
delay; the earlier `*-2438486-net.md` run serialized the delay per chunk and
is superseded): deflate sends 4.2x fewer bytes and finishes a 20,000-line
burst 3.6x faster on poor 4G and 4x faster on the train profile (1.2x on good
4G), with the same echo latency. Echo p95 is within RTT + 25 ms on every
profile (poor 4G: 170 ms at a 150 ms RTT). On the train profile a single
run's p95 can reach ~3.7 s when the profile's scheduled 3 s blackout lands in
the echo window; reruns show it follows whichever codec is measured at that
moment, not the codec itself.

**Shipped in v0.0.3: a quiet channel's chunk goes out at once (O1).**
The batcher used to hold every frame's first chunk for the whole 8 ms
window, so a keystroke echo always waited 8 ms in the service. Now a chunk
on a channel that sent nothing in the last window is sent immediately and
only what follows is batched (`service/src/coalesce.rs`). Measured with
`anila-bench net --profiles lan,good4g --codecs zstd_stream`, one run each
(`bench/results/2026-10-22-*-o1.jsonl`):

| Profile | Echo p50 | Echo p95 | Throughput | Down bytes |
|---|---:|---:|---:|---:|
| lan, before | 14.2 ms | 21.1 ms | 74.6 ms | 19,376 |
| lan, after | 3.6 ms | 6.6 ms | 48.2 ms | 17,296 |
| good4g, before | 67.2 ms | 68.7 ms | 110.0 ms | 20,483 |
| good4g, after | 55.6 ms | 74.6 ms | 92.4 ms | 21,055 |

| lan, v0.0.3 repeat | 6.2 ms | 9.5 ms | 54.8 ms | 20,310 |
| good4g, v0.0.3 repeat | 57.4 ms | 59.6 ms | 129.1 ms | 18,129 |

The median echo drops by 8 to 11 ms, about the window. The good4g p95 rise
in the first "after" run did not repeat on the finished v0.0.3 build.

**Measured, no change: zstd decode cost on a throttled browser (O4).**
`anila-bench phone-cpu` decodes the synthetic workloads, framed and encoded
as in production (8 ms, 32 KiB), with the real wasm decoder in headless
Chromium at 4x and 6x CPU throttling
(`bench/results/2026-10-22-6e67174-phone-cpu.*`). Per-MB cost and the
mean per piece come from timing whole runs; the browser clock has 0.1 ms
steps, so per-piece percentiles only bound the tail.

| At 6x throttling | none | deflate_stream | zstd_stream |
|---|---:|---:|---:|
| ticker (tiny pieces), µs per piece | 0.5 | 14.8 | 41.4 |
| build output, ms per MB | 0.6 | 18.2 | 51.0 |
| TUI redraws, ms per MB | 0.4 | 10.4 | 26.4 |

Every piece under 64 bytes decoded within 0.1 ms (p95). zstd costs 2.5 to
3 times deflate's CPU, which trips the second clause of the O4 rule, but
the absolute cost is what matters: 41 µs per keystroke echo and about
50 ms per MB of output on a throttled core, against zstd's smaller
downloads on build output and redraws. Sending tiny pieces uncompressed
would save 26 to 41 µs per keystroke (4x to 6x throttling) and needs a
new codec id, so the
codecs stay as they are.

## Next optimizations (reviewed 2026-10-22; O1-O3 shipped in v0.0.3, see `docs/optimizations.md`)

The byte format is close to done; what is left is around it. In order of
value:

1. **Let the host room hibernate when nobody watches (cost).** The service's
   10 s `heartbeat` wakes the room, and the 15 s hung-service alarm re-arms
   while any socket is open (`worker/src/room.rs`, `alarm` and
   `Hop::Heartbeat`). The service is always connected, so each room stays
   in memory around the clock and bills Durable Object duration per
   machine. Plan: the service sends `PING_TEXT`, which the auto-response
   answers without waking the room; the room reads the socket's
   auto-response timestamp (`getWebSocketAutoResponseTimestamp`, which
   workers-rs 0.8.7 does not bind yet) when a browser connects, and runs
   the alarm only while a browser is attached.
2. **Instant tab switches (UX).** Switching tabs closes the channel and
   re-attaches tmux, losing xterm scrollback; a tab on another machine
   drops the socket. Keep background tabs' channels open (up to
   `MAX_CHANNELS_PER_TAB`), hidden, and one socket per machine with open
   tabs. The flow window bounds what hidden tabs receive. Frontend only.
3. **Echo without the batching wait (latency).** `Coalescer::push` holds
   the first chunk for the whole window (8 ms). Send a chunk that follows a
   quiet period at once and batch only bursts. Service only, no protocol
   change.
4. **Measure zstd CPU on phones.** Every frame, even a one-byte echo, is
   compressed and decoded against a 32 KiB prefix. The benchmarks measured
   size, not phone CPU or battery; measure before changing anything (for
   example, sending tiny frames uncompressed).

Not worth it: varint headers (19 → ~8 bytes per OUT frame, about 10 % of a
tiny echo packet after TLS and TCP/IP, nothing on bulk output, costs a
protocol bump); binary acks (~70 bytes every 200 ms, only while output
flows); faster dead-link detection (the client already wakes on `online`
and `visibilitychange`).

## Why not gRPC (or protobuf)

The protocol is a custom one over a single WebSocket per socket pair:
binary frames with fixed big-endian headers for terminal bytes
(`proto/src/data.rs`), JSON text frames for control (`proto/src/control.rs`),
and plain HTTP JSON for accounts (`proto/src/api.rs`).

- Browsers cannot speak gRPC directly. gRPC-Web needs a translating proxy
  and has no client-to-server streaming, and a terminal needs both
  directions at once.
- The host room is a Durable Object using the WebSocket hibernation API.
  It routes a data frame by peeking at bytes 1..5 (`peek_tab`) without
  decoding it, and answers pings with an exact-text auto-response without
  waking. An RPC framework would hide both behind its own framing.
- The data frames are already minimal; protobuf would only shrink the
  JSON control frames (acks, a few tens of bytes) and would add a schema
  toolchain and size to the wasm bundles.

The equivalent that could matter is the transport, not the encoding:
QUIC-based WebTransport would survive switching between cell towers or
Wi-Fi without reconnecting, and avoids head-of-line blocking. Workers
cannot serve it as of this review, and byte-offset replay already makes
reconnects lossless.
