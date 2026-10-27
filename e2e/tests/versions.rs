//! F10: a service one protocol version behind still connects (the service
//! on a host machine can lag a worker deploy); other versions are refused
//! with an `unsupported_proto` error and close code 4400.
//! Run with `cargo test -p anila-e2e --test versions -- --nocapture`.

mod common;

use std::time::Duration;

use anila_proto::{Hop, PROTO, sig::ws_sign_input};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// Connects as the service with `proto`; returns the hop frames received
/// and the close code, if the room closed the socket.
async fn agent_with_proto(ws_base: &str, proto: u16) -> (Vec<Hop>, Option<u16>) {
    let (seed, _) = vector_key();
    let key = SigningKey::from_bytes(&seed);
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("{ws_base}/ws/agent?host={HOST}"))
        .await
        .unwrap();
    let mut got = Vec::new();
    let mut close = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, ws.next()).await {
        match msg {
            Message::Text(t) => {
                let hop: Hop = serde_json::from_str(&t).unwrap();
                if let Hop::Challenge { nonce } = &hop {
                    let ts = anila_e2e::util::unix_now();
                    let sig = B64.encode(
                        key.sign(ws_sign_input(HOST, ts, nonce).as_bytes())
                            .to_bytes(),
                    );
                    let auth = Hop::Auth {
                        proto,
                        host: HOST.into(),
                        ts,
                        sig,
                        codecs: vec![],
                        watch: false,
                    };
                    ws.send(Message::text(serde_json::to_string(&auth).unwrap()))
                        .await
                        .unwrap();
                }
                let welcomed = matches!(hop, Hop::Welcome { .. });
                got.push(hop);
                if welcomed {
                    break;
                }
            }
            Message::Close(frame) => {
                close = Some(frame.map_or(1005, |f| u16::from(f.code)));
                break;
            }
            _ => {}
        }
    }
    (got, close)
}

#[test]
fn protocol_versions() {
    const { assert!(PROTO >= 1) };
    let worker = start_worker(false);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (frames, close) = agent_with_proto(&worker.ws_base(), PROTO - 1).await;
        assert!(
            frames.iter().any(|h| matches!(h, Hop::Welcome { .. })),
            "proto n-1 is welcomed: {frames:?} {close:?}"
        );

        let (frames, close) = agent_with_proto(&worker.ws_base(), PROTO + 1).await;
        assert!(
            !frames.iter().any(|h| matches!(h, Hop::Welcome { .. })),
            "proto n+1 is not welcomed"
        );
        assert_eq!(
            close,
            Some(u16::from(CloseCode::Library(4400))),
            "refused with 4400; frames {frames:?}"
        );
        assert!(
            frames.iter().any(|h| matches!(
                h,
                Hop::Error {
                    code: anila_proto::ErrorCode::UnsupportedProto,
                    ..
                }
            )),
            "with an unsupported_proto error first: {frames:?}"
        );
    });
    eprintln!("\nversion suite passed");
}
