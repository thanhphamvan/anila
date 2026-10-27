//! Optional browser PWA APIs, accessed dynamically to support older browsers.

use anila_proto::{HostId, SessionName};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use leptos::prelude::*;
use serde_json::json;
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::api;

pub(crate) fn get(object: &JsValue, key: &str) -> Result<JsValue, String> {
    Reflect::get(object, &key.into()).map_err(api::error)
}

pub(crate) fn call(object: &JsValue, key: &str, args: &[JsValue]) -> Result<JsValue, String> {
    get(object, key)?
        .dyn_into::<Function>()
        .map_err(|_| format!("Browser API {key} is unavailable"))?
        .apply(object, &args.iter().collect::<Array>())
        .map_err(api::error)
}

async fn resolve(value: JsValue) -> Result<JsValue, String> {
    JsFuture::from(Promise::resolve(&value))
        .await
        .map_err(api::error)
}

fn service_worker() -> Result<JsValue, String> {
    let window = web_sys::window().ok_or("Browser window unavailable")?;
    let worker = get(window.navigator().as_ref(), "serviceWorker")?;
    if worker.is_undefined() || worker.is_null() {
        return Err("Service workers are unavailable in this browser".into());
    }
    Ok(worker)
}

async fn registration() -> Result<JsValue, String> {
    let worker = service_worker()?;
    resolve(call(&worker, "register", &["/sw.js".into()])?).await?;
    resolve(get(&worker, "ready")?).await
}

/// Register the shell worker without preventing startup if unsupported.
pub fn register() {
    spawn_local(async {
        let _ = registration().await;
    });
}

/// Read and remove the notification parameter, preserving other URL fields.
pub fn launch_target() -> (Option<HostId>, Option<SessionName>) {
    let read = || -> Result<(Option<HostId>, Option<SessionName>), String> {
        let global = js_sys::global();
        let href = get(&get(&global, "location")?, "href")?;
        let constructor = get(&global, "URL")?
            .dyn_into::<Function>()
            .map_err(api::error)?;
        let url = Reflect::construct(&constructor, &Array::of1(&href)).map_err(api::error)?;
        let params = get(&url, "searchParams")?;
        let session = call(&params, "get", &["session".into()])?.as_string();
        let host = call(&params, "get", &["host".into()])?.as_string();
        if session.is_some() || host.is_some() {
            call(&params, "delete", &["host".into()])?;
            call(&params, "delete", &["session".into()])?;
            let history = get(&global, "history")?;
            call(
                &history,
                "replaceState",
                &[get(&history, "state")?, "".into(), get(&url, "href")?],
            )?;
        }
        Ok((
            host.and_then(|h| HostId::new(h).ok()),
            session.and_then(|s| SessionName::new(s).ok()),
        ))
    };
    read().unwrap_or_default()
}

/// Owned listener on the service-worker container, not arbitrary window messages.
pub struct Listener {
    worker: JsValue,
    callback: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = call(
            &self.worker,
            "removeEventListener",
            &["message".into(), self.callback.as_ref().clone()],
        );
    }
}

/// Forward validated notification sessions to the application model.
pub fn listen(
    open: impl Fn(Option<HostId>, Option<SessionName>) + 'static,
) -> Result<Listener, String> {
    let worker = service_worker()?;
    let callback = Closure::new(move |event: web_sys::MessageEvent| {
        let session = get(&event.data(), "open")
            .ok()
            .and_then(|v| v.as_string())
            .and_then(|s| SessionName::new(s).ok());
        let host = get(&event.data(), "host")
            .ok()
            .and_then(|v| v.as_string())
            .and_then(|s| HostId::new(s).ok());
        if session.is_some() || host.is_some() {
            open(host, session);
        }
    });
    call(
        &worker,
        "addEventListener",
        &["message".into(), callback.as_ref().clone()],
    )?;
    Ok(Listener { worker, callback })
}

fn supported() -> bool {
    ["PushManager", "Notification"]
        .iter()
        .all(|key| get(&js_sys::global(), key).is_ok_and(|v| !v.is_undefined() && !v.is_null()))
}

fn ios_install_hint() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let nav = window.navigator();
    let ua = get(nav.as_ref(), "userAgent")
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    let ios = ["iPhone", "iPad", "iPod"].iter().any(|s| ua.contains(s))
        || (ua.contains("Macintosh")
            && get(nav.as_ref(), "maxTouchPoints")
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                > 1.0);
    ios && get(nav.as_ref(), "standalone")
        .ok()
        .and_then(|v| v.as_bool())
        != Some(true)
        && get(&js_sys::global(), "PushManager").is_ok_and(|v| v.is_undefined())
}

async fn subscription() -> Result<JsValue, String> {
    let manager = get(&registration().await?, "pushManager")?;
    resolve(call(&manager, "getSubscription", &[])?).await
}

async fn post(path: &str, body: serde_json::Value) -> Result<(), String> {
    let (status, result) = api::request("POST", path, Some(body)).await?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(api::failure(status, &result))
    }
}

async fn enable() -> Result<JsValue, String> {
    let window = web_sys::window().ok_or("Browser window unavailable")?;
    let response = resolve(window.fetch_with_str("/api/push/key").into())
        .await?
        .dyn_into::<web_sys::Response>()
        .map_err(api::error)?;
    let status = response.status();
    if status == 503 {
        return Err("Push is not set up on the worker yet".into());
    }
    let text = resolve(response.text().map_err(api::error)?.into())
        .await?
        .as_string()
        .ok_or("Invalid push key response")?;
    let body: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| format!("Invalid push key response (HTTP {status})"))?;
    if status != 200 {
        return Err(api::failure(status, &body));
    }
    // The worker answers `{"key": "<base64url VAPID public key>"}`.
    let key = body["key"].as_str().ok_or("Missing push public key")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(key.trim_end_matches('='))
        .map_err(|_| "Invalid push public key")?;
    if bytes.len() != 65 || bytes.first() != Some(&4) {
        return Err("Invalid push public key".into());
    }
    let notification = get(&js_sys::global(), "Notification")?;
    let permission = resolve(call(&notification, "requestPermission", &[])?).await?;
    if permission.as_string().as_deref() != Some("granted") {
        return Err(
            "Notifications were not allowed. Enable them in your browser settings to try again."
                .into(),
        );
    }
    let manager = get(&registration().await?, "pushManager")?;
    let options = Object::new();
    Reflect::set(&options, &"userVisibleOnly".into(), &true.into()).map_err(api::error)?;
    Reflect::set(
        &options,
        &"applicationServerKey".into(),
        &Uint8Array::from(bytes.as_slice()),
    )
    .map_err(api::error)?;
    let sub = resolve(call(&manager, "subscribe", &[options.into()])?).await?;
    let save = async {
        let serialized =
            js_sys::JSON::stringify(&call(&sub, "toJSON", &[])?).map_err(api::error)?;
        let body = serde_json::from_str(&String::from(serialized))
            .map_err(|_| "Invalid push subscription")?;
        post("/api/push/subscribe", body).await
    }
    .await;
    if let Err(error) = save {
        // Roll back the browser subscription so a failed save remains retryable.
        if let Ok(promise) = call(&sub, "unsubscribe", &[]) {
            let _ = resolve(promise).await;
        }
        return Err(error);
    }
    Ok(sub)
}

async fn disable(sub: &JsValue) -> Result<(), String> {
    let endpoint = get(sub, "endpoint")?
        .as_string()
        .ok_or("Missing push endpoint")?;
    resolve(call(sub, "unsubscribe", &[])?).await?;
    post("/api/push/unsubscribe", json!({"endpoint": endpoint})).await
}

/// Optional push toggle with browser subscription state and actionable errors.
#[component]
pub fn Notifications() -> impl IntoView {
    let available = supported();
    let hint = ios_install_hint();
    let current = StoredValue::new_local(JsValue::NULL);
    let enabled = RwSignal::new(false);
    let busy = RwSignal::new(available);
    let error = RwSignal::new(String::new());
    if available {
        spawn_local(async move {
            match subscription().await {
                Ok(sub) => {
                    enabled.try_set(!sub.is_null() && !sub.is_undefined());
                    current.try_update_value(|value| *value = sub);
                }
                Err(e) => {
                    error.try_set(e);
                }
            }
            busy.try_set(false);
        });
    }
    let toggle = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(String::new());
        let sub = current.get_value();
        let was_enabled = enabled.get_untracked();
        spawn_local(async move {
            let result = if was_enabled {
                disable(&sub).await.map(|()| JsValue::NULL)
            } else {
                enable().await
            };
            match result {
                Ok(sub) => {
                    enabled.try_set(!sub.is_null());
                    current.try_update_value(|value| *value = sub);
                }
                Err(e) => {
                    error.try_set(e);
                }
            }
            busy.try_set(false);
        });
    };
    view! {
        <div hidden=!available>
            <button role="switch" aria-checked=move || enabled.get().to_string() disabled=move || busy.get() on:click=toggle>
                {move || if busy.get() { "Notifications: Checking…" } else if enabled.get() { "Notifications: On" } else { "Notifications: Off" }}
            </button>
            <p class="error" role="alert" hidden=move || error.get().is_empty()>{move || error.get()}</p>
        </div>
        <p class="muted small" hidden=!hint>"Add anila to your Home Screen to get notifications"</p>
    }
}

/// Copy text when the browser exposes the clipboard API.
pub fn copy(text: String) {
    spawn_local(async move {
        let copy = async {
            let navigator = get(&js_sys::global(), "navigator")?;
            let clipboard = get(&navigator, "clipboard")?;
            resolve(call(&clipboard, "writeText", &[text.into()])?).await
        };
        let _ = copy.await;
    });
}

/// The origin used in the pairing command.
pub fn origin() -> String {
    get(&js_sys::global(), "location")
        .and_then(|l| get(&l, "origin"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default()
}
