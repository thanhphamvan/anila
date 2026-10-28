//! Copy completed terminal selections and handle local copy shortcuts.

use crate::{pwa, xterm::Terminal};
use leptos::prelude::*;
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, spawn_local};

/// Retain application copies per terminal so a direct click can retry a
/// clipboard write rejected for lacking browser user activation.
#[derive(Clone, Default)]
pub(crate) struct CopyBuffer(Rc<RefCell<Option<String>>>);

impl CopyBuffer {
    pub(crate) fn text(&self, term: &Terminal) -> Option<String> {
        let selected = term.get_selection();
        if selected.is_empty() {
            self.0.borrow().clone()
        } else {
            Some(selected)
        }
    }

    pub(crate) fn clear(&self) {
        self.0.borrow_mut().take();
    }
}

pub(crate) fn copy(text: &str, banner: RwSignal<String>) {
    if text.is_empty() {
        return;
    }
    // Invoke writeText before yielding, retaining browser user activation.
    match pwa::start_copy(text) {
        Ok(promise) => spawn_local(async move {
            if JsFuture::from(promise).await.is_err() {
                banner.try_set("Could not copy terminal text. Tap Copy to retry. Allow clipboard access in your browser and try again.".into());
            } else {
                clear_error(banner);
            }
        }),
        Err(error) => {
            banner.try_set(format!("Could not copy terminal text: {error}"));
        }
    }
}

fn clear_error(banner: RwSignal<String>) {
    banner.try_update(|message| {
        if message.starts_with("Could not copy terminal text")
            || message.starts_with("Select terminal text first")
        {
            message.clear();
        }
    });
}

/// Run directly in a trusted click or keyboard event, before any await.
pub(crate) fn copy_explicit(text: &str, banner: RwSignal<String>) {
    if text.is_empty() {
        return;
    }
    if pwa::native_copy(text).unwrap_or(false) {
        clear_error(banner);
    } else {
        copy(text, banner);
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
        if container.is_connected()
            && !container.hidden()
            && container.get_attribute("data-active").as_deref() != Some("false")
        {
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
    buffer: CopyBuffer,
) -> Closure<dyn FnMut(String) -> bool> {
    let container = container.clone();
    let handler = Closure::new(move |data: String| {
        if container.is_connected()
            && !container.hidden()
            && let Some(text) = crate::clipboard_payload::decode(&data)
        {
            *buffer.0.borrow_mut() = Some(text.clone());
            if container.get_attribute("data-active").as_deref() != Some("false") {
                copy(&text, banner);
            }
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
    buffer: CopyBuffer,
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
        let mut text = target.get_selection();
        if text.is_empty() && (command || (control && event.shift_key())) {
            text = buffer.text(&target).unwrap_or_default();
        }
        if text.is_empty() && !(control && event.shift_key()) {
            // In particular, bare Ctrl+C remains a terminal interrupt.
            return true;
        }
        // Stop terminal input and browser shortcuts such as opening DevTools.
        event.prevent_default();
        if event.type_() == "keydown" && !text.is_empty() {
            copy_explicit(&text, banner);
        }
        false
    });
    term.attach_key_handler(&handler);
    handler
}
