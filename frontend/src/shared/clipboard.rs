//! Local terminal selection shortcuts. Clipboard contents never become input.

use crate::{pwa, xterm::Terminal};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, spawn_local};

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
            // Invoke writeText before yielding, retaining browser user activation.
            match pwa::start_copy(&text) {
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
        false
    });
    term.attach_key_handler(&handler);
    handler
}
