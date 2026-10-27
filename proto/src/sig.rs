//! The exact bytes the service signs with its ed25519 key. Hashing and
//! signing happen in the callers, so this crate stays free of crypto.

pub const CONTEXT: &str = "term-agent-v1";

/// Signed when answering the room's `challenge` on the websocket.
pub fn ws_sign_input(host: &str, ts: i64, nonce: &str) -> String {
    format!("{CONTEXT}\nWS\n{host}\n{ts}\n{nonce}")
}

/// Signed for `POST /api/agent/*` calls. `body_sha256_hex` is the lowercase
/// hex SHA-256 of the request body.
pub fn http_sign_input(method: &str, path: &str, ts: i64, body_sha256_hex: &str) -> String {
    format!("{CONTEXT}\n{method}\n{path}\n{ts}\n{body_sha256_hex}")
}

/// Header names for signed HTTP calls.
pub const HEADER_HOST: &str = "Term-Host";
pub const HEADER_TS: &str = "Term-Ts";
pub const HEADER_SIG: &str = "Term-Sig";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_are_stable() {
        assert_eq!(
            ws_sign_input("my-mac", 1792585534, "bm9uY2U"),
            "term-agent-v1\nWS\nmy-mac\n1792585534\nbm9uY2U"
        );
        assert_eq!(
            http_sign_input("POST", "/api/agent/enroll-code", 5, "e3b0"),
            "term-agent-v1\nPOST\n/api/agent/enroll-code\n5\ne3b0"
        );
    }
}
