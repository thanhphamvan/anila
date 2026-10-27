//! Acceptance test for `anila_service::link` (task T6). Do not edit.
//!
//! The test plays the host room: it accepts the service's websocket,
//! checks the challenge signature, then drives a real tmux session through
//! list, open, input, a dropped link with exact resume, a silent room, and
//! shutdown.

// The handshake callback's error type comes from tungstenite.
#![allow(clippy::result_large_err)]

use std::process::Command;
use std::time::{Duration, Instant};

use anila_proto::sig::ws_sign_input;
use anila_proto::{Data, Hop, PROTO};
use anila_service::config::Config;
use anila_service::keys::ServiceKey;
use anila_service::link::{Timings, agent_url, run};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

type Ws = WebSocketStream<TcpStream>;

const VECTOR: &str = include_str!("../../proto/vectors/agent_sig.json");
const TAB: u32 = 7;

#[test]
fn agent_url_rules() {
    let host = anila_proto::HostId::new("my-mac").unwrap();
    assert_eq!(
        agent_url("https://term.example.com", &host).unwrap(),
        "wss://term.example.com/ws/agent?host=my-mac"
    );
    assert_eq!(
        agent_url("https://term.example.com/", &host).unwrap(),
        "wss://term.example.com/ws/agent?host=my-mac"
    );
    assert_eq!(
        agent_url("http://localhost:8787", &host).unwrap(),
        "ws://localhost:8787/ws/agent?host=my-mac"
    );
    assert!(agent_url("ftp://x", &host).is_err());
}

struct TmuxServer(String);

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .output();
    }
}

fn tmux(socket: &str, args: &[&str]) -> String {
    let out = Command::new("tmux")
        .args(["-L", socket])
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

enum Msg {
    Text(Value),
    Out {
        epoch: u32,
        off: u64,
        bytes: Vec<u8>,
    },
}

struct Room {
    heartbeats: usize,
}

impl Room {
    async fn accept(&self, listener: &TcpListener, secs: u64) -> (Ws, String) {
        let (stream, _) = timeout(Duration::from_secs(secs), listener.accept())
            .await
            .unwrap_or_else(|_| panic!("service did not connect within {secs}s"))
            .unwrap();
        let mut path = String::new();
        let ws = accept_hdr_async(stream, |req: &Request, resp: Response| {
            path = req.uri().to_string();
            Ok(resp)
        })
        .await
        .unwrap();
        (ws, path)
    }

    async fn handshake(&mut self, ws: &mut Ws, verifier: &VerifyingKey) {
        let nonce = format!("nonce-{}", Instant::now().elapsed().as_nanos());
        send(ws, json!({ "t": "challenge", "nonce": nonce })).await;
        let auth = match self.next(ws, 5).await {
            Msg::Text(v) => v,
            Msg::Out { .. } => panic!("expected auth, got binary"),
        };
        let hop: Hop = serde_json::from_value(auth.clone()).unwrap();
        let Hop::Auth {
            proto,
            host,
            ts,
            sig,
            codecs,
            ..
        } = hop
        else {
            panic!("expected auth, got {auth}")
        };
        assert_eq!(proto, PROTO);
        assert_eq!(host, "test-host");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!((now - ts).abs() <= 5, "auth ts must be the current time");
        assert!(codecs.contains(&anila_proto::Codec::None));
        let sig: [u8; 64] = URL_SAFE_NO_PAD.decode(sig).unwrap().try_into().unwrap();
        verifier
            .verify_strict(
                ws_sign_input(&host, ts, &nonce).as_bytes(),
                &Signature::from_bytes(&sig),
            )
            .expect("auth signature verifies");
        send(ws, json!({ "t": "welcome", "proto": PROTO })).await;
    }

    /// Next frame from the service. Heartbeats are counted and answered.
    async fn next(&mut self, ws: &mut Ws, secs: u64) -> Msg {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = timeout(left, ws.next())
                .await
                .unwrap_or_else(|_| panic!("no frame from the service within {secs}s"))
                .expect("socket open")
                .unwrap();
            match msg {
                Message::Text(t) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v["t"] == "heartbeat" {
                        self.heartbeats += 1;
                        send(ws, json!({ "t": "pong" })).await;
                        continue;
                    }
                    return Msg::Text(v);
                }
                Message::Binary(b) => match Data::decode(&b).expect("valid binary frame") {
                    Data::Out {
                        tab,
                        ch,
                        epoch,
                        off,
                        compressed,
                        bytes,
                    } => {
                        assert_eq!((tab, ch, compressed), (TAB, 1, false));
                        return Msg::Out {
                            epoch,
                            off,
                            bytes: bytes.to_vec(),
                        };
                    }
                    Data::In { .. } => panic!("the service never sends IN"),
                },
                _ => continue,
            }
        }
    }

    /// Reads until a text frame with `t == kind`, collecting OUT bytes.
    async fn until_text(&mut self, ws: &mut Ws, kind: &str, out: &mut Output) -> Value {
        loop {
            match self.next(ws, 5).await {
                Msg::Text(v) if v["t"] == kind => return v,
                Msg::Text(v) => out.texts.push(v),
                Msg::Out { epoch, off, bytes } => out.push(epoch, off, &bytes),
            }
        }
    }

    /// Reads until the collected output contains `needle`.
    async fn until_output(&mut self, ws: &mut Ws, needle: &str, out: &mut Output) {
        while !String::from_utf8_lossy(&out.bytes).contains(needle) {
            match self.next(ws, 5).await {
                Msg::Text(v) => out.texts.push(v),
                Msg::Out { epoch, off, bytes } => out.push(epoch, off, &bytes),
            }
        }
    }
}

#[derive(Default)]
struct Output {
    epoch: u32,
    next_off: u64,
    bytes: Vec<u8>,
    texts: Vec<Value>,
}

impl Output {
    fn push(&mut self, epoch: u32, off: u64, bytes: &[u8]) {
        assert_eq!(epoch, self.epoch, "OUT carries the channel's epoch");
        assert_eq!(off, self.next_off, "OUT offsets are contiguous");
        self.next_off += bytes.len() as u64;
        self.bytes.extend_from_slice(bytes);
    }
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::text(v.to_string())).await.unwrap();
}

async fn send_in(ws: &mut Ws, id: u32, bytes: &[u8]) {
    let frame = Data::In {
        tab: TAB,
        ch: 1,
        id,
        bytes,
    }
    .to_vec()
    .unwrap();
    ws.send(Message::binary(frame)).await.unwrap();
}

#[test]
fn link_end_to_end_with_fake_room() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed, skipping");
        return;
    }
    let socket = format!("anila-link-{}", std::process::id());
    let _server = TmuxServer(socket.clone());
    tmux(
        &socket,
        &["new-session", "-d", "-s", "work", "-c", "/tmp", "cat"],
    );

    let v: Value = serde_json::from_str(VECTOR).unwrap();
    let hex = v["seed_hex"].as_str().unwrap();
    let seed: [u8; 32] =
        core::array::from_fn(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap());
    let pk: [u8; 32] = URL_SAFE_NO_PAD
        .decode(v["public_key"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let verifier = VerifyingKey::from_bytes(&pk).unwrap();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Config::parse(&format!(
            "host = \"test-host\"\nworker = \"http://127.0.0.1:{port}\"\ntmux_socket = \"{socket}\"\n\
             [[preset]]\nid = \"shell\"\nlabel = \"Shell\"\ndir = \"/tmp\"\n"
        ))
        .unwrap();
        let timings = Timings {
            heartbeat: Duration::from_millis(200),
            dead_after: Duration::from_secs(1),
            tick: Duration::from_millis(50),
            batch: Duration::from_millis(8),
            max_backoff: Duration::from_millis(500),
        };
        let (stop, stopped) = watch::channel(false);
        let service = tokio::task::spawn_local(run(config, ServiceKey::from_seed(seed), timings, stopped));
        let mut room = Room { heartbeats: 0 };

        // Connect and authenticate.
        let (mut ws, path) = room.accept(&listener, 5).await;
        assert_eq!(path, "/ws/agent?host=test-host");
        room.handshake(&mut ws, &verifier).await;

        // List.
        send(&mut ws, json!({ "t": "list", "tab": TAB, "req": 1 })).await;
        let mut out = Output::default();
        let sessions = room.until_text(&mut ws, "sessions", &mut out).await;
        assert_eq!(sessions["req"], 1);
        assert!(sessions["items"].as_array().unwrap().iter().any(|i| i["name"] == "work" && i["cmd"] == "cat"));
        assert_eq!(sessions["presets"][0]["id"], "shell");

        // Open and type.
        send(&mut ws, json!({ "t": "open", "tab": TAB, "ch": 1, "req": 2, "session": "work", "cols": 80, "rows": 24 })).await;
        let opened = room.until_text(&mut ws, "opened", &mut out).await;
        out.epoch = opened["epoch"].as_u64().unwrap() as u32;
        // Wait for the tmux client to attach before typing, or the PTY's own
        // echo would satisfy the check below without tmux.
        let deadline = Instant::now() + Duration::from_secs(5);
        while tmux(&socket, &["list-clients", "-F", "#{client_width}x#{client_height}"]).trim() != "80x24" {
            assert!(Instant::now() < deadline, "tmux client attached at 80x24");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        send_in(&mut ws, 1, b"hello-link\r").await;
        room.until_output(&mut ws, "hello-link", &mut out).await;
        let ack = room.until_text(&mut ws, "in_ack", &mut out).await;
        assert_eq!(ack["id"], 1);

        // The link drops. The service reconnects; the browser resumes exactly.
        let rendered = out.next_off;
        ws.close(None).await.ok();
        drop(ws);
        let (mut ws, _) = room.accept(&listener, 5).await;
        room.handshake(&mut ws, &verifier).await;
        send(
            &mut ws,
            json!({ "t": "resume", "tab": TAB, "ch": 1, "session": "work", "epoch": out.epoch,
                    "from": rendered, "cols": 100, "rows": 30 }),
        )
        .await;
        let resumed = room.until_text(&mut ws, "resumed", &mut out).await;
        assert_eq!(resumed["from"], rendered);
        assert_eq!(resumed["in_acked"], 1);
        assert_eq!(resumed["epoch"], out.epoch);

        send_in(&mut ws, 1, b"duplicate-should-not-appear\r").await;
        send_in(&mut ws, 2, b"again\r").await;
        room.until_output(&mut ws, "again", &mut out).await;
        assert!(!String::from_utf8_lossy(&out.bytes).contains("duplicate-should-not-appear"));
        assert!(room.heartbeats >= 1, "the service sends heartbeats");

        // The room goes silent: the service must give up on this link and
        // reconnect within dead_after plus backoff.
        let silent_since = Instant::now();
        let (_ws3, _) = room.accept(&listener, 4).await;
        assert!(silent_since.elapsed() >= Duration::from_millis(700), "not long before dead_after");
        drop(ws);

        // Shutdown detaches every PTY and returns.
        stop.send(true).unwrap();
        timeout(Duration::from_secs(3), service).await.expect("run returns after shutdown").unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !tmux(&socket, &["list-clients"]).trim().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(tmux(&socket, &["list-clients"]).trim().is_empty(), "no tmux clients left after shutdown");
    });
}
