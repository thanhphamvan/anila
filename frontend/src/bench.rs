//! Feature-gated hooks for measuring the real wasm decoder.

use std::{cell::RefCell, rc::Rc};

use anila_proto::{Codec, codec::Decoder};
use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::{JsValue, closure::Closure};

/// Installs a fresh decoder reset hook and a timed-piece decode hook on window.
pub fn install() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let decoder = Rc::new(RefCell::new(None::<Decoder>));
    let state = decoder.clone();
    let reset = Closure::<dyn FnMut(String)>::new(move |name: String| {
        let codec = serde_json::from_value::<Codec>(serde_json::Value::String(name));
        *state.borrow_mut() = match codec {
            Ok(codec @ (Codec::None | Codec::DeflateStream | Codec::ZstdStream)) => {
                Some(Decoder::new(codec))
            }
            _ => None,
        };
    });
    let decode = Closure::<dyn FnMut(Uint8Array) -> i32>::new(move |piece: Uint8Array| {
        decoder
            .borrow_mut()
            .as_mut()
            .and_then(|decoder| decoder.decode(&piece.to_vec()).ok())
            .map_or(-1, |bytes| bytes.len() as i32)
    });
    for (name, function) in [
        ("anilaBenchReset", reset.as_ref()),
        ("anilaBenchDecode", decode.as_ref()),
    ] {
        if let Err(error) = Reflect::set(&window, &JsValue::from_str(name), function) {
            wasm_bindgen::throw_val(error);
        }
    }
    reset.forget();
    decode.forget();
}
