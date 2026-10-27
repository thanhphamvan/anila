//! Acceptance tests for `anila_worker::agent_sig` (task T5). Do not edit.

use anila_proto::sig::{http_sign_input, ws_sign_input};
use anila_worker::agent_sig::HostKeys;
use serde_json::Value;

const VECTOR: &str = include_str!("../../proto/vectors/agent_sig.json");

fn setup() -> (HostKeys, Value) {
    let v: Value = serde_json::from_str(VECTOR).unwrap();
    let json = format!(r#"{{"my-mac": "{}"}}"#, v["public_key"].as_str().unwrap());
    (HostKeys::parse(&json).unwrap(), v)
}

fn ws_input(v: &Value) -> String {
    let ws = &v["ws"];
    ws_sign_input(
        ws["host"].as_str().unwrap(),
        ws["ts"].as_i64().unwrap(),
        ws["nonce"].as_str().unwrap(),
    )
}

#[test]
fn verifies_ws_vector() {
    let (keys, v) = setup();
    assert!(keys.contains("my-mac"));
    assert!(keys.verify(
        "my-mac",
        ws_input(&v).as_bytes(),
        v["ws"]["sig"].as_str().unwrap()
    ));
}

#[test]
fn verifies_http_vector() {
    let (keys, v) = setup();
    let h = &v["http"];
    // sha256 of the empty body
    let input = http_sign_input(
        h["method"].as_str().unwrap(),
        h["path"].as_str().unwrap(),
        h["ts"].as_i64().unwrap(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    );
    assert!(keys.verify("my-mac", input.as_bytes(), h["sig"].as_str().unwrap()));
}

#[test]
fn rejects_tampering_and_strangers() {
    let (keys, v) = setup();
    let sig = v["ws"]["sig"].as_str().unwrap();
    let mut input = ws_input(&v).into_bytes();
    *input.last_mut().unwrap() ^= 1;
    assert!(!keys.verify("my-mac", &input, sig), "changed message");
    assert!(
        !keys.verify("other-mac", ws_input(&v).as_bytes(), sig),
        "unknown host"
    );
    assert!(!keys.contains("other-mac"));

    let mut bad = sig.to_string();
    bad.replace_range(0..1, if bad.starts_with('A') { "B" } else { "A" });
    assert!(
        !keys.verify("my-mac", ws_input(&v).as_bytes(), &bad),
        "changed signature"
    );
}

#[test]
fn malformed_signatures_are_false_not_panics() {
    let (keys, v) = setup();
    let msg = ws_input(&v);
    for sig in [
        "",
        "!!!",
        "AAAA",
        "yjxh0IKBSbZdbR9R6C3DXur7D0vt-kSNbWJsYSNS8Ib8NMOdmd4_3kO-Dj0s4hVJwBPWh3E9byHRxcmeI5j3AQ==",
    ] {
        assert!(!keys.verify("my-mac", msg.as_bytes(), sig), "{sig:?}");
    }
}

#[test]
fn parse_rules() {
    assert!(HostKeys::parse("{}").is_ok());
    assert!(!HostKeys::parse("{}").unwrap().contains("my-mac"));
    assert!(HostKeys::parse("[]").is_err());
    assert!(HostKeys::parse("not json").is_err());
    assert!(HostKeys::parse(r#"{"my-mac": "short"}"#).is_err());
    assert!(HostKeys::parse(r#"{"my-mac": 5}"#).is_err());
}

/// Task T33: one key passed directly (machines paired in v0.0.2 have their
/// key in the auth store, not in `HOST_KEYS`).
#[test]
fn verifies_with_a_single_key() {
    use anila_worker::agent_sig::verify_with_key;
    let (_, v) = setup();
    let key = v["public_key"].as_str().unwrap();
    let sig = v["ws"]["sig"].as_str().unwrap();
    let input = ws_input(&v);
    assert!(verify_with_key(key, input.as_bytes(), sig));
    assert!(!verify_with_key(key, b"other message", sig));
    assert!(!verify_with_key("not base64!", input.as_bytes(), sig));
    assert!(
        !verify_with_key(&key[..10], input.as_bytes(), sig),
        "short key"
    );
    assert!(
        !verify_with_key(key, input.as_bytes(), "AAAA"),
        "short signature"
    );
}
