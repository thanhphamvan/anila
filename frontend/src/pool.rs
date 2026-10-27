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

struct Entry {
    session: SessionName,
    div: web_sys::HtmlDivElement,
    term: Terminal,
    fit: FitAddon,
    _on_data: Closure<dyn FnMut(String)>,
    ch: Ch,
    req: Req,
    size: (u16, u16),
    banner: String,
    shown_at: u64,
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
            container.remove();
        }
    }

    /// Reuse an attachment, evicting the least recently shown when full.
    pub(crate) fn show(
        &mut self,
        model: Model,
        conn: &Connection,
        session: SessionName,
        create: Option<Create>,
    ) -> Result<(), String> {
        if !self.entries.iter().any(|e| e.session == session) {
            let container = self
                .container
                .as_ref()
                .ok_or("Terminal container unavailable")?
                .clone();
            let host = model.current.get_untracked().ok_or("Machine unavailable")?;
            if self.entries.len() >= MAX_CHANNELS_PER_TAB
                && let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|e| e.shown_at)
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
            if let Err(e) = container.append_child(&div) {
                term.dispose();
                return Err(api::error(e));
            }
            for entry in &self.entries {
                entry.div.set_hidden(true);
            }
            term.load_addon(&fit);
            term.open(&div);
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
                ch,
                req,
                size,
                banner: String::new(),
                shown_at: 0,
            });
        }
        self.counter += 1;
        for entry in &mut self.entries {
            let shown = entry.session == session;
            entry.div.set_hidden(!shown);
            if shown {
                entry.shown_at = self.counter;
                model.banner.set(entry.banner.clone());
            }
        }
        self.fit_shown(conn);
        // The caller focuses after releasing the pool: focusing can make
        // xterm emit input (a focus report) synchronously.
        Ok(())
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
            if !entry.div.hidden()
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
    pub(crate) fn shown_ch(&self) -> Option<Ch> {
        self.entries.iter().find(|e| !e.div.hidden()).map(|e| e.ch)
    }

    /// Whether this session still has an attached terminal.
    pub(crate) fn contains(&self, session: &SessionName) -> bool {
        self.entries.iter().any(|e| &e.session == session)
    }

    /// Clone the JS handle so the caller can focus after releasing the pool borrow.
    pub(crate) fn shown_terminal(&self) -> Option<Terminal> {
        self.entries
            .iter()
            .find(|e| !e.div.hidden() && e.div.is_connected())
            .map(|e| JsValue::from(&e.term).unchecked_into())
    }
}
