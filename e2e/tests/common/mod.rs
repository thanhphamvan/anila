//! Shared setup for the end-to-end suites: a worker, a private tmux server,
//! the service (optionally behind a fault proxy), a passkey, and probes.

#![allow(dead_code)]

use std::process::Command;
use std::time::Duration;

use anila_e2e::authn::SoftAuthenticator;
use anila_e2e::http::Http;
use anila_e2e::probe::{Probe, ProbeOpts};
use anila_e2e::stack::{Service, Worker, WorkerOpts};
use anila_e2e::tmux::TmuxServer;
use anila_e2e::util::{run_dir, workspace_root};
use anila_proto::client::ClientConfig;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

pub const HOST: &str = "test-host";

/// The shared test key from `proto/vectors/agent_sig.json`.
pub fn vector_key() -> ([u8; 32], String) {
    let v: Value =
        serde_json::from_str(include_str!("../../../proto/vectors/agent_sig.json")).unwrap();
    let hex = v["seed_hex"].as_str().unwrap();
    let seed = core::array::from_fn(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap());
    (seed, v["public_key"].as_str().unwrap().to_string())
}

pub fn start_worker(dev_insecure: bool) -> Worker {
    start_worker_with(dev_insecure, Vec::new())
}

pub fn start_worker_with(dev_insecure: bool, extra_vars: Vec<(String, String)>) -> Worker {
    let (_, pk) = vector_key();
    Worker::start(WorkerOpts {
        dev_insecure,
        host_keys: json!({ HOST: pk }).to_string(),
        default_host: HOST.into(),
        extra_vars,
        ..Default::default()
    })
    .expect("worker starts under wrangler dev")
}

/// Service config pointing at `worker_url` (the worker, or a proxy in front
/// of it), with presets "shell" and "ticker".
pub fn service_config(worker_url: &str, tmux: &TmuxServer) -> String {
    service_config_for(HOST, worker_url, tmux)
}

/// `service_config` for another machine id.
pub fn service_config_for(host: &str, worker_url: &str, tmux: &TmuxServer) -> String {
    format!(
        "host = \"{host}\"\nworker = \"{worker_url}\"\ntmux_socket = \"{}\"\ngrace_secs = 600\n\n\
         [[preset]]\nid = \"shell\"\nlabel = \"Shell\"\ndir = \"/tmp\"\n\n\
         [[preset]]\nid = \"ticker\"\nlabel = \"Ticker\"\ndir = \"/tmp\"\n\
         cmd = \"sh -c 'i=0; while :; do i=$((i+1)); echo tick $i; sleep 0.1; done'\"\n",
        tmux.socket()
    )
}

pub fn start_service(worker_url: &str, tmux: &TmuxServer) -> Service {
    let (seed, _) = vector_key();
    Service::start(&service_config(worker_url, tmux), seed).expect("service starts")
}

/// Runs `anila-service <args>` with its own copy of the config and key,
/// returning stdout. Panics with stderr on failure.
pub fn service_cli(worker_url: &str, tmux: &TmuxServer, args: &[&str]) -> String {
    let dir = run_dir("cli");
    let (seed, _) = vector_key();
    let key = dir.join("service.key");
    std::fs::write(&key, format!("{}\n", URL_SAFE_NO_PAD.encode(seed))).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = dir.join("service.toml");
    // key_file must come before the [[preset]] tables or TOML files it under them.
    std::fs::write(
        &config,
        format!(
            "key_file = \"{}\"\n{}",
            key.display(),
            service_config(worker_url, tmux)
        ),
    )
    .unwrap();
    let bin = workspace_root().join("target/debug/anila-service");
    let out = Command::new(bin)
        .arg("--config")
        .arg(&config)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "anila-service {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// `anila-service enroll` → the code it printed.
pub fn enroll_code(worker_url: &str, tmux: &TmuxServer) -> String {
    let out = service_cli(worker_url, tmux, &["enroll"]);
    let line = out
        .lines()
        .find(|l| l.starts_with("Enroll code: "))
        .expect("enroll prints the code");
    line.trim_start_matches("Enroll code: ").trim().to_string()
}

/// Registers `authn` with an enroll code; returns a signed-in client.
pub fn register(worker: &Worker, code: &str, authn: &mut SoftAuthenticator) -> Http {
    let (status, body, http) = try_register(worker, code, authn, None);
    assert_eq!(status, 201, "register/verify: {body}");
    assert!(http.cookie().is_some(), "registration signs in");
    http
}

/// Registers `authn` with a device or invite code, sending `account_name`
/// if given. Returns the register/verify status and body, and the client.
pub fn try_register(
    worker: &Worker,
    code: &str,
    authn: &mut SoftAuthenticator,
    account_name: Option<&str>,
) -> (u16, Value, Http) {
    let mut http = Http::new(&worker.http_base(), &worker.origin());
    let (status, body) = http.post("/api/passkeys/register/options", json!({ "code": code }));
    assert_eq!(status, 200, "register/options: {body}");
    let credential = authn.register(&body["publicKey"], &worker.origin());
    let mut verify = json!({ "flow": body["flow"], "label": "e2e", "credential": credential });
    if let Some(name) = account_name {
        verify["account_name"] = json!(name);
    }
    let (status, body) = http.post("/api/passkeys/register/verify", verify);
    (status, body, http)
}

/// A second machine's key seed (the first uses `vector_key`).
pub const SEED2: [u8; 32] = [42; 32];

/// Pairs a new machine through the real CLI: writes its key (from `seed`)
/// and an unpaired config (pointing at `tmux`) under a fresh
/// `XDG_CONFIG_HOME`, runs `anila-service pair <code> --name <name>`, and
/// returns stdout and the rewritten config, or stderr if it failed.
pub fn pair_machine(
    worker_url: &str,
    tmux: &TmuxServer,
    seed: [u8; 32],
    code: &str,
    name: &str,
) -> Result<(String, String), String> {
    use std::os::unix::fs::PermissionsExt;
    let xdg = run_dir("pair");
    let dir = xdg.join("anila");
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("service.key");
    std::fs::write(&key, format!("{}\n", URL_SAFE_NO_PAD.encode(seed))).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = dir.join("service.toml");
    std::fs::write(&config, service_config_for("unpaired", worker_url, tmux)).unwrap();
    let bin = workspace_root().join("target/debug/anila-service");
    let out = Command::new(bin)
        .env("XDG_CONFIG_HOME", &xdg)
        .args(["pair", code, "--name", name])
        .output()
        .unwrap();
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok((
        String::from_utf8_lossy(&out.stdout).to_string(),
        std::fs::read_to_string(&config).unwrap(),
    ))
}

/// The machine id a config's top-level `host = "<id>"` line names.
pub fn config_host(config: &str) -> String {
    config
        .lines()
        .find_map(|l| l.strip_prefix("host = \"")?.strip_suffix('"'))
        .expect("a host line")
        .to_string()
}

/// The HTTP status of a websocket upgrade (101 when it succeeds).
pub async fn ws_status(url: &str, origin: &str, cookie: Option<&str>) -> u16 {
    use tokio_tungstenite::tungstenite::{Error, client::IntoClientRequest};
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert("Origin", origin.parse().unwrap());
    if let Some(cookie) = cookie {
        req.headers_mut()
            .insert("Cookie", format!("__Host-term={cookie}").parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, response)) => {
            drop(ws);
            response.status().as_u16()
        }
        Err(Error::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("websocket upgrade to {url} failed: {other}"),
    }
}

/// A probe on one machine's room (`/ws/term?host=<id>`).
pub async fn probe_host(
    ws_base: &str,
    origin: &str,
    cookie: Option<String>,
    tab: u32,
    host: &str,
) -> Probe {
    Probe::start(ProbeOpts {
        url: format!("{ws_base}/ws/term?host={host}"),
        origin: origin.into(),
        cookie,
        tab,
        config: fast_client(),
        backoff_scale: 0.1,
    })
    .await
}

/// Signs in with an existing passkey; returns the HTTP status and client.
pub fn login(worker: &Worker, authn: &mut SoftAuthenticator) -> (u16, Http) {
    let mut http = Http::new(&worker.http_base(), &worker.origin());
    let (status, body) = http.post("/api/passkeys/login/options", json!({}));
    assert_eq!(status, 200, "login/options: {body}");
    let credential = authn.login(&body["publicKey"], &worker.origin());
    let (status, _) = http.post(
        "/api/passkeys/login/verify",
        json!({ "flow": body["flow"], "credential": credential }),
    );
    (status, http)
}

pub fn fast_client() -> ClientConfig {
    ClientConfig {
        ping_ms: 500,
        dead_ms: 2_000,
        ack_ms: 200,
        ack_bytes: 64 * 1024,
        // ANILA_CODEC=none runs the suites uncompressed; default is the
        // build's preference list (deflate_stream first).
        codecs: match std::env::var("ANILA_CODEC").as_deref() {
            Ok("none") => vec![anila_proto::Codec::None],
            _ => anila_proto::codec::SUPPORTED.to_vec(),
        },
    }
}

pub async fn probe(ws_base: &str, origin: &str, cookie: Option<String>, tab: u32) -> Probe {
    Probe::start(ProbeOpts {
        url: format!("{ws_base}/ws/term"),
        origin: origin.into(),
        cookie,
        tab,
        config: fast_client(),
        backoff_scale: 0.1,
    })
    .await
}

pub async fn wait_connected(p: &Probe, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while !(p.is_connected() && p.host_online() == Some(true)) {
        assert!(
            std::time::Instant::now() < deadline,
            "probe connected with the host online within {secs}s; stats {:?}",
            p.stats()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The `tick N` numbers in a screen, in order.
pub fn ticks(screen: &[u8]) -> Vec<u64> {
    let text = String::from_utf8_lossy(screen);
    text.split("tick ")
        .skip(1)
        .filter_map(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
        .collect()
}

/// No-loss check: reading the ticks in order, every number above the
/// highest seen so far is exactly one above it, so no line was lost across
/// reconnects. Lower numbers are allowed: tmux redraws the visible screen
/// when a client attaches or resizes. Returns how many redrawn ticks it saw.
pub fn assert_contiguous(screen: &[u8], what: &str) -> usize {
    let t = ticks(screen);
    assert!(t.len() >= 3, "{what}: too few ticks on screen ({t:?})");
    let mut max = t[0];
    let mut redrawn = 0;
    for &n in &t[1..] {
        if n > max {
            assert_eq!(
                n,
                max + 1,
                "{what}: ticks skip from {max} to {n}, output was lost (full: {t:?})"
            );
            max = n;
        } else {
            redrawn += 1;
        }
    }
    redrawn
}

/// Polls until the probe's view of the host matches `online`. Polling, not
/// events, so a status that arrives before the wait starts is not missed.
pub async fn wait_host(p: &Probe, online: bool, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while p.host_online() != Some(online) {
        assert!(
            std::time::Instant::now() < deadline,
            "host online={online} within {secs}s; stats {:?}",
            p.stats()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Polls until the probe has seen more than `before` resets.
pub async fn wait_reset(p: &Probe, before: u32, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while p.stats().resets <= before {
        assert!(
            std::time::Instant::now() < deadline,
            "a reset within {secs}s; stats {:?}",
            p.stats()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Subscribes for an event, then runs `trigger`, so the event cannot slip
/// past before the wait starts.
pub async fn expect_event(
    p: &Probe,
    secs: u64,
    pred: impl Fn(&anila_proto::client::Event) -> bool,
    trigger: impl FnOnce(),
) -> anila_proto::client::Event {
    let (event, _) = tokio::join!(p.wait_event(Duration::from_secs(secs), pred), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger();
    });
    event.expect("expected event")
}

/// Polls until the probe stopped with `code`.
pub async fn wait_stopped(p: &Probe, code: u16, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while !p.stats().stops.contains(&code) {
        assert!(
            std::time::Instant::now() < deadline,
            "stopped with {code} within {secs}s; stats {:?}",
            p.stats()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
