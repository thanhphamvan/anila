//! Copy completed terminal selections and handle local copy shortcuts.

use crate::{pwa, xterm::Terminal};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, spawn_local};

fn copy(text: &str, banner: RwSignal<String>) {
    if text.is_empty() {
        return;
    }
    // Invoke writeText before yielding, retaining browser user activation.
    match pwa::start_copy(text) {
        Ok(promise) => spawn_local(async move {
            if JsFuture::from(promise).await.is_err() {
                banner.try_set("Could not copy terminal text. Allow clipboard access in your browser and try again.".into());
            }
        }),
        Err(error) => {
            banner.try_set(format!("Could not copy terminal text: {error}"));
        }
    }
}

/// xterm's selection is separate from the browser's DOM selection. Its event
/// fires when a mouse selection completes, synchronously in the mouseup handler.
/// Keep the closure until after disposing the terminal, just like onData.
pub(crate) fn install_selection(
    term: &Terminal,
    container: &web_sys::HtmlDivElement,
    banner: RwSignal<String>,
) -> Closure<dyn FnMut()> {
    let target: Terminal = JsValue::from(term).unchecked_into();
    let container = container.clone();
    let handler = Closure::new(move || {
        if container.is_connected() && !container.hidden() {
            copy(&target.get_selection(), banner);
        }
    });
    term.on_selection_change(&handler);
    handler
}

/// Forward application copy requests, including Codex/tmux OSC 52 forwarding.
pub(crate) fn install_osc52(
    term: &Terminal,
    container: &web_sys::HtmlDivElement,
    banner: RwSignal<String>,
) -> Closure<dyn FnMut(String) -> bool> {
    let container = container.clone();
    let handler = Closure::new(move |data: String| {
        if container.is_connected()
            && !container.hidden()
            && let Some(text) = crate::clipboard_payload::decode(&data)
        {
            copy(&text, banner);
        }
        true
    });
    term.parser().register_osc_handler(52, &handler);
    handler
}

/// Keep this closure alive for exactly as long as the terminal owns its handler.
pub(crate) fn install(
    term: &Terminal,
    banner: RwSignal<String>,
) -> Closure<dyn FnMut(web_sys::KeyboardEvent) -> bool> {
    let target: Terminal = JsValue::from(term).unchecked_into();
    let handler = Closure::new(move |event: web_sys::KeyboardEvent| {
        if !event.key().eq_ignore_ascii_case("c") || event.alt_key() {
            return true;
        }
        let control = event.ctrl_key() && !event.meta_key();
        let command = event.meta_key() && !event.ctrl_key() && !event.shift_key();
        if !control && !command {
            return true;
        }
        let text = target.get_selection();
        if text.is_empty() && !(control && event.shift_key()) {
            // In particular, bare Ctrl+C remains a terminal interrupt.
            return true;
        }
        // Stop terminal input and browser shortcuts such as opening DevTools.
        event.prevent_default();
        if event.type_() == "keydown" && !text.is_empty() {
            copy(&text, banner);
        }
        false
    });
    term.attach_key_handler(&handler);
    handler
}
