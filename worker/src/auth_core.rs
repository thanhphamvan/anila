//! Pure helpers for passkey sign-in and sessions (contract C1): cookies,
//! session ids, enroll codes, CSRF checks, and the WebAuthn Level 3 JSON
//! the browser exchanges with the worker. No I/O and no Workers types, so
//! everything here tests natively; randomness is passed in as bytes.

use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The session cookie's name. The `__Host-` prefix forces Secure, Path=/
/// and no Domain.
pub const COOKIE_NAME: &str = "__Host-term";

/// The public edition accepts passkey-only stores. Reject newer schemas
/// before initialization can rewrite their version or authentication data.
pub fn store_schema_supported(schema: Option<&str>) -> bool {
    matches!(schema, None | Some("1" | "2"))
}

/// A new session id: `value` goes in the cookie, `hash` is what the auth
/// store keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sid {
    /// Unpadded base64url of 32 random bytes.
    pub value: String,
    /// `hash_sid(&value)`.
    pub hash: String,
}

/// Encodes random bytes as a session id and hashes its cookie value.
pub fn sid_from_bytes(bytes: [u8; 32]) -> Sid {
    let value = URL_SAFE_NO_PAD.encode(bytes);
    Sid {
        hash: hash_sid(&value),
        value,
    }
}

/// Lowercase hex SHA-256 of the cookie value string.
pub fn hash_sid(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

/// `__Host-term=<value>; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=<max_age_secs>`
pub fn set_cookie(value: &str, max_age_secs: u64) -> String {
    format!(
        "{COOKIE_NAME}={value}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={max_age_secs}"
    )
}

/// The same cookie with an empty value and `Max-Age=0`.
pub fn clear_cookie() -> String {
    set_cookie("", 0)
}

/// The session cookie's value from a `Cookie` request header, if present
/// and non-empty. Matches the name exactly.
pub fn read_cookie(header: Option<&str>) -> Option<String> {
    header?.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == COOKIE_NAME && !value.is_empty()).then(|| value.to_owned())
    })
}

/// Crockford base32 alphabet used for enroll codes.
pub const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 40 random bits as 8 Crockford base32 characters, most significant bits
/// first, shown as `XXXX-XXXX`.
pub fn enroll_code_from_bytes(bytes: [u8; 5]) -> String {
    let bits = bytes
        .into_iter()
        .fold(0u64, |bits, byte| (bits << 8) | u64::from(byte));
    let mut code = String::with_capacity(9);
    for group in 0..8 {
        if group == 4 {
            code.push('-');
        }
        code.push(char::from(
            CODE_ALPHABET[((bits >> (35 - group * 5)) & 31) as usize],
        ));
    }
    code
}

/// What the user typed, normalized for lookup: uppercase, spaces and dashes
/// removed, `O` read as `0`, `I` and `L` read as `1`. `None` unless exactly
/// 8 characters from `CODE_ALPHABET` remain.
pub fn normalize_enroll_code(input: &str) -> Option<String> {
    let mut code = String::with_capacity(8);
    for byte in input.bytes() {
        let byte = match byte.to_ascii_uppercase() {
            b' ' | b'-' => continue,
            b'O' => b'0',
            b'I' | b'L' => b'1',
            byte => byte,
        };
        if !CODE_ALPHABET.contains(&byte) || code.len() == 8 {
            return None;
        }
        code.push(char::from(byte));
    }
    (code.len() == 8).then_some(code)
}

/// Lowercase hex SHA-256 of a normalized code; the store keeps only this.
pub fn hash_code(normalized: &str) -> String {
    hash_sid(normalized)
}

/// CSRF rule for API calls: every method other than GET and HEAD needs a
/// `Content-Type` of `application/json` (parameters allowed) and an
/// `Origin` equal to `expected_origin`.
pub fn check_mutation(
    method: &str,
    content_type: Option<&str>,
    origin: Option<&str>,
    expected_origin: &str,
) -> Result<(), &'static str> {
    if matches!(method, "GET" | "HEAD") {
        return Ok(());
    }
    let media_type = content_type.map(|value| {
        value
            .split_once(';')
            .map_or(value, |(media_type, _)| media_type)
            .trim()
    });
    if !media_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
        return Err("Content-Type must be application/json");
    }
    if origin != Some(expected_origin) {
        return Err("Origin must match expected origin");
    }
    Ok(())
}

/// `PublicKeyCredentialCreationOptionsJSON`: EdDSA then ES256, resident key
/// and user verification required, attestation "none", 60 s timeout.
/// Binary fields are unpadded base64url.
pub fn creation_options(
    challenge: &[u8],
    rp_id: &str,
    rp_name: &str,
    user_id: &[u8],
    user_name: &str,
    exclude: &[Vec<u8>],
) -> Value {
    let exclude: Vec<Value> = exclude
        .iter()
        .map(|id| {
            json!({
                "type": "public-key", "id": URL_SAFE_NO_PAD.encode(id)
            })
        })
        .collect();
    json!({
        "challenge": URL_SAFE_NO_PAD.encode(challenge),
        "rp": { "id": rp_id, "name": rp_name },
        "user": { "id": URL_SAFE_NO_PAD.encode(user_id), "name": user_name, "displayName": user_name },
        "pubKeyCredParams": [{ "type": "public-key", "alg": -8 }, { "type": "public-key", "alg": -7 }],
        "timeout": 60000,
        "authenticatorSelection": { "residentKey": "required", "requireResidentKey": true, "userVerification": "required" },
        "attestation": "none",
        "excludeCredentials": exclude
    })
}

/// `PublicKeyCredentialRequestOptionsJSON` with an empty allow list, so the
/// authenticator offers its discoverable passkey.
pub fn request_options(challenge: &[u8], rp_id: &str) -> Value {
    json!({
        "challenge": URL_SAFE_NO_PAD.encode(challenge),
        "rpId": rp_id,
        "timeout": 60000,
        "userVerification": "required",
        "allowCredentials": []
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationResponse {
    pub id: Vec<u8>,
    pub client_data_json: Vec<u8>,
    pub attestation_object: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticationResponse {
    pub id: Vec<u8>,
    pub client_data_json: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    pub signature: Vec<u8>,
    pub user_handle: Option<Vec<u8>>,
}

/// Parses `PublicKeyCredential.toJSON()` from `create()`: `id`, `rawId`
/// (must decode to the same bytes), `type` = `public-key`, and
/// `response.clientDataJSON` / `response.attestationObject`, all unpadded
/// base64url (padded input is accepted too).
pub fn parse_registration(v: &Value) -> Result<RegistrationResponse, String> {
    let id = parse_id(v)?;
    let response = parse_response(v)?;
    Ok(RegistrationResponse {
        id,
        client_data_json: decode_field(response, "clientDataJSON")?,
        attestation_object: decode_field(response, "attestationObject")?,
    })
}

/// Parses `PublicKeyCredential.toJSON()` from `get()`: as above with
/// `response.clientDataJSON`, `authenticatorData`, `signature`, and an
/// optional `userHandle`.
pub fn parse_authentication(v: &Value) -> Result<AuthenticationResponse, String> {
    let id = parse_id(v)?;
    let response = parse_response(v)?;
    Ok(AuthenticationResponse {
        id,
        client_data_json: decode_field(response, "clientDataJSON")?,
        authenticator_data: decode_field(response, "authenticatorData")?,
        signature: decode_field(response, "signature")?,
        user_handle: match response.get("userHandle") {
            None | Some(Value::Null) => None,
            Some(_) => Some(decode_field(response, "userHandle")?),
        },
    })
}

/// A session is valid while `now < expires_at`; sensitive actions also
/// need `now - auth_at <= step_up_secs`.
pub fn session_valid(now: i64, expires_at: i64) -> bool {
    now < expires_at
}

/// Whether the time since authentication is within the step-up window.
pub fn step_up_ok(now: i64, auth_at: i64, step_up_secs: u64) -> bool {
    i128::from(now) - i128::from(auth_at) <= i128::from(step_up_secs)
}

fn decode_field(v: &Value, field: &str) -> Result<Vec<u8>, String> {
    let value = v
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or bad {field}: expected a base64url string"))?;
    URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| URL_SAFE.decode(value))
        .map_err(|_| format!("bad {field}: invalid base64url"))
}

fn parse_id(v: &Value) -> Result<Vec<u8>, String> {
    if v.get("type").and_then(Value::as_str) != Some("public-key") {
        return Err("missing or bad type: expected public-key".into());
    }
    let id = decode_field(v, "id")?;
    if id != decode_field(v, "rawId")? {
        return Err("bad rawId: does not match id".into());
    }
    Ok(id)
}

fn parse_response(v: &Value) -> Result<&Value, String> {
    v.get("response")
        .filter(|response| response.is_object())
        .ok_or_else(|| "missing or bad response: expected an object".into())
}
