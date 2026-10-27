//! Acceptance tests for `anila_worker::webpush` (task T19). Do not edit.
//! The RFC 8291 vector was checked independently by decrypting it.

use anila_worker::webpush::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use serde_json::Value;

const VECTOR: &str = include_str!("../../proto/vectors/webpush_rfc8291.json");

fn v() -> Value {
    serde_json::from_str(VECTOR).unwrap()
}

fn b(v: &Value, k: &str) -> Vec<u8> {
    let s = v[k].as_str().unwrap();
    B64.decode(s.trim_end_matches('=')).unwrap()
}

#[test]
fn encrypts_the_rfc_8291_vector_exactly() {
    let v = v();
    let body = encrypt(
        v["plaintext"].as_str().unwrap().as_bytes(),
        &b(&v, "ua_public"),
        &b(&v, "auth"),
        b(&v, "as_private").try_into().unwrap(),
        b(&v, "salt").try_into().unwrap(),
    )
    .unwrap();
    assert_eq!(B64.encode(&body), v["body"].as_str().unwrap());
}

#[test]
fn rejects_bad_subscription_keys() {
    let v = v();
    let as_private: [u8; 32] = b(&v, "as_private").try_into().unwrap();
    let salt: [u8; 16] = b(&v, "salt").try_into().unwrap();
    assert_eq!(
        encrypt(b"x", &[4; 10], &b(&v, "auth"), as_private, salt),
        Err(PushError::BadKey)
    );
    let mut not_on_curve = b(&v, "ua_public");
    not_on_curve[40] ^= 0xff;
    assert_eq!(
        encrypt(b"x", &not_on_curve, &b(&v, "auth"), as_private, salt),
        Err(PushError::BadKey)
    );
    assert_eq!(
        encrypt(b"x", &b(&v, "ua_public"), &[1; 3], as_private, salt),
        Err(PushError::BadKey)
    );
}

#[test]
fn audiences() {
    assert_eq!(
        audience("https://fcm.googleapis.com/fcm/send/abc").unwrap(),
        "https://fcm.googleapis.com"
    );
    assert_eq!(
        audience("https://web.push.apple.com/QGuQ").unwrap(),
        "https://web.push.apple.com"
    );
    assert_eq!(
        audience("http://127.0.0.1:9000/push/1").unwrap(),
        "http://127.0.0.1:9000"
    );
    assert_eq!(audience("not a url"), Err(PushError::BadEndpoint));
    assert_eq!(audience("ftp://x/y"), Err(PushError::BadEndpoint));
}

#[test]
fn vapid_header_verifies() {
    let private: [u8; 32] = core::array::from_fn(|i| i as u8 + 7);
    let public = vapid_public_key(private).unwrap();
    let public_bytes = B64.decode(&public).unwrap();
    assert_eq!(public_bytes.len(), 65);
    assert_eq!(public_bytes[0], 4);

    let header = vapid_header(
        "https://fcm.googleapis.com/fcm/send/abc",
        "mailto:me@example.com",
        private,
        1_800_000_000,
    )
    .unwrap();
    let rest = header.strip_prefix("vapid t=").expect("vapid scheme");
    let (jwt, k) = rest.split_once(", k=").expect("t and k");
    assert_eq!(k, public);

    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let head: Value = serde_json::from_slice(&B64.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(head["alg"], "ES256");
    let claims: Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["aud"], "https://fcm.googleapis.com");
    assert_eq!(claims["exp"], 1_800_000_000);
    assert_eq!(claims["sub"], "mailto:me@example.com");

    let sig = Signature::from_slice(&B64.decode(parts[2]).unwrap()).expect("raw 64-byte r||s");
    let key = VerifyingKey::from_sec1_bytes(&public_bytes).unwrap();
    key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
        .expect("JWT signature verifies");
}
