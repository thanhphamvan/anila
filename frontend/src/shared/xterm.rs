//! Bindings to the vendored xterm.js (`vendor/xterm/`), loaded as globals
//! by `index.html` before the wasm starts.

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    pub type Terminal;

    #[wasm_bindgen(constructor, catch)]
    pub fn new(options: &JsValue) -> Result<Terminal, JsValue>;

    #[wasm_bindgen(method)]
    pub fn open(this: &Terminal, parent: &web_sys::HtmlElement);

    /// Clear terminal state after a fresh channel epoch.
    #[wasm_bindgen(method)]
    pub fn reset(this: &Terminal);

    /// Let xterm decode the byte stream, preserving split UTF-8 sequences.
    #[wasm_bindgen(method, js_name = write)]
    pub fn write_bytes(this: &Terminal, data: &js_sys::Uint8Array);

    #[wasm_bindgen(method)]
    pub fn focus(this: &Terminal);

    #[wasm_bindgen(method)]
    pub fn dispose(this: &Terminal);

    #[wasm_bindgen(method, getter)]
    pub fn cols(this: &Terminal) -> u16;

    #[wasm_bindgen(method, getter)]
    pub fn rows(this: &Terminal) -> u16;

    #[wasm_bindgen(method, js_name = loadAddon)]
    pub fn load_addon(this: &Terminal, addon: &FitAddon);

    /// Called with each chunk the user types.
    #[wasm_bindgen(method, js_name = onData)]
    pub fn on_data(this: &Terminal, callback: &Closure<dyn FnMut(String)>) -> JsValue;
}

#[wasm_bindgen]
extern "C" {
    pub type FitAddon;

    #[wasm_bindgen(method)]
    pub fn fit(this: &FitAddon);
}

impl FitAddon {
    /// `addon-fit.js` exposes the class as the global `FitAddon.FitAddon`.
    pub fn new() -> Result<FitAddon, JsValue> {
        let global = js_sys::global();
        let ns = js_sys::Reflect::get(&global, &"FitAddon".into())?;
        let class: js_sys::Function = js_sys::Reflect::get(&ns, &"FitAddon".into())?.dyn_into()?;
        Ok(js_sys::Reflect::construct(&class, &js_sys::Array::new())?.unchecked_into())
    }
}

/// Shared terminal appearance and scrollback settings.
pub fn options() -> JsValue {
    let o = js_sys::Object::new();
    let set = |k: &str, v: JsValue| {
        let _ = js_sys::Reflect::set(&o, &k.into(), &v);
    };
    set("cursorBlink", true.into());
    set("fontSize", 14.into());
    set(
        "fontFamily",
        "\"SF Mono\", Menlo, \"IBM Plex Mono\", monospace".into(),
    );
    set("scrollback", 10_000.into());
    o.into()
}
