//! Verifies the service's ed25519 signatures against `HOST_KEYS`.

use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};

/// Trusted ed25519 public keys indexed by host id.
pub struct HostKeys {
    keys: HashMap<String, VerifyingKey>,
}

impl HostKeys {
    /// `json` is the `HOST_KEYS` var: an object mapping host id to an
    /// unpadded base64url 32-byte ed25519 public key. `{}` is valid and
    /// empty. Any invalid key or non-object JSON is an error.
    pub fn parse(json: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("invalid HOST_KEYS JSON: {e}"))?;
        let object = value.as_object().ok_or("HOST_KEYS must be an object")?;
        let mut keys = HashMap::new();
        for (host, value) in object {
            let encoded = value
                .as_str()
                .ok_or_else(|| format!("host {host:?}: key must be a string"))?;
            let bytes = URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|e| format!("host {host:?}: invalid key encoding: {e}"))?;
            let bytes: [u8; 32] = bytes
                .try_into()
                .map_err(|_| format!("host {host:?}: key must be exactly 32 bytes"))?;
            let key = VerifyingKey::from_bytes(&bytes)
                .map_err(|e| format!("host {host:?}: invalid public key: {e}"))?;
            keys.insert(host.clone(), key);
        }
        Ok(Self { keys })
    }

    /// Whether a trusted key is configured for `host`.
    pub fn contains(&self, host: &str) -> bool {
        self.keys.contains_key(host)
    }

    /// True only if `host` is known and `sig_b64` (unpadded base64url, 64
    /// bytes) is a valid signature of `message` by that host's key. Never
    /// panics on malformed input.
    pub fn verify(&self, host: &str, message: &[u8], sig_b64: &str) -> bool {
        let Some(key) = self.keys.get(host) else {
            return false;
        };
        verify_signature(key, message, sig_b64)
    }
}

/// Verifies a signature with one unpadded base64url ed25519 public key.
/// Malformed keys and signatures are rejected without panicking.
pub fn verify_with_key(public_key_b64: &str, message: &[u8], sig_b64: &str) -> bool {
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(public_key_b64) else {
        return false;
    };
    let Ok(bytes) = <[u8; 32]>::try_from(bytes) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&bytes) else {
        return false;
    };
    verify_signature(&key, message, sig_b64)
}

fn verify_signature(key: &VerifyingKey, message: &[u8], sig_b64: &str) -> bool {
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(sig_b64) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&bytes) else {
        return false;
    };
    key.verify_strict(message, &signature).is_ok()
}
