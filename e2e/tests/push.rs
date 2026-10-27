//! Push notifications end to end: a browser subscription pointing at a fake
//! push service, `anila-service notify` on the host, and the worker's
//! encrypted, VAPID-signed delivery, decrypted here with the subscription's
//! private key. Run with `cargo test -p anila-e2e --test push -- --nocapture`.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use aes_gcm::{Aes128Gcm, KeyInit, aead::Aead};
use anila_e2e::authn::SoftAuthenticator;
use anila_e2e::tmux::TmuxServer;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use hkdf::Hkdf;
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use p256::{PublicKey, SecretKey, elliptic_curve::sec1::ToEncodedPoint};
use serde_json::{Value, json};
use sha2::Sha256;

struct Delivery {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// A fake push service: records each POST and answers with `statuses` in turn.
fn fake_push_service(statuses: Vec<u16>) -> (u16, mpsc::Receiver<Delivery>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for status in statuses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let mut headers = Vec::new();
            let mut length = 0;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                let (k, v) = h.split_once(':').unwrap();
                if k.eq_ignore_ascii_case("content-length") {
                    length = v.trim().parse().unwrap();
                }
                headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            let _ = tx.send(Delivery {
                path,
                headers,
                body,
            });
        }
    });
    (port, rx)
}

/// RFC 8291 decryption with the subscription's private key.
fn decrypt(body: &[u8], ua: &SecretKey, auth: &[u8]) -> Vec<u8> {
    let salt = &body[..16];
    let rs = u32::from_be_bytes(body[16..20].try_into().unwrap());
    assert_eq!(rs, 4096);
    let idlen = body[20] as usize;
    let as_public = &body[21..21 + idlen];
    let ciphertext = &body[21 + idlen..];
    let ua_public = ua.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(
        ua.to_nonzero_scalar(),
        PublicKey::from_sec1_bytes(as_public).unwrap().as_affine(),
    );
    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(ua_public.as_bytes());
    info.extend_from_slice(as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes())
        .expand(&info, &mut ikm)
        .unwrap();
    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .unwrap();
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce).unwrap();
    let mut plain = Aes128Gcm::new(&cek.into())
        .decrypt(&nonce.into(), ciphertext)
        .expect("payload decrypts");
    assert_eq!(
        plain.pop(),
        Some(2),
        "single record ends with the 0x02 delimiter"
    );
    plain
}

#[test]
fn push_notifications() {
    let vapid_private: [u8; 32] = core::array::from_fn(|i| 200 - i as u8);
    let worker = start_worker_with(
        false,
        vec![
            ("VAPID_PRIVATE_KEY".into(), B64.encode(vapid_private)),
            ("VAPID_SUBJECT".into(), "mailto:e2e@example.com".into()),
        ],
    );
    let tmux = TmuxServer::new("e2e-push");
    let _service = start_service(&worker.http_base(), &tmux);
    let code = enroll_code(&worker.http_base(), &tmux);
    let mut key = SoftAuthenticator::new(5);
    let mut http = register(&worker, &code, &mut key);

    // The browser asks for the VAPID key and subscribes.
    let (status, k) = http.get("/api/push/key");
    assert_eq!(status, 200, "{k}");
    let vapid_public = k["key"].as_str().unwrap().to_string();

    let (port, deliveries) = fake_push_service(vec![201, 410]);
    let ua = SecretKey::from_slice(&[7u8; 32]).unwrap();
    let auth = [9u8; 16];
    let endpoint = format!("http://127.0.0.1:{port}/push/device-1");
    let (status, body) = http.post(
        "/api/push/subscribe",
        json!({ "endpoint": endpoint,
                "keys": { "p256dh": B64.encode(ua.public_key().to_encoded_point(false).as_bytes()),
                          "auth": B64.encode(auth) } }),
    );
    assert_eq!(status, 204, "subscribe: {body}");
    let (status, _) = http.post(
        "/api/push/subscribe",
        json!({ "endpoint": "ftp://nope", "keys": { "p256dh": "AA", "auth": "AA" } }),
    );
    assert_eq!(status, 400, "bad subscriptions are refused");

    // The host says Claude finished.
    let out = service_cli(
        &worker.http_base(),
        &tmux,
        &[
            "notify",
            "--title",
            "Claude finished",
            "--body",
            "Ready for your next prompt",
            "--session",
            "work",
        ],
    );
    assert!(out.contains("Sent to 1 device"), "notify output: {out}");

    let d = deliveries
        .recv_timeout(Duration::from_secs(10))
        .expect("the worker posted to the push service");
    assert_eq!(d.path, "/push/device-1");
    let header = |name: &str| {
        d.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(header("content-encoding"), "aes128gcm");
    assert_eq!(header("ttl"), "86400");
    let payload: Value = serde_json::from_slice(&decrypt(&d.body, &ua, &auth)).unwrap();
    assert_eq!(
        payload,
        json!({ "title": "Claude finished", "body": "Ready for your next prompt", "session": "work",
                "host": HOST, "host_name": HOST })
    );

    // VAPID: signed by the worker's key for this push service's origin.
    let vapid = header("authorization");
    let (jwt, k) = vapid
        .strip_prefix("vapid t=")
        .unwrap()
        .split_once(", k=")
        .unwrap();
    assert_eq!(k, vapid_public);
    let parts: Vec<&str> = jwt.split('.').collect();
    let claims: Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["aud"], format!("http://127.0.0.1:{port}"));
    assert_eq!(claims["sub"], "mailto:e2e@example.com");
    VerifyingKey::from_sec1_bytes(&B64.decode(k).unwrap())
        .unwrap()
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &Signature::from_slice(&B64.decode(parts[2]).unwrap()).unwrap(),
        )
        .expect("VAPID JWT verifies");

    // The push service says the subscription is gone: the worker forgets it.
    let out = service_cli(
        &worker.http_base(),
        &tmux,
        &["notify", "--title", "t", "--body", "b"],
    );
    assert!(
        out.contains("Sent to 0") || out.contains("No devices") || out.contains("removed"),
        "notify after 410: {out}"
    );
    deliveries
        .recv_timeout(Duration::from_secs(10))
        .expect("second delivery attempt");
    let out = service_cli(
        &worker.http_base(),
        &tmux,
        &["notify", "--title", "t", "--body", "b"],
    );
    assert!(
        out.contains("No devices subscribed"),
        "a 410 removes the subscription: {out}"
    );
    eprintln!("\npush suite passed");
}
