//! Acceptance test for O3 (`docs/optimizations.md`): the service heartbeats
//! only while the room says a browser watches, and otherwise sends the exact
//! `PING_TEXT` that the room's auto-response answers without waking.
//! Plays the room over a real websocket; needs no tmux.

use std::time::{Duration, Instant};

use anila_proto::{Hop, PROTO, control::PING_TEXT};
use anila_service::config::Config;
use anila_service::keys::ServiceKey;
use anila_service::link::{Timings, run};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_async};

type Ws = WebSocketStream<TcpStream>;

const BEAT: Duration = Duration::from_millis(400);

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::text(v.to_string())).await.unwrap();
}

/// Text frames from the service during `window`, answering each with
/// `pong` as the room (or its auto-response) would.
async fn collect(ws: &mut Ws, window: Duration) -> Vec<String> {
    let deadline = Instant::now() + window;
    let mut texts = Vec::new();
    while let Ok(Some(msg)) = timeout(
        deadline.saturating_duration_since(Instant::now()),
        ws.next(),
    )
    .await
    {
        if let Message::Text(t) = msg.unwrap() {
            send(ws, json!({ "t": "pong" })).await;
            texts.push(t.to_string());
        }
    }
    texts
}

fn is_heartbeat(text: &str) -> bool {
    serde_json::from_str::<Value>(text).is_ok_and(|v| v["t"] == "heartbeat")
}

#[test]
fn heartbeats_only_while_watched() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Config::parse(&format!(
            "host = \"test-host\"\nworker = \"http://127.0.0.1:{port}\"\n"
        ))
        .unwrap();
        let timings = Timings {
            heartbeat: BEAT,
            dead_after: Duration::from_secs(3),
            tick: Duration::from_millis(50),
            batch: Duration::from_millis(8),
            max_backoff: Duration::from_millis(500),
        };
        let (stop, stopped) = watch::channel(false);
        let service = tokio::task::spawn_local(run(
            config,
            ServiceKey::from_seed([3; 32]),
            timings,
            stopped,
        ));

        let (stream, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("service connects")
            .unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        send(&mut ws, json!({ "t": "challenge", "nonce": "n" })).await;
        let auth = match timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Message::Text(t) => t.to_string(),
            other => panic!("expected auth, got {other:?}"),
        };
        let Ok(Hop::Auth { watch, .. }) = serde_json::from_str::<Hop>(&auth) else {
            panic!("expected auth, got {auth}");
        };
        assert!(
            watch,
            "the service says it understands watch frames: {auth}"
        );
        send(&mut ws, json!({ "t": "welcome", "proto": PROTO })).await;

        // A room that never sends `watch` (an older room) gets heartbeats.
        let texts = collect(&mut ws, BEAT * 3).await;
        assert!(
            texts.iter().filter(|t| is_heartbeat(t)).count() >= 2,
            "heartbeats by default: {texts:?}"
        );

        // Unwatched: only the exact ping text, never a heartbeat.
        send(&mut ws, json!({ "t": "watch", "on": false })).await;
        collect(&mut ws, Duration::from_millis(100)).await;
        let texts = collect(&mut ws, BEAT * 4).await;
        assert!(
            !texts.iter().any(|t| is_heartbeat(t)),
            "no heartbeat while unwatched: {texts:?}"
        );
        assert!(
            texts.len() >= 2,
            "still pings for its own link check: {texts:?}"
        );
        assert!(
            texts.iter().all(|t| t == PING_TEXT),
            "exactly PING_TEXT: {texts:?}"
        );

        // Watched again: a heartbeat at once, well before the next tick.
        send(&mut ws, json!({ "t": "watch", "on": true })).await;
        let texts = collect(&mut ws, BEAT / 4).await;
        assert!(
            texts.iter().any(|t| is_heartbeat(t)),
            "heartbeat at once: {texts:?}"
        );
        let texts = collect(&mut ws, BEAT * 3).await;
        assert!(
            texts.iter().filter(|t| is_heartbeat(t)).count() >= 2,
            "keeps heartbeating while watched: {texts:?}"
        );
        assert!(!service.is_finished(), "the link stayed up throughout");

        stop.send(true).unwrap();
        timeout(Duration::from_secs(5), service)
            .await
            .expect("service stops")
            .unwrap()
            .unwrap();
    });
}
