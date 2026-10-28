//! Attached terminals for one machine, independent of the view lifetime.

use crate::{
    api,
    app::Model,
    conn::Connection,
    xterm::{FitAddon, Terminal, options},
};
use anila_proto::{Ch, Create, Req, SessionName, client::Event, limits::MAX_CHANNELS_PER_TAB};
use leptos::prelude::*;
use wasm_bindgen::prelude::*;

/// Owns the terminal DOM and channels until explicitly closed or cleared.
#[derive(Default)]
pub(crate) struct Pool {
    entries: Vec<Entry>,
    container: Option<web_sys::HtmlDivElement>,
    counter: u64,
}

pub(crate) struct PanelMount<'a> {
    pub(crate) host: &'a anila_proto::HostId,
    pub(crate) session: SessionName,
    pub(crate) create: Option<Create>,
    pub(crate) parent: &'a web_sys::HtmlElement,
    pub(crate) index: usize,
    pub(crate) active: bool,
}

struct Entry {
    session: SessionName,
    div: web_sys::HtmlDivElement,
    term: Terminal,
    fit: FitAddon,
    _on_data: Closure<dyn FnMut(String)>,
    _on_key: Closure<dyn FnMut(web_sys::KeyboardEvent) -> bool>,
    _on_selection: Closure<dyn FnMut()>,
    _on_osc52: Closure<dyn FnMut(String) -> bool>,
    clipboard: crate::clipboard::CopyBuffer,
    ch: Ch,
    req: Req,
    size: (u16, u16),
    banner: String,
    shown_at: u64,
    active: bool,
}

fn div() -> Result<web_sys::HtmlDivElement, String> {
    web_sys::window()
        .and_then(|w| w.document())
        .ok_or("Document unavailable")?
        .create_element("div")
        .map_err(api::error)?
        .dyn_into()
        .map_err(|e| api::error(e.into()))
}

impl Pool {
    /// Mount the same container each time the terminal view opens.
    pub(crate) fn mount(&mut self, parent: &web_sys::HtmlElement) -> Result<(), String> {
        if self.container.is_none() {
            let container = div()?;
            container.set_class_name("terms");
            self.container = Some(container);
        }
        if let Some(container) = &self.container {
            parent.append_child(container).map_err(api::error)?;
        }
        Ok(())
    }

    /// Detach the view without destroying terminals or channels.
    pub(crate) fn detach(&self) {
        if let Some(container) = &self.container {
            for entry in &self.entries {
                entry.div.set_hidden(true);
                let _ = container.append_child(&entry.div);
            }
            container.remove();
        }
    }

    /// Reuse an attachment, evicting the least recently shown when full.
    pub(crate) fn show_in(
        &mut self,
        model: Model,
        conn: &Connection,
        placement: PanelMount<'_>,
    ) -> Result<(), String> {
        let PanelMount {
            host,
            session,
            create,
            parent,
            index,
            active,
        } = placement;
        if !self.entries.iter().any(|e| e.session == session) {
            if self.entries.len() >= MAX_CHANNELS_PER_TAB
                && let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|e| (!e.div.hidden(), e.shown_at))
                    .map(|e| e.session.clone())
            {
                self.close(conn, &oldest);
            }
            let div = div()?;
            div.set_class_name("term");
            div.set_attribute("data-host", host.as_str())
                .map_err(api::error)?;
            div.set_attribute("data-session", session.as_str())
                .map_err(api::error)?;
            let term = Terminal::new(&options()).map_err(api::error)?;
            let fit = match FitAddon::new() {
                Ok(fit) => fit,
                Err(e) => {
                    term.dispose();
                    return Err(api::error(e));
                }
            };
            if let Err(e) = parent.append_child(&div) {
                term.dispose();
                return Err(api::error(e));
            }
            div.set_attribute("data-panel", &index.to_string())
                .map_err(api::error)?;
            div.set_attribute("data-active", &active.to_string())
                .map_err(api::error)?;
            term.load_addon(&fit);
            term.open(&div);
            let clipboard = crate::clipboard::CopyBuffer::default();
            let on_key = crate::clipboard::install(&term, model.banner, clipboard.clone());
            let on_selection = crate::clipboard::install_selection(&term, &div, model.banner);
            let on_osc52 =
                crate::clipboard::install_osc52(&term, &div, model.banner, clipboard.clone());
            fit.fit();
            let size = (term.cols(), term.rows());
            let (ch, req) = conn.open(session.clone(), create, size.0, size.1);
            let input_conn = conn.clone();
            let on_data = Closure::new(move |data: String| model.send(&input_conn, ch, &data));
            term.on_data(&on_data);
            self.entries.push(Entry {
                session: session.clone(),
                div,
                term,
                fit,
                _on_data: on_data,
                _on_key: on_key,
                _on_selection: on_selection,
                _on_osc52: on_osc52,
                clipboard,
                ch,
                req,
                size,
                banner: String::new(),
                shown_at: 0,
                active,
            });
        }
        self.counter += 1;
        for entry in &mut self.entries {
            if entry.session == session {
                if !entry
                    .div
                    .parent_element()
                    .is_some_and(|element| element.is_same_node(Some(parent)))
                {
                    parent.append_child(&entry.div).map_err(api::error)?;
                }
                entry.div.set_hidden(false);
                entry
                    .div
                    .set_attribute("data-panel", &index.to_string())
                    .map_err(api::error)?;
                entry
                    .div
                    .set_attribute("data-active", &active.to_string())
                    .map_err(api::error)?;
                entry.active = active;
                entry.shown_at = self.counter;
            }
        }
        self.fit_shown(conn);
        // The caller focuses after releasing the pool: focusing can make
        // xterm emit input (a focus report) synchronously.
        Ok(())
    }

    pub(crate) fn hide_except(&mut self, sessions: &[SessionName]) {
        for entry in &mut self.entries {
            if !sessions.contains(&entry.session) {
                entry.div.set_hidden(true);
                entry.active = false;
                if let Some(container) = &self.container {
                    let _ = container.append_child(&entry.div);
                }
            }
        }
    }

    pub(crate) fn activate(&mut self, session: Option<&SessionName>) {
        for entry in &mut self.entries {
            entry.active = session == Some(&entry.session);
            let _ = entry
                .div
                .set_attribute("data-active", &entry.active.to_string());
        }
    }

    pub(crate) fn banner_for(&self, session: &SessionName) -> String {
        self.entries
            .iter()
            .find(|entry| &entry.session == session)
            .map(|entry| entry.banner.clone())
            .unwrap_or_default()
    }

    /// Deliver output and channel status even while the view is detached.
    pub(crate) fn on_event(&mut self, event: &Event, banner: Option<RwSignal<String>>) {
        for entry in &mut self.entries {
            match event {
                Event::Output { ch, bytes } if *ch == entry.ch => {
                    entry
                        .term
                        .write_bytes(&js_sys::Uint8Array::from(bytes.as_slice()));
                    continue;
                }
                Event::Reset { ch } if *ch == entry.ch => {
                    entry.clipboard.clear();
                    entry.term.reset();
                    entry.banner.clear();
                }
                Event::Exited { ch, reason } if *ch == entry.ch => entry.banner = reason.clone(),
                Event::Error { ch, req, msg, .. }
                    if *ch == Some(entry.ch) || *req == Some(entry.req) =>
                {
                    entry.banner = msg.clone()
                }
                _ => continue,
            }
            if entry.active
                && !entry.div.hidden()
                && let Some(banner) = banner
            {
                banner.set(entry.banner.clone());
            }
        }
    }

    /// Close an attachment while leaving its tmux session running.
    pub(crate) fn close(&mut self, conn: &Connection, session: &SessionName) {
        if let Some(index) = self.entries.iter().position(|e| &e.session == session) {
            let entry = self.entries.remove(index);
            conn.close_channel(entry.ch);
            entry.term.dispose();
            entry.div.remove();
        }
    }

    /// Discard attachments before dropping or replacing their connection.
    pub(crate) fn clear(&mut self) {
        for entry in self.entries.drain(..) {
            entry.term.dispose();
            entry.div.remove();
        }
        self.detach();
        self.counter = 0;
    }

    /// Fit only a visible, mounted terminal and publish changed dimensions.
    pub(crate) fn fit_shown(&mut self, conn: &Connection) {
        for entry in &mut self.entries {
            if entry.div.hidden() || !entry.div.is_connected() {
                continue;
            }
            entry.fit.fit();
            let size = (entry.term.cols(), entry.term.rows());
            if size != entry.size {
                entry.size = size;
                conn.resize(entry.ch, size.0, size.1);
            }
        }
    }

    /// The visible channel used by the key row.
    pub(crate) fn channel_for(&self, session: &SessionName) -> Option<Ch> {
        self.entries
            .iter()
            .find(|e| &e.session == session && !e.div.hidden())
            .map(|e| e.ch)
    }

    pub(crate) fn copy_text(&self, session: &SessionName) -> Option<String> {
        self.entries
            .iter()
            .find(|entry| {
                &entry.session == session && !entry.div.hidden() && entry.div.is_connected()
            })
            .and_then(|entry| entry.clipboard.text(&entry.term))
    }

    /// Whether this session still has an attached terminal.
    pub(crate) fn contains(&self, session: &SessionName) -> bool {
        self.entries.iter().any(|e| &e.session == session)
    }

    /// Clone the JS handle so the caller can focus after releasing the pool borrow.
    pub(crate) fn terminal_for(&self, session: &SessionName) -> Option<Terminal> {
        self.entries
            .iter()
            .find(|e| &e.session == session && !e.div.hidden() && e.div.is_connected())
            .map(|e| JsValue::from(&e.term).unchecked_into())
    }
}
