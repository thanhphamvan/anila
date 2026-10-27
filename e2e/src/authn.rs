//! Deterministic ES256 credentials for browser-free WebAuthn ceremonies.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ciborium::value::Value as Cbor;
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// A deterministic, discoverable software passkey.
pub struct SoftAuthenticator {
    key: SigningKey,
    id: Vec<u8>,
    user: Value,
    uv: bool,
    count: u32,
}

fn cbor(value: Cbor) -> Vec<u8> {
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes).expect("CBOR values serialize into memory");
    bytes
}

impl SoftAuthenticator {
    /// Deterministic P-256 key and credential id from `seed`.
    pub fn new(seed: u8) -> Self {
        // A small nonzero scalar is valid for every possible seed.
        let mut scalar = [0; 32];
        scalar[30..].copy_from_slice(&(u16::from(seed) + 1).to_be_bytes());
        Self {
            key: SigningKey::from_slice(&scalar).expect("small nonzero scalar"),
            id: Sha256::digest(scalar).to_vec(),
            user: Value::Null,
            uv: true,
            count: 0,
        }
    }

    /// Set user verification on future responses (default true).
    pub fn set_uv(&mut self, uv: bool) {
        self.uv = uv;
    }

    /// Return the credential's raw id.
    pub fn credential_id(&self) -> Vec<u8> {
        self.id.clone()
    }

    fn auth_data(&self, rp: &str, attested: bool) -> Vec<u8> {
        let mut data = Sha256::digest(rp.as_bytes()).to_vec();
        data.push(1 | if self.uv { 4 } else { 0 } | if attested { 64 } else { 0 });
        data.extend_from_slice(&self.count.to_be_bytes());
        data
    }

    /// Build a WebAuthn Level 3 registration response, remembering the user handle.
    pub fn register(&mut self, options: &Value, origin: &str) -> Value {
        self.user = options["user"]["id"].clone();
        self.count = 0;
        let client = json!({"type":"webauthn.create", "challenge":options["challenge"], "origin":origin, "crossOrigin":false}).to_string();
        let mut data = self.auth_data(options["rp"]["id"].as_str().unwrap_or_default(), true);
        data.extend_from_slice(&[0; 16]);
        data.extend_from_slice(&(self.id.len() as u16).to_be_bytes());
        data.extend_from_slice(&self.id);
        let point = self.key.verifying_key().to_encoded_point(false);
        let point = point.as_bytes();
        data.extend(cbor(Cbor::Map(vec![
            (1.into(), 2.into()),
            (3.into(), (-7).into()),
            ((-1).into(), 1.into()),
            ((-2).into(), Cbor::Bytes(point[1..33].to_vec())),
            ((-3).into(), Cbor::Bytes(point[33..65].to_vec())),
        ])));
        let attestation = cbor(Cbor::Map(vec![
            ("fmt".into(), "none".into()),
            ("attStmt".into(), Cbor::Map(vec![])),
            ("authData".into(), Cbor::Bytes(data)),
        ]));
        let id = URL_SAFE_NO_PAD.encode(&self.id);
        json!({"id":id,"rawId":id,"type":"public-key","response":{
            "clientDataJSON":URL_SAFE_NO_PAD.encode(client),
            "attestationObject":URL_SAFE_NO_PAD.encode(attestation)},"clientExtensionResults":{}})
    }

    /// Increment the counter and build a DER-signed WebAuthn assertion.
    pub fn login(&mut self, options: &Value, origin: &str) -> Value {
        self.count = self.count.saturating_add(1);
        let client = json!({"type":"webauthn.get","challenge":options["challenge"],"origin":origin,"crossOrigin":false}).to_string();
        let data = self.auth_data(options["rpId"].as_str().unwrap_or_default(), false);
        let mut signed = data.clone();
        signed.extend_from_slice(&Sha256::digest(client.as_bytes()));
        let signature: Signature = self.key.sign(&signed);
        let id = URL_SAFE_NO_PAD.encode(&self.id);
        json!({"id":id,"rawId":id,"type":"public-key","response":{
            "clientDataJSON":URL_SAFE_NO_PAD.encode(client),"authenticatorData":URL_SAFE_NO_PAD.encode(data),
            "signature":URL_SAFE_NO_PAD.encode(signature.to_der().as_bytes()),"userHandle":self.user},"clientExtensionResults":{}})
    }
}
