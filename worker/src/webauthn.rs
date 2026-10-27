//! Relying-party verification for passkeys (WebAuthn Level 3), written
//! against RustCrypto because webauthn-rs needs OpenSSL. Pure functions:
//! no I/O, no Workers types, so they test natively.
//!
//! Supported keys: ES256 (COSE alg -7, P-256) and EdDSA (alg -8, Ed25519).
//! Attestation statements are not verified: any `fmt` is accepted and
//! `attStmt` is ignored, because this relying party does not trust
//! attestation.

use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use ciborium::value::Value;
use p256::ecdsa::signature::Verifier;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// What the relying party expects of a ceremony.
pub struct Expect<'a> {
    /// The raw challenge bytes the server issued.
    pub challenge: &'a [u8],
    /// e.g. `https://term.example.com`
    pub origin: &'a str,
    /// e.g. `term.example.com`
    pub rp_id: &'a str,
}

/// Verified credential material to persist after registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCredential {
    pub id: Vec<u8>,
    /// The COSE_Key bytes exactly as they appear in the authenticator data.
    pub public_key_cose: Vec<u8>,
    /// -7 or -8
    pub alg: i64,
    pub sign_count: u32,
}

/// The first failed check in a passkey ceremony.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebauthnError {
    /// clientDataJSON is not valid JSON or lacks a field.
    BadClientData,
    /// `type` is not `webauthn.create` / `webauthn.get` as expected.
    WrongType,
    WrongChallenge,
    WrongOrigin,
    /// clientData `crossOrigin` is true.
    CrossOrigin,
    /// authenticatorData rpIdHash is not SHA-256(rp_id).
    WrongRpId,
    /// UP flag (0x01) clear.
    UserNotPresent,
    /// UV flag (0x04) clear.
    UserNotVerified,
    /// authenticatorData is truncated or malformed, or lacks attested
    /// credential data (AT flag 0x40) at registration.
    BadAuthData,
    /// attestationObject is not the expected CBOR map.
    BadAttestation,
    /// COSE key type or algorithm other than ES256 / EdDSA, or a malformed key.
    UnsupportedKey,
    BadSignature,
    /// The signature counter did not increase (possible cloned authenticator).
    CounterRegressed,
}

/// Verifies a `navigator.credentials.create()` response.
pub fn verify_registration(
    client_data_json: &[u8],
    attestation_object: &[u8],
    expect: &Expect,
) -> Result<NewCredential, WebauthnError> {
    check_client(client_data_json, "webauthn.create", expect)?;
    let object = decode_exact(attestation_object).ok_or(WebauthnError::BadAttestation)?;
    let map = object.as_map().ok_or(WebauthnError::BadAttestation)?;
    let field = |name: &str| {
        let mut values = map.iter().filter(|(k, _)| k.as_text() == Some(name));
        let value = &values.next()?.1;
        if values.next().is_some() {
            return None;
        }
        Some(value)
    };
    field("fmt")
        .and_then(Value::as_text)
        .ok_or(WebauthnError::BadAttestation)?;
    field("attStmt").ok_or(WebauthnError::BadAttestation)?;
    let data = field("authData")
        .and_then(Value::as_bytes)
        .ok_or(WebauthnError::BadAttestation)?;
    let (flags, sign_count) = check_auth(data, expect)?;
    if flags & 0x40 == 0 {
        return Err(WebauthnError::BadAuthData);
    }
    let length = data.get(53..55).ok_or(WebauthnError::BadAuthData)?;
    let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
    let end = 55 + length;
    let id = data
        .get(55..end)
        .ok_or(WebauthnError::BadAuthData)?
        .to_vec();
    let remaining = data.get(end..).ok_or(WebauthnError::BadAuthData)?;
    let mut reader = remaining;
    let value: Value =
        ciborium::from_reader(&mut reader).map_err(|_| WebauthnError::BadAuthData)?;
    let public_key_cose = remaining[..remaining.len() - reader.len()].to_vec();
    let key = parse_key(&value)?;
    Ok(NewCredential {
        id,
        public_key_cose,
        alg: key.alg(),
        sign_count,
    })
}

/// Verifies a `navigator.credentials.get()` response and returns the new
/// signature counter to store.
///
/// The signature covers `authenticator_data || SHA-256(client_data_json)`.
/// ES256 signatures are ASN.1 DER; EdDSA signatures are raw 64 bytes.
///
/// Counter rule: if both the stored and the new counter are 0, accept
/// (synced passkeys always report 0). Otherwise the new counter must be
/// strictly greater than the stored one.
pub fn verify_assertion(
    client_data_json: &[u8],
    authenticator_data: &[u8],
    signature: &[u8],
    public_key_cose: &[u8],
    stored_sign_count: u32,
    expect: &Expect,
) -> Result<u32, WebauthnError> {
    check_client(client_data_json, "webauthn.get", expect)?;
    let (_, count) = check_auth(authenticator_data, expect)?;
    let value = decode_exact(public_key_cose).ok_or(WebauthnError::UnsupportedKey)?;
    let key = parse_key(&value)?;
    let mut message = authenticator_data.to_vec();
    message.extend_from_slice(&Sha256::digest(client_data_json));
    let valid = match key {
        Key::Es256(key) => p256::ecdsa::Signature::from_der(signature)
            .is_ok_and(|sig| key.verify(&message, &sig).is_ok()),
        Key::Ed25519(key) => ed25519_dalek::Signature::from_slice(signature)
            .is_ok_and(|sig| key.verify_strict(&message, &sig).is_ok()),
    };
    if !valid {
        return Err(WebauthnError::BadSignature);
    }
    if (count != 0 || stored_sign_count != 0) && count <= stored_sign_count {
        return Err(WebauthnError::CounterRegressed);
    }
    Ok(count)
}

#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
    origin: String,
    #[serde(default, rename = "crossOrigin")]
    cross_origin: bool,
}

fn check_client(data: &[u8], kind: &str, expect: &Expect) -> Result<(), WebauthnError> {
    let client: ClientData =
        serde_json::from_slice(data).map_err(|_| WebauthnError::BadClientData)?;
    if client.kind != kind {
        return Err(WebauthnError::WrongType);
    }
    let challenge = URL_SAFE_NO_PAD
        .decode(&client.challenge)
        .or_else(|_| URL_SAFE.decode(&client.challenge))
        .map_err(|_| WebauthnError::WrongChallenge)?;
    if challenge != expect.challenge {
        return Err(WebauthnError::WrongChallenge);
    }
    if client.origin != expect.origin {
        return Err(WebauthnError::WrongOrigin);
    }
    if client.cross_origin {
        return Err(WebauthnError::CrossOrigin);
    }
    Ok(())
}

fn check_auth(data: &[u8], expect: &Expect) -> Result<(u8, u32), WebauthnError> {
    let header = data.get(..37).ok_or(WebauthnError::BadAuthData)?;
    if header[..32] != Sha256::digest(expect.rp_id.as_bytes())[..] {
        return Err(WebauthnError::WrongRpId);
    }
    let flags = header[32];
    if flags & 1 == 0 {
        return Err(WebauthnError::UserNotPresent);
    }
    if flags & 4 == 0 {
        return Err(WebauthnError::UserNotVerified);
    }
    let count = u32::from_be_bytes([header[33], header[34], header[35], header[36]]);
    Ok((flags, count))
}

fn decode_exact(bytes: &[u8]) -> Option<Value> {
    let mut reader = bytes;
    let value = ciborium::from_reader(&mut reader).ok()?;
    reader.is_empty().then_some(value)
}

enum Key {
    Es256(p256::ecdsa::VerifyingKey),
    Ed25519(ed25519_dalek::VerifyingKey),
}

impl Key {
    fn alg(&self) -> i64 {
        match self {
            Self::Es256(_) => -7,
            Self::Ed25519(_) => -8,
        }
    }
}

fn parse_key(value: &Value) -> Result<Key, WebauthnError> {
    let bad = WebauthnError::UnsupportedKey;
    let map = value.as_map().ok_or(bad)?;
    let field = |label: i64| {
        let mut values = map
            .iter()
            .filter(|(k, _)| k.as_integer().map(i128::from) == Some(i128::from(label)));
        let value = &values.next()?.1;
        if values.next().is_some() {
            return None;
        }
        Some(value)
    };
    let integer = |label| {
        field(label)
            .and_then(Value::as_integer)
            .map(i128::from)
            .ok_or(bad)
    };
    let bytes = |label| -> Result<[u8; 32], WebauthnError> {
        field(label)
            .and_then(Value::as_bytes)
            .ok_or(bad)?
            .as_slice()
            .try_into()
            .map_err(|_| bad)
    };
    match (integer(1)?, integer(3)?, integer(-1)?) {
        (2, -7, 1) => {
            let mut sec1 = [0u8; 65];
            sec1[0] = 4;
            sec1[1..33].copy_from_slice(&bytes(-2)?);
            sec1[33..].copy_from_slice(&bytes(-3)?);
            p256::ecdsa::VerifyingKey::from_sec1_bytes(&sec1)
                .map(Key::Es256)
                .map_err(|_| bad)
        }
        (1, -8, 6) => ed25519_dalek::VerifyingKey::from_bytes(&bytes(-2)?)
            .map(Key::Ed25519)
            .map_err(|_| bad),
        _ => Err(bad),
    }
}
