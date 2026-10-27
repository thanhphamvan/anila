//! Same-origin JSON requests and browser error messages.

use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

/// A readable browser exception, including passkey cancellation.
pub fn error(value: JsValue) -> String {
    let field = |key: &str| {
        js_sys::Reflect::get(&value, &key.into())
            .ok()
            .and_then(|v| v.as_string())
    };
    if field("name").is_some_and(|n| n == "NotAllowedError" || n == "AbortError") {
        return "Passkey prompt cancelled or timed out. Please try again.".into();
    }
    field("message")
        .or_else(|| value.as_string())
        .unwrap_or_else(|| "The browser could not complete this operation.".into())
}

/// Fetch JSON with cookies; empty responses (including 204) return null.
pub async fn request(
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<(u16, Value), String> {
    let window = web_sys::window().ok_or("Browser window unavailable")?;
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    init.set_credentials(web_sys::RequestCredentials::SameOrigin);
    if method == "POST" || method == "DELETE" || method == "PATCH" {
        let headers = js_sys::Object::new();
        js_sys::Reflect::set(&headers, &"Content-Type".into(), &"application/json".into())
            .map_err(error)?;
        init.set_headers(&headers);
        init.set_body(
            &body
                .unwrap_or_else(|| serde_json::json!({}))
                .to_string()
                .into(),
        );
    }
    let response = JsFuture::from(window.fetch_with_str_and_init(path, &init))
        .await
        .map_err(error)?
        .dyn_into::<web_sys::Response>()
        .map_err(error)?;
    let status = response.status();
    let text = JsFuture::from(response.text().map_err(error)?)
        .await
        .map_err(error)?
        .as_string()
        .ok_or("Invalid HTTP response")?;
    let json = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).map_err(|_| format!("Invalid JSON response (HTTP {status})"))?
    };
    Ok((status, json))
}

/// Preserve the worker's human-readable failure message.
pub fn failure(status: u16, json: &Value) -> String {
    json["error"]["msg"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Request failed (HTTP {status})"))
}

/// Retry a protected operation once after a fresh passkey ceremony.
pub async fn request_step_up(
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<(u16, Value), String> {
    let reply = request(method, path, body.clone()).await?;
    if reply.0 == 401 && reply.1["error"]["code"] == "step_up" {
        crate::edition::reauthenticate().await?;
        request(method, path, body).await
    } else {
        Ok(reply)
    }
}

/// Decode a successful API reply using the shared contract.
pub async fn read<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, String> {
    let (status, body) = request("GET", path, None).await?;
    if status != 200 {
        return Err(failure(status, &body));
    }
    serde_json::from_value(body).map_err(|_| "Invalid API response".into())
}

/// Perform a mutation, preserving the worker's error message.
pub async fn change(
    method: &str,
    path: &str,
    body: Option<Value>,
    step_up: bool,
) -> Result<Value, String> {
    let (status, body) = if step_up {
        request_step_up(method, path, body).await?
    } else {
        request(method, path, body).await?
    };
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        Err(failure(status, &body))
    }
}
