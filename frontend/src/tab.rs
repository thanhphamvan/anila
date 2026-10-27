//! The browser tab's id: random, nonzero, and kept in sessionStorage so it
//! survives reloads and reconnects (see the identifiers contract).

use anila_proto::Tab;

const KEY: &str = "anila.tab";

/// Return the persisted id, or generate it using Web Crypto.
pub fn tab_id() -> Result<Tab, String> {
    let window = web_sys::window().ok_or("Browser window unavailable")?;
    let storage = window.session_storage().ok().flatten();
    if let Some(t) = storage
        .as_ref()
        .and_then(|s| s.get_item(KEY).ok().flatten())
        .and_then(|v| v.parse::<Tab>().ok())
        .filter(|t| *t != 0)
    {
        return Ok(t);
    }
    let tab = loop {
        let mut buf = [0u8; 4];
        let crypto = window.crypto().map_err(crate::api::error)?;
        crypto
            .get_random_values_with_u8_array(&mut buf)
            .map_err(crate::api::error)?;
        let t = u32::from_be_bytes(buf);
        if t != 0 {
            break t;
        }
    };
    if let Some(s) = storage {
        let _ = s.set_item(KEY, &tab.to_string());
    }
    Ok(tab)
}
