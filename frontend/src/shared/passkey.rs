//! Manual WebAuthn Level 3 JSON conversion for older browsers.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use js_sys::{Array, Function, Reflect, Uint8Array};
use serde_json::{Value, json};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::api::error;

fn get(object: &JsValue, key: &str) -> Result<JsValue, String> {
    Reflect::get(object, &key.into()).map_err(error)
}

fn decode(object: &JsValue, key: &str) -> Result<(), String> {
    let encoded = get(object, key)?
        .as_string()
        .ok_or_else(|| format!("Missing passkey {key}"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded.trim_end_matches('='))
        .map_err(|_| format!("Invalid passkey {key}"))?;
    Reflect::set(object, &key.into(), &Uint8Array::from(bytes.as_slice())).map_err(error)?;
    Ok(())
}

fn encode(object: &JsValue, key: &str) -> Result<Value, String> {
    let value = get(object, key)?;
    if value.is_null() || value.is_undefined() {
        if key == "userHandle" {
            return Ok(Value::Null);
        }
        return Err(format!("Missing credential {key}"));
    }
    if !value.is_instance_of::<js_sys::ArrayBuffer>() {
        return Err(format!("Invalid credential {key}"));
    }
    Ok(Value::String(
        URL_SAFE_NO_PAD.encode(Uint8Array::new(&value).to_vec()),
    ))
}

/// Request a passkey and serialize the credential without relying on toJSON.
pub async fn credential(options: &Value, create: bool) -> Result<Value, String> {
    let public_key = js_sys::JSON::parse(&options.to_string()).map_err(error)?;
    decode(&public_key, "challenge")?;
    if create {
        decode(&get(&public_key, "user")?, "id")?;
    }
    for key in ["excludeCredentials", "allowCredentials"] {
        let list = get(&public_key, key)?;
        if !list.is_undefined() && !list.is_null() {
            if !Array::is_array(&list) {
                return Err(format!("Invalid passkey {key}"));
            }
            for item in Array::from(&list).iter() {
                decode(&item, "id")?;
            }
        }
    }
    let args = js_sys::Object::new();
    Reflect::set(&args, &"publicKey".into(), &public_key).map_err(error)?;
    let window = web_sys::window().ok_or("Browser window unavailable")?;
    let credentials = get(window.navigator().as_ref(), "credentials")?;
    let method = get(&credentials, if create { "create" } else { "get" })?
        .dyn_into::<Function>()
        .map_err(|_| "Passkeys are unavailable in this browser".to_string())?;
    let promise = method
        .call1(&credentials, &args)
        .map_err(error)?
        .dyn_into::<js_sys::Promise>()
        .map_err(error)?;
    let result = JsFuture::from(promise).await.map_err(error)?;
    if result.is_null() {
        return Err("Passkey prompt cancelled. Please try again.".into());
    }
    let response = get(&result, "response")?;
    let mut fields = json!({"clientDataJSON": encode(&response, "clientDataJSON")?});
    for key in if create {
        &["attestationObject"][..]
    } else {
        &["authenticatorData", "signature", "userHandle"][..]
    } {
        fields[*key] = encode(&response, key)?;
    }
    Ok(
        json!({"id": get(&result, "id")?.as_string().ok_or("Missing credential id")?, "rawId": encode(&result, "rawId")?, "type": get(&result, "type")?.as_string().ok_or("Missing credential type")?, "response": fields}),
    )
}
