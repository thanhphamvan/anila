//! Acceptance tests for `anila_service::keys` (task T1). Do not edit.

use std::os::unix::fs::PermissionsExt;

use anila_service::keys::ServiceKey;
use serde_json::Value;

const VECTOR: &str = include_str!("../../proto/vectors/agent_sig.json");

fn vector() -> Value {
    serde_json::from_str(VECTOR).unwrap()
}

fn seed(v: &Value) -> [u8; 32] {
    let hex = v["seed_hex"].as_str().unwrap();
    core::array::from_fn(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
}

#[test]
fn matches_shared_vector_public_key() {
    let v = vector();
    let key = ServiceKey::from_seed(seed(&v));
    assert_eq!(key.public_key_b64(), v["public_key"].as_str().unwrap());
}

#[test]
fn ws_signature_matches_vector() {
    let v = vector();
    let key = ServiceKey::from_seed(seed(&v));
    let ws = &v["ws"];
    let sig = key.sign_ws(
        ws["host"].as_str().unwrap(),
        ws["ts"].as_i64().unwrap(),
        ws["nonce"].as_str().unwrap(),
    );
    assert_eq!(sig, ws["sig"].as_str().unwrap());
}

#[test]
fn http_headers_match_vector() {
    let v = vector();
    let key = ServiceKey::from_seed(seed(&v));
    let h = &v["http"];
    let headers = key.signed_headers(
        h["host"].as_str().unwrap(),
        h["method"].as_str().unwrap(),
        h["path"].as_str().unwrap(),
        h["ts"].as_i64().unwrap(),
        h["body"].as_str().unwrap().as_bytes(),
    );
    assert_eq!(
        headers,
        [
            ("Term-Host", "my-mac".to_string()),
            ("Term-Ts", "1792585534".to_string()),
            ("Term-Sig", h["sig"].as_str().unwrap().to_string()),
        ]
    );
}

#[test]
fn generated_keys_differ() {
    assert_ne!(
        ServiceKey::generate().public_key_b64(),
        ServiceKey::generate().public_key_b64()
    );
}

#[test]
fn save_then_load_round_trips_with_private_permissions() {
    let dir = tempdir();
    let path = dir.join("nested").join("service.key");
    let saved = ServiceKey::save_new(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "key file must be readable only by its owner");
    let loaded = ServiceKey::load(&path).unwrap();
    assert_eq!(saved.public_key_b64(), loaded.public_key_b64());
}

#[test]
fn save_refuses_to_overwrite() {
    let dir = tempdir();
    let path = dir.join("service.key");
    ServiceKey::save_new(&path).unwrap();
    assert!(
        ServiceKey::save_new(&path).is_err(),
        "an existing key must never be replaced silently"
    );
}

#[test]
fn load_rejects_group_or_world_readable_file() {
    let dir = tempdir();
    let path = dir.join("service.key");
    ServiceKey::save_new(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = ServiceKey::load(&path)
        .err()
        .expect("0644 key must be refused");
    assert!(
        err.to_string().contains("0600"),
        "error should tell the user the expected mode: {err}"
    );
}

#[test]
fn load_rejects_garbage() {
    let dir = tempdir();
    let path = dir.join("service.key");
    std::fs::write(&path, "not a key\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(ServiceKey::load(&path).is_err());
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "anila-keys-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        ^ (std::thread::current().id().as_u64_stable())
}

trait StableId {
    fn as_u64_stable(&self) -> u64;
}

impl StableId for std::thread::ThreadId {
    fn as_u64_stable(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}
