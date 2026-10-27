use serde_json::json;
use wasm_bindgen::prelude::*;
use worker::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = crypto, js_name = getRandomValues)]
    fn get_random_values(buf: &mut [u8]);
}

/// Cryptographically random bytes from the runtime's Web Crypto.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    get_random_values(&mut buf);
    buf
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn now_secs() -> i64 {
    (Date::now().as_millis() / 1000) as i64
}

/// The C1 error body: `{"error": {"code": ..., "msg": ...}}`.
pub fn json_error(status: u16, code: &str, msg: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": { "code": code, "msg": msg } }))?.with_status(status))
}

/// `DEV_INSECURE = "1"` skips browser sign-in and service signature checks.
/// It is honored only when `ORIGIN` is a localhost URL, so a deployed
/// worker can never run with it.
pub fn insecure_dev_allowed(env: &Env) -> Result<bool> {
    let flag = env
        .var("DEV_INSECURE")
        .map(|v| v.to_string() == "1")
        .unwrap_or(false);
    let origin = env.var("ORIGIN")?.to_string();
    let local = origin.starts_with("http://localhost:") || origin.starts_with("http://127.0.0.1:");
    Ok(flag && local)
}

/// Builds an internal JSON POST to a Durable Object.
pub fn json_request(url: &str, body: &serde_json::Value) -> Result<Request> {
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    init.with_body(Some(serde_json::to_string(body)?.into()));
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    init.with_headers(headers);
    Request::new_with_init(url, &init)
}
