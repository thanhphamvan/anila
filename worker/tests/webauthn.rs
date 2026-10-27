use anila_worker::webauthn::{
    Expect, NewCredential, WebauthnError as Error, verify_assertion, verify_registration,
};
use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use ciborium::value::Value;
use p256::ecdsa::{SigningKey, signature::Signer};
use sha2::{Digest, Sha256};

const RP: &str = "term.example.com";
const ORIGIN: &str = "https://term.example.com";
const CHALLENGE: &[u8] = b"fixed challenge!";
const ID: &[u8] = b"credential id";

fn expect() -> Expect<'static> {
    Expect {
        challenge: CHALLENGE,
        origin: ORIGIN,
        rp_id: RP,
    }
}

fn cbor(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).unwrap();
    out
}

fn int(value: i64) -> Value {
    Value::Integer(value.into())
}

struct Authenticator {
    es: SigningKey,
    ed: ed25519_dalek::SigningKey,
    use_ed: bool,
    cose: Vec<u8>,
}

impl Authenticator {
    fn new(use_ed: bool) -> Self {
        let es = SigningKey::from_slice(&[7; 32]).unwrap();
        let ed = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let point = es.verifying_key().to_encoded_point(false);
        let fields = if use_ed {
            vec![
                (int(1), int(1)),
                (int(3), int(-8)),
                (int(-1), int(6)),
                (
                    int(-2),
                    Value::Bytes(ed.verifying_key().to_bytes().to_vec()),
                ),
            ]
        } else {
            vec![
                (int(1), int(2)),
                (int(3), int(-7)),
                (int(-1), int(1)),
                (int(-2), Value::Bytes(point.x().unwrap().to_vec())),
                (int(-3), Value::Bytes(point.y().unwrap().to_vec())),
            ]
        };
        Self {
            es,
            ed,
            use_ed,
            cose: cbor(&Value::Map(fields)),
        }
    }

    fn auth(&self, registration: bool, count: u32) -> Vec<u8> {
        let mut data = Sha256::digest(RP).to_vec();
        data.push(if registration { 0x45 } else { 5 });
        data.extend(count.to_be_bytes());
        if registration {
            data.extend([0; 16]);
            data.extend((ID.len() as u16).to_be_bytes());
            data.extend(ID);
            data.extend(&self.cose);
        }
        data
    }

    fn sign(&self, client: &[u8], data: &[u8]) -> Vec<u8> {
        let mut message = data.to_vec();
        message.extend(Sha256::digest(client));
        if self.use_ed {
            self.ed.sign(&message).to_bytes().to_vec()
        } else {
            let sig: p256::ecdsa::Signature = self.es.sign(&message);
            sig.to_der().as_bytes().to_vec()
        }
    }

    fn register(&self, client: &[u8], data: &[u8]) -> Result<NewCredential, Error> {
        verify_registration(client, &attestation(data, "none"), &expect())
    }

    fn assert(&self, client: &[u8], data: &[u8], stored: u32) -> Result<u32, Error> {
        verify_assertion(
            client,
            data,
            &self.sign(client, data),
            &self.cose,
            stored,
            &expect(),
        )
    }
}

fn client(kind: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type": kind, "challenge": URL_SAFE_NO_PAD.encode(CHALLENGE), "origin": ORIGIN,
        "unknown": "ignored"
    }))
    .unwrap()
}

fn changed_client(kind: &str, field: &str, value: serde_json::Value) -> Vec<u8> {
    let mut data: serde_json::Value = serde_json::from_slice(&client(kind)).unwrap();
    data[field] = value;
    serde_json::to_vec(&data).unwrap()
}

fn attestation(data: &[u8], fmt: &str) -> Vec<u8> {
    cbor(&Value::Map(vec![
        (Value::Text("fmt".into()), Value::Text(fmt.into())),
        (Value::Text("attStmt".into()), Value::Text("junk".into())),
        (Value::Text("authData".into()), Value::Bytes(data.to_vec())),
    ]))
}

fn round_trip(ed: bool) {
    let a = Authenticator::new(ed);
    let credential = a
        .register(&client("webauthn.create"), &a.auth(true, 1))
        .unwrap();
    assert_eq!(credential.id, ID);
    assert_eq!(credential.alg, if ed { -8 } else { -7 });
    assert_eq!(credential.public_key_cose, a.cose);
    assert_eq!(credential.sign_count, 1);
    let data = a.auth(false, 2);
    let client = client("webauthn.get");
    assert_eq!(
        verify_assertion(
            &client,
            &data,
            &a.sign(&client, &data),
            &credential.public_key_cose,
            credential.sign_count,
            &expect()
        ),
        Ok(2)
    );
}

#[test]
fn es256_round_trip() {
    round_trip(false);
}
#[test]
fn eddsa_round_trip() {
    round_trip(true);
}

fn client_failure(field: &str, value: serde_json::Value, error: Error) {
    let a = Authenticator::new(false);
    assert_eq!(
        a.register(
            &changed_client("webauthn.create", field, value.clone()),
            &a.auth(true, 1)
        ),
        Err(error)
    );
    assert_eq!(
        a.assert(
            &changed_client("webauthn.get", field, value),
            &a.auth(false, 2),
            1
        ),
        Err(error)
    );
}

#[test]
fn wrong_challenge() {
    client_failure("challenge", "bad!".into(), Error::WrongChallenge);
    client_failure(
        "challenge",
        URL_SAFE_NO_PAD.encode(b"other").into(),
        Error::WrongChallenge,
    );
}
#[test]
fn wrong_origin() {
    client_failure(
        "origin",
        "https://other.example.com".into(),
        Error::WrongOrigin,
    );
}
#[test]
fn cross_origin() {
    client_failure("crossOrigin", true.into(), Error::CrossOrigin);
}
#[test]
fn wrong_type() {
    let a = Authenticator::new(false);
    assert_eq!(
        a.register(&client("webauthn.get"), &a.auth(true, 1)),
        Err(Error::WrongType)
    );
    assert_eq!(
        a.assert(&client("webauthn.create"), &a.auth(false, 2), 1),
        Err(Error::WrongType)
    );
}
#[test]
fn bad_client_data() {
    let a = Authenticator::new(false);
    for bytes in [b"invalid".as_slice(), b"{}", b"\xff"] {
        assert_eq!(a.register(bytes, &[]), Err(Error::BadClientData));
        assert_eq!(a.assert(bytes, &[], 0), Err(Error::BadClientData));
    }
    client_failure("crossOrigin", "true".into(), Error::BadClientData);
}

fn auth_failure(index: usize, mask: u8, error: Error) {
    let a = Authenticator::new(false);
    let mut registration = a.auth(true, 1);
    registration[index] ^= mask;
    assert_eq!(
        a.register(&client("webauthn.create"), &registration),
        Err(error)
    );
    let mut assertion = a.auth(false, 2);
    assertion[index] ^= mask;
    assert_eq!(a.assert(&client("webauthn.get"), &assertion, 1), Err(error));
}
#[test]
fn wrong_rp_id() {
    auth_failure(0, 1, Error::WrongRpId);
}
#[test]
fn up_clear() {
    auth_failure(32, 1, Error::UserNotPresent);
}
#[test]
fn uv_clear() {
    auth_failure(32, 4, Error::UserNotVerified);
}
#[test]
fn tampered_auth_data() {
    for ed in [false, true] {
        let a = Authenticator::new(ed);
        let client = client("webauthn.get");
        let mut data = a.auth(false, 2);
        let sig = a.sign(&client, &data);
        data[36] ^= 1;
        assert_eq!(
            verify_assertion(&client, &data, &sig, &a.cose, 1, &expect()),
            Err(Error::BadSignature)
        );
    }
}
#[test]
fn different_signing_key() {
    for ed in [false, true] {
        let mut a = Authenticator::new(ed);
        a.es = SigningKey::from_slice(&[8; 32]).unwrap();
        a.ed = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        assert_eq!(
            a.assert(&client("webauthn.get"), &a.auth(false, 2), 1),
            Err(Error::BadSignature)
        );
    }
}
#[test]
fn truncated_auth_data() {
    let a = Authenticator::new(false);
    let data = a.auth(true, 1);
    for end in 0..data.len() {
        assert_eq!(
            a.register(&client("webauthn.create"), &data[..end]),
            Err(Error::BadAuthData)
        );
    }
    for end in 0..37 {
        assert_eq!(
            a.assert(&client("webauthn.get"), &data[..end], 0),
            Err(Error::BadAuthData)
        );
    }
}
#[test]
fn missing_at_flag() {
    let a = Authenticator::new(false);
    assert_eq!(
        a.register(&client("webauthn.create"), &a.auth(false, 1)),
        Err(Error::BadAuthData)
    );
}
#[test]
fn malformed_attestation() {
    for data in [vec![0xff], cbor(&Value::Map(vec![])), cbor(&int(1))] {
        assert_eq!(
            verify_registration(&client("webauthn.create"), &data, &expect()),
            Err(Error::BadAttestation)
        );
    }
}
#[test]
fn unsupported_key() {
    let mut a = Authenticator::new(false);
    let mut key: Value = ciborium::from_reader(a.cose.as_slice()).unwrap();
    for (k, v) in key.as_map_mut().unwrap() {
        if *k == int(3) {
            *v = int(-257);
        }
    }
    a.cose = cbor(&key);
    assert_eq!(
        a.register(&client("webauthn.create"), &a.auth(true, 1)),
        Err(Error::UnsupportedKey)
    );
    assert_eq!(
        a.assert(&client("webauthn.get"), &a.auth(false, 2), 1),
        Err(Error::UnsupportedKey)
    );
}
#[test]
fn counters() {
    for ed in [false, true] {
        let a = Authenticator::new(ed);
        for (stored, new, result) in [
            (0, 0, Ok(0)),
            (5, 6, Ok(6)),
            (5, 5, Err(Error::CounterRegressed)),
            (5, 0, Err(Error::CounterRegressed)),
        ] {
            assert_eq!(
                a.assert(&client("webauthn.get"), &a.auth(false, new), stored),
                result
            );
        }
    }
}
#[test]
fn packed_attestation_ignored() {
    let a = Authenticator::new(false);
    assert!(
        verify_registration(
            &client("webauthn.create"),
            &attestation(&a.auth(true, 1), "packed"),
            &expect()
        )
        .is_ok()
    );
}
#[test]
fn challenge_padding() {
    let a = Authenticator::new(false);
    for encoded in [
        URL_SAFE.encode(CHALLENGE),
        URL_SAFE_NO_PAD.encode(CHALLENGE),
    ] {
        assert!(
            a.register(
                &changed_client("webauthn.create", "challenge", encoded.clone().into()),
                &a.auth(true, 1)
            )
            .is_ok()
        );
        assert_eq!(
            a.assert(
                &changed_client("webauthn.get", "challenge", encoded.into()),
                &a.auth(false, 2),
                1
            ),
            Ok(2)
        );
    }
}
#[test]
fn extensions_do_not_change_cose_bytes() {
    let a = Authenticator::new(false);
    let mut data = a.auth(true, 1);
    data[32] |= 0x80;
    data.extend(cbor(&Value::Map(vec![(
        Value::Text("example".into()),
        Value::Bool(true),
    )])));
    assert_eq!(
        a.register(&client("webauthn.create"), &data)
            .unwrap()
            .public_key_cose,
        a.cose
    );
}
#[test]
fn random_inputs_never_panic() {
    let a = Authenticator::new(false);
    let create = client("webauthn.create");
    let get = client("webauthn.get");
    let valid = a.auth(false, 2);
    let mut seed = 0x12345678u32;
    for len in 0..400 {
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let _ = verify_registration(&bytes, &bytes, &expect());
        let _ = verify_registration(&create, &bytes, &expect());
        let _ = verify_registration(&create, &attestation(&bytes, "none"), &expect());
        let _ = verify_assertion(&bytes, &bytes, &bytes, &bytes, 0, &expect());
        let _ = verify_assertion(&get, &bytes, &bytes, &a.cose, 0, &expect());
        let _ = verify_assertion(&get, &valid, &bytes, &bytes, 0, &expect());
        let _ = verify_assertion(&get, &valid, &bytes, &a.cose, 0, &expect());
    }
}

#[test]
fn first_failure_wins() {
    let a = Authenticator::new(false);
    let mut data = a.auth(true, 1);
    data[0] ^= 1;
    data[32] = 0;
    assert_eq!(
        a.register(&client("webauthn.create"), &data[..37]),
        Err(Error::WrongRpId)
    );
    data[0] ^= 1;
    assert_eq!(
        a.register(&client("webauthn.create"), &data[..37]),
        Err(Error::UserNotPresent)
    );
    data[32] = 1;
    assert_eq!(
        a.register(&client("webauthn.create"), &data[..37]),
        Err(Error::UserNotVerified)
    );
    data[32] = 5;
    assert_eq!(
        a.register(&client("webauthn.create"), &data[..37]),
        Err(Error::BadAuthData)
    );
    assert_eq!(
        verify_registration(&client("webauthn.get"), &[], &expect()),
        Err(Error::WrongType)
    );
    let get = client("webauthn.get");
    let data = a.auth(false, 0);
    assert_eq!(
        verify_assertion(&get, &data, &[], &[], 5, &expect()),
        Err(Error::UnsupportedKey)
    );
    assert_eq!(
        verify_assertion(&get, &data, &[], &a.cose, 5, &expect()),
        Err(Error::BadSignature)
    );
}

#[test]
fn malformed_keys_and_signatures() {
    for ed in [false, true] {
        let mut a = Authenticator::new(ed);
        let get = client("webauthn.get");
        let data = a.auth(false, 2);
        for signature in [vec![], vec![0; 64], vec![0x30, 0xff]] {
            assert_eq!(
                verify_assertion(&get, &data, &signature, &a.cose, 1, &expect()),
                Err(Error::BadSignature)
            );
        }
        let mut key: Value = ciborium::from_reader(a.cose.as_slice()).unwrap();
        for (k, v) in key.as_map_mut().unwrap() {
            if *k == int(-2) {
                *v = Value::Bytes(vec![0; 31]);
            }
        }
        a.cose = cbor(&key);
        assert_eq!(
            a.register(&client("webauthn.create"), &a.auth(true, 1)),
            Err(Error::UnsupportedKey)
        );
        assert_eq!(a.assert(&get, &data, 1), Err(Error::UnsupportedKey));
    }
}
