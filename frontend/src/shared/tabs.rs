//! Open sessions as tabs, like a browser or an IDE: one tab per session
//! opened from this device, across machines, kept in localStorage so a
//! reload brings them back. Closing a tab only detaches; the tmux session
//! keeps running.

use crate::app::{Model, Screen};
use anila_proto::{AgentStatus, HostId, SessionName};
use leptos::prelude::*;
use serde::{Deserialize, Serialize};

const KEY: &str = "anila.tabs";

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub(crate) struct OpenTab {
    pub(crate) host: HostId,
    pub(crate) host_name: String,
    pub(crate) session: SessionName,
}

impl OpenTab {
    fn is(&self, host: &HostId, session: &SessionName) -> bool {
        &self.host == host && &self.session == session
    }
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// The tabs saved on this device; empty when storage is unavailable.
pub(crate) fn load() -> Vec<OpenTab> {
    storage()
        .and_then(|s| s.get_item(KEY).ok().flatten())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub(crate) fn save(tabs: &[OpenTab]) {
    if let (Some(storage), Ok(json)) = (storage(), serde_json::to_string(tabs)) {
        let _ = storage.set_item(KEY, &json);
    }
}

/// Adds a tab for `session` unless it is open already.
pub(crate) fn open(tabs: &mut Vec<OpenTab>, tab: OpenTab) {
    match tabs.iter_mut().find(|t| t.is(&tab.host, &tab.session)) {
        Some(existing) => existing.host_name = tab.host_name,
        None => tabs.push(tab),
    }
}

/// Removes a tab and returns the one to show instead: the next tab to the
/// right, else the one to the left.
pub(crate) fn close(
    tabs: &mut Vec<OpenTab>,
    host: &HostId,
    session: &SessionName,
) -> Option<OpenTab> {
    let index = tabs.iter().position(|t| t.is(host, session))?;
    tabs.remove(index);
    tabs.get(index).or_else(|| tabs.last()).cloned()
}

impl Model {
    /// Shows a tab's session, switching machines when needed.
    pub(crate) fn show_tab(self, tab: OpenTab) {
        if self.current.get_untracked().as_ref() == Some(&tab.host) {
            self.error.set(String::new());
            self.screen.set(Screen::Terminal(tab.session, None));
        } else if self.has_terminal(&tab.host, &tab.session) {
            self.pending_session.set(None);
            self.connect(tab.host);
            self.screen.set(Screen::Terminal(tab.session, None));
        } else {
            self.target(Some(tab.host), Some(tab.session));
        }
    }

    fn close_tab(self, tab: &OpenTab) {
        let showing = matches!(
            self.screen.get_untracked(),
            Screen::Terminal(ref name, _) if self.current.get_untracked().as_ref() == Some(&tab.host) && name == &tab.session
        );
        self.close_terminal(&tab.host, &tab.session);
        let mut next = None;
        self.tabs
            .update(|tabs| next = close(tabs, &tab.host, &tab.session));
        if showing {
            match next {
                Some(next) => self.show_tab(next),
                None => self.sessions(),
            }
        }
    }
}

/// The text of the connection status pill.
fn status(model: Model) -> String {
    if model.stopped.get() {
        return "Disconnected".into();
    }
    if !model.connected.get() {
        return "Reconnecting…".into();
    }
    let AgentStatus { online, since, .. } = model.agent.get();
    if online {
        return "Live".into();
    }
    match since {
        Some(since) => {
            let date = js_sys::Date::new(&((since as f64) * 1000.0).into());
            format!(
                "Host offline since {:02}:{:02}",
                date.get_hours(),
                date.get_minutes()
            )
        }
        None => "Host offline".into(),
    }
}

/// One row at the top of the session and terminal screens: a home button
/// back to the machines, the open tabs, a "+" for this machine's sessions,
/// and the connection status (a dot while live, words otherwise).
#[component]
pub(crate) fn TabBar(model: Model) -> impl IntoView {
    let live = move || model.connected.get() && model.agent.get().online && !model.stopped.get();
    // Name the machine on each tab only when the tabs span machines.
    let many_hosts = move || {
        model.tabs.with(|tabs| {
            tabs.first()
                .is_some_and(|first| tabs.iter().any(|t| t.host != first.host))
        })
    };
    let active = move |tab: &OpenTab| match model.screen.get() {
        Screen::Terminal(name, _) => {
            model.current.get().as_ref() == Some(&tab.host) && name == tab.session
        }
        _ => false,
    };
    view! {
        <nav class="tabbar" aria-label="Open sessions">
            <button class="home" title="Machines" aria-label="Machines" on:click=move |_| model.machines()>
                <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
                    <path d="M2 7.5 8 2.5l6 5M3.8 6.3V13.5h3V10h2.4v3.5h3V6.3" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round" />
                </svg>
            </button>
            <div class="tabs" role="tablist">
                <For
                    each=move || model.tabs.get()
                    key=|tab| (tab.host.clone(), tab.session.clone())
                    children=move |tab| {
                        let selected = {
                            let tab = tab.clone();
                            Memo::new(move |_| active(&tab))
                        };
                        let title = format!("{} on {}", tab.session, tab.host_name);
                        let (show, close) = (tab.clone(), tab.clone());
                        let host = tab.host.clone();
                        let offline = move || model.links.with(|links| links.get(&host)
                            .is_some_and(|link| !link.connected || !link.online || link.stopped));
                        view! {
                            <div class="tab" class:active=selected class:offline=offline title=title>
                                <button class="tab-open" role="tab" aria-selected=move || selected.get().to_string() on:click=move |_| model.show_tab(show.clone())>
                                    <span class="tab-host" hidden=move || !many_hosts()>{tab.host_name.clone()}</span>
                                    <span class="tab-name">{tab.session.to_string()}</span>
                                </button>
                                <button class="tab-close" title="Close tab (the session keeps running)" aria-label=format!("Close {}", tab.session) on:click=move |_| model.close_tab(&close)>"×"</button>
                            </div>
                        }
                    }
                />
            </div>
            <button class="tab-new" class:active=move || model.screen.get() == Screen::Sessions title=move || format!("Sessions on {}", model.host.get()) aria-label="Sessions" on:click=move |_| model.sessions()>"+"</button>
            <span class="pill" class:warn=move || !live() class:dot=live role="status" title=move || format!("{}: {}", model.host.get(), status(model))>
                <span class="pill-text">{move || status(model)}</span>
            </span>
        </nav>
    }
}
