//! O3 (`docs/optimizations.md`): the host room tells a `watch`-capable
//! service whether a browser is connected, never closes an unwatched
//! service for staying quiet, and catches a service that is hung when a
//! browser arrives within `WATCH_PROBE_SECS`. The test plays the service
//! against the real worker so it can choose exactly what to send.
//! Run with `cargo test -p anila-e2e --test watch -- --nocapture`.

mod common;

use std::time::{Duration, Instant};

use anila_proto::control::PING_TEXT;
use anila_proto::limits::{DEAD_AFTER_SECS, HUNG_AGENT_CHECK_SECS, WATCH_PROBE_SECS};
use anila_proto::{Hop, PROTO, sig::ws_sign_input};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn step(name: &str) {
    eprintln!("\n=== {name}");
}

/// What the room sent the fake service.
#[derive(Debug, PartialEq)]
enum Got {
    Watch(bool),
    Closed(u16),
    Nothing,
}

struct FakeService {
    ws: Ws,
}

impl FakeService {
    /// Connects and authenticates as the test host, offering `watch`.
    async fn connect(ws_base: &str, watch: bool) -> Self {
        let (seed, _) = vector_key();
        let key = SigningKey::from_bytes(&seed);
        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("{ws_base}/ws/agent?host={HOST}"))
                .await
                .unwrap();
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("the room answers")
                .expect("socket open")
                .unwrap();
            let Message::Text(t) = msg else { continue };
            match serde_json::from_str::<Hop>(&t).unwrap() {
                Hop::Challenge { nonce } => {
                    let ts = anila_e2e::util::unix_now();
                    let sig = B64.encode(
                        key.sign(ws_sign_input(HOST, ts, &nonce).as_bytes())
                            .to_bytes(),
                    );
                    let auth = Hop::Auth {
                        proto: PROTO,
                        host: HOST.into(),
                        ts,
                        sig,
                        codecs: vec![],
                        watch,
                    };
                    ws.send(Message::text(serde_json::to_string(&auth).unwrap()))
                        .await
                        .unwrap();
                }
                Hop::Welcome { .. } => return Self { ws },
                other => panic!("unexpected {other:?} before welcome"),
            }
        }
    }

    async fn send(&mut self, text: &str) {
        self.ws.send(Message::text(text.to_owned())).await.unwrap();
    }

    async fn heartbeat(&mut self) {
        self.send(&serde_json::to_string(&Hop::Heartbeat).unwrap())
            .await;
    }

    /// The next `watch` frame or close within `within`, skipping pongs and
    /// anything routed. Sends `PING_TEXT` every second meanwhile, like an
    /// unwatched service, and a heartbeat too when `beat` is set.
    async fn next(&mut self, within: Duration, beat: bool) -> Got {
        let deadline = Instant::now() + within;
        let mut ping = tokio::time::interval(Duration::from_secs(1));
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Got::Nothing;
            }
            tokio::select! {
                _ = ping.tick() => {
                    self.send(PING_TEXT).await;
                    if beat {
                        self.heartbeat().await;
                    }
                }
                msg = tokio::time::timeout(left, self.ws.next()) => match msg {
                    Err(_) => return Got::Nothing,
                    Ok(None) => return Got::Closed(1006),
                    Ok(Some(Err(_))) => return Got::Closed(1006),
                    Ok(Some(Ok(Message::Close(frame)))) => {
                        return Got::Closed(frame.map_or(1005, |f| u16::from(f.code)));
                    }
                    Ok(Some(Ok(Message::Text(t)))) => {
                        if let Ok(Hop::Watch { on }) = serde_json::from_str::<Hop>(&t) {
                            return Got::Watch(on);
                        }
                    }
                    Ok(Some(Ok(_))) => {}
                },
            }
        }
    }
}

async fn wait_online(p: &anila_e2e::probe::Probe, online: bool, within: Duration) {
    let deadline = Instant::now() + within;
    while p.host_online() != Some(online) {
        assert!(
            Instant::now() < deadline,
            "host_online == {online} within {within:?}; stats {:?}",
            p.stats()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn the_room_watches_only_while_a_browser_is_connected() {
    let worker = start_worker(true);
    let ws_base = worker.ws_base();
    let origin = worker.origin();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        step("a watch-capable service with no browser is told it is unwatched");
        let mut service = FakeService::connect(&ws_base, true).await;
        assert_eq!(
            service.next(Duration::from_secs(3), false).await,
            Got::Watch(false)
        );

        step("unwatched, it only pings, and the room never closes it for that");
        let quiet = Duration::from_secs(DEAD_AFTER_SECS + HUNG_AGENT_CHECK_SECS + 3);
        assert_eq!(service.next(quiet, false).await, Got::Nothing);

        step("a browser arrives: watch on, and heartbeats keep it online");
        let p = probe_host(&ws_base, &origin, None, 1, HOST).await;
        assert_eq!(
            service.next(Duration::from_secs(3), false).await,
            Got::Watch(true)
        );
        service.heartbeat().await;
        wait_online(&p, true, Duration::from_secs(5)).await;
        let past_probe = Duration::from_secs(WATCH_PROBE_SECS + 3);
        assert_eq!(service.next(past_probe, true).await, Got::Nothing);
        assert_eq!(p.host_online(), Some(true));

        step("the last browser leaves: watch off");
        p.shutdown().await;
        assert_eq!(
            service.next(Duration::from_secs(3), false).await,
            Got::Watch(false)
        );

        step("a browser arrives while the service is hung: offline within the probe");
        let p = probe_host(&ws_base, &origin, None, 2, HOST).await;
        let started = Instant::now();
        assert_eq!(
            service.next(Duration::from_secs(3), false).await,
            Got::Watch(true)
        );
        // Hung: no heartbeat. The room closes it with 4408 and says offline.
        let got = service.next(past_probe, false).await;
        assert_eq!(got, Got::Closed(4408), "closed as hung");
        wait_online(&p, false, Duration::from_secs(2)).await;
        assert!(
            started.elapsed() <= Duration::from_secs(WATCH_PROBE_SECS + 3),
            "noticed within the probe window: {:?}",
            started.elapsed()
        );

        step("a service without watch never gets watch frames and stays online");
        let mut old = FakeService::connect(&ws_base, false).await;
        old.heartbeat().await;
        wait_online(&p, true, Duration::from_secs(5)).await;
        assert_eq!(
            old.next(Duration::from_secs(WATCH_PROBE_SECS + 3), true)
                .await,
            Got::Nothing
        );
        p.shutdown().await;
        assert_eq!(old.next(Duration::from_secs(3), true).await, Got::Nothing);
        let p = probe_host(&ws_base, &origin, None, 3, HOST).await;
        assert_eq!(old.next(Duration::from_secs(3), true).await, Got::Nothing);
        assert_eq!(p.host_online(), Some(true));
        p.shutdown().await;
    });
    eprintln!("\nwatch suite passed");
}
