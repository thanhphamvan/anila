//! Web Push (RFC 8030) with message encryption (RFC 8291, aes128gcm) and
//! VAPID (RFC 8292). Pure functions: randomness is passed in, so they test
//! natively against the RFC 8291 vector in `proto/vectors/webpush_rfc8291.json`.

use aes_gcm::{Aes128Gcm, KeyInit, aead::Aead};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hkdf::Hkdf;
use p256::{
    PublicKey, SecretKey,
    ecdh::diffie_hellman,
    ecdsa::{Signature, SigningKey, signature::Signer},
    elliptic_curve::sec1::ToEncodedPoint,
};
use serde_json::json;
use sha2::Sha256;

/// Invalid push key material or endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// A key is not a valid P-256 key of the expected encoding and length.
    BadKey,
    /// The endpoint is not an absolute http(s) URL.
    BadEndpoint,
}

/// Encrypts `plaintext` for one subscription as an RFC 8291 `aes128gcm`
/// body: `salt(16) | rs(4, = 4096) | idlen(1, = 65) | as_public(65) |
/// ciphertext`, a single record padded with the `0x02` delimiter.
///
/// `ua_public` is the subscription's `p256dh` (65-byte uncompressed point),
/// `auth` its 16-byte secret, `as_private` a fresh ephemeral P-256 scalar,
/// and `salt` 16 fresh random bytes.
pub fn encrypt(
    plaintext: &[u8],
    ua_public: &[u8],
    auth: &[u8],
    as_private: [u8; 32],
    salt: [u8; 16],
) -> Result<Vec<u8>, PushError> {
    if ua_public.len() != 65 || auth.len() != 16 {
        return Err(PushError::BadKey);
    }
    let ua = PublicKey::from_sec1_bytes(ua_public).map_err(|_| PushError::BadKey)?;
    let secret = SecretKey::from_slice(&as_private).map_err(|_| PushError::BadKey)?;
    let public = secret.public_key().to_encoded_point(false);
    let shared = diffie_hellman(secret.to_nonzero_scalar(), ua.as_affine());
    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(ua_public);
    info.extend_from_slice(public.as_bytes());
    let mut ikm = [0; 32];
    Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes())
        .expand(&info, &mut ikm)
        .map_err(|_| PushError::BadKey)?;
    let hkdf = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0; 16];
    let mut nonce = [0; 12];
    hkdf.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| PushError::BadKey)?;
    hkdf.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| PushError::BadKey)?;
    let mut padded = plaintext.to_vec();
    padded.push(2);
    let ciphertext = Aes128Gcm::new(&cek.into())
        .encrypt(&nonce.into(), padded.as_slice())
        .map_err(|_| PushError::BadKey)?;
    let mut body = salt.to_vec();
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(65);
    body.extend_from_slice(public.as_bytes());
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// `scheme://host[:port]` of a push endpoint, the VAPID audience.
pub fn audience(endpoint: &str) -> Result<String, PushError> {
    let url = worker::Url::parse(endpoint).map_err(|_| PushError::BadEndpoint)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(PushError::BadEndpoint);
    }
    Ok(url.origin().ascii_serialization())
}

/// The VAPID public key (unpadded base64url of the 65-byte uncompressed
/// point) for a 32-byte private scalar. Browsers pass it to
/// `pushManager.subscribe` as `applicationServerKey`.
pub fn vapid_public_key(vapid_private: [u8; 32]) -> Result<String, PushError> {
    let key = SecretKey::from_slice(&vapid_private).map_err(|_| PushError::BadKey)?;
    Ok(B64.encode(key.public_key().to_encoded_point(false).as_bytes()))
}

/// The `Authorization` header value for a push request:
/// `vapid t=<jwt>, k=<vapid public key>`. The JWT header is
/// `{"typ":"JWT","alg":"ES256"}`, the claims `{"aud", "exp", "sub"}`, the
/// signature the raw 64-byte `r || s`, all unpadded base64url.
pub fn vapid_header(
    endpoint: &str,
    subject: &str,
    vapid_private: [u8; 32],
    exp: i64,
) -> Result<String, PushError> {
    let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims =
        B64.encode(json!({"aud": audience(endpoint)?, "exp": exp, "sub": subject}).to_string());
    let input = format!("{header}.{claims}");
    let key = SigningKey::from_slice(&vapid_private).map_err(|_| PushError::BadKey)?;
    let signature: Signature = key.sign(input.as_bytes());
    Ok(format!(
        "vapid t={input}.{}, k={}",
        B64.encode(signature.to_bytes()),
        vapid_public_key(vapid_private)?
    ))
}
