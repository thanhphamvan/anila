//! Passkey sign-in, live sessions, and the terminal.

use crate::{
    account, api,
    conn::{Connection, Notice},
    hosts, passkey,
    pool::Pool,
    pwa,
    tabs::{self, OpenTab, TabBar},
};
use anila_proto::api::{CodeCheck, CodeKind, HostView, Me};
use anila_proto::{
    AgentStatus, Ch, Create, HostId, Preset, SessionInfo, SessionName, client::Event,
};
use leptos::prelude::*;
use serde_json::json;

use std::collections::HashMap;

/// A retained socket and terminal pool with its latest session list.
pub(crate) struct Machine {
    conn: Connection,
    pool: Pool,
    sessions: Vec<SessionInfo>,
    presets: Vec<Preset>,
    agent: AgentStatus,
    used_at: u64,
}

/// Per-machine status, stored in a single application-owned signal.
#[derive(Clone, Copy, Default)]
pub(crate) struct Link {
    pub(crate) connected: bool,
    pub(crate) online: bool,
    pub(crate) stopped: bool,
}

#[derive(Clone, PartialEq)]
pub(crate) enum Screen {
    Loading,
    SignIn,
    Machines,
    Account,
    MachineSettings(HostId),
    AddMachine,
    Sessions,
    Terminal(SessionName, Option<Create>),
}

#[derive(Clone, Copy)]
pub(crate) struct Model {
    pub(crate) me: RwSignal<Option<Me>>,
    pub(crate) hosts: RwSignal<Vec<HostView>>,
    pub(crate) current: RwSignal<Option<HostId>>,
    pub(crate) pending_host: RwSignal<Option<HostId>>,
    pub(crate) screen: RwSignal<Screen>,
    pub(crate) machines: StoredValue<HashMap<HostId, Machine>, LocalStorage>,
    pub(crate) links: RwSignal<HashMap<HostId, Link>>,
    used_at: StoredValue<u64, LocalStorage>,
    pub(crate) banner: RwSignal<String>,
    pub(crate) ctrl: RwSignal<bool>,
    pub(crate) paused: RwSignal<bool>,
    pause_timer: StoredValue<Option<leptos::leptos_dom::helpers::TimeoutHandle>, LocalStorage>,
    pub(crate) host: RwSignal<String>,
    pub(crate) connected: RwSignal<bool>,
    pub(crate) agent: RwSignal<AgentStatus>,
    pub(crate) stopped: RwSignal<bool>,
    pub(crate) error: RwSignal<String>,
    pub(crate) sessions: RwSignal<Vec<SessionInfo>>,
    pub(crate) presets: RwSignal<Vec<Preset>>,
    pub(crate) pending_session: RwSignal<Option<SessionName>>,
    pub(crate) tabs: RwSignal<Vec<OpenTab>>,
}

impl Model {
    /// Send terminal input, applying the key-row modifier and backpressure notice.
    ///
    /// Never touches the pool: xterm can emit input synchronously while the
    /// pool is borrowed (a focus report when it is focused), so input must
    /// not need it.
    pub(crate) fn send(self, conn: &Connection, ch: Ch, data: &str) {
        let mut text = data.to_string();
        if self.ctrl.get_untracked() && text.len() == 1 && text.as_bytes()[0].is_ascii_alphabetic()
        {
            text = ((text.as_bytes()[0] & 0x1f) as char).to_string();
            self.ctrl.set(false);
        }
        let sent = conn.input(ch, text.as_bytes());
        self.paused.set(!sent);
        if let Some(timer) = self.pause_timer.get_value() {
            timer.clear();
        }
        self.pause_timer.set_value(None);
        if !sent {
            match leptos::leptos_dom::helpers::set_timeout_with_handle(
                move || {
                    self.paused.try_set(false);
                },
                std::time::Duration::from_secs(3),
            ) {
                Ok(timer) => self.pause_timer.set_value(Some(timer)),
                Err(e) => self.banner.set(api::error(e)),
            }
        }
    }

    /// Clone the connection before calling transport code, which can notify us.
    pub(crate) fn connection(self, id: &HostId) -> Option<Connection> {
        self.machines
            .with_value(|machines| machines.get(id).map(|m| m.conn.clone()))
    }

    fn with_pool<R>(self, id: &HostId, f: impl FnOnce(&mut Pool) -> R) -> Option<R> {
        self.machines
            .try_update_value(|machines| machines.get_mut(id).map(|m| f(&mut m.pool)))
            .flatten()
    }

    /// Detach a session from its machine, even when that machine is hidden.
    pub(crate) fn close_terminal(self, id: &HostId, session: &SessionName) {
        if let Some(conn) = self.connection(id) {
            self.with_pool(id, |pool| pool.close(&conn, session));
        }
    }

    /// Whether a machine already has this terminal attached.
    pub(crate) fn has_terminal(self, id: &HostId, session: &SessionName) -> bool {
        self.machines
            .with_value(|machines| machines.get(id).is_some_and(|m| m.pool.contains(session)))
    }

    fn focus(self, id: &HostId) {
        let term = self.with_pool(id, |pool| pool.shown_terminal()).flatten();
        if let Some(term) = term {
            term.focus();
        }
    }

    fn drop_machine(self, id: &HostId) {
        let machine = self
            .machines
            .try_update_value(|machines| machines.remove(id))
            .flatten();
        if let Some(mut machine) = machine {
            machine.pool.clear();
            drop(machine);
        }
        self.links.update(|links| {
            links.remove(id);
        });
    }

    /// Clear every terminal before releasing its socket.
    pub(crate) fn disconnect_all(self) {
        let machines = self
            .machines
            .try_update_value(std::mem::take)
            .unwrap_or_default();
        for (_, mut machine) in machines {
            machine.pool.clear();
        }
        self.links.set(HashMap::new());
        self.current.set(None);
        self.connected.set(false);
        self.agent.set(AgentStatus::default());
        self.sessions.set(Vec::new());
        self.presets.set(Vec::new());
        self.pending_session.set(None);
    }

    fn list_machine(self, id: &HostId) {
        if let Some(conn) = self.connection(id) {
            conn.list();
        }
    }

    fn list(self) {
        if let Some(id) = self.current.get_untracked() {
            self.list_machine(&id);
        }
    }
    /// The current machine's session list.
    pub(crate) fn sessions(self) {
        self.screen.set(Screen::Sessions);
        self.list();
    }
    /// Drops tabs of machines this account no longer has.
    fn keep_tabs_of(self, hosts: &[HostView]) {
        self.tabs
            .update(|tabs| tabs.retain(|t| hosts.iter().any(|h| h.id == t.host)));
        let removed = self.machines.with_value(|machines| {
            machines
                .keys()
                .filter(|id| !hosts.iter().any(|h| &h.id == *id))
                .cloned()
                .collect::<Vec<_>>()
        });
        for id in removed {
            self.drop_machine(&id);
            if self.current.get_untracked().as_ref() == Some(&id) {
                self.current.set(None);
                self.connected.set(false);
                self.sessions.set(Vec::new());
                self.presets.set(Vec::new());
                self.screen.set(Screen::Machines);
            }
        }
    }
    pub(crate) fn machines(self) {
        self.screen.set(Screen::Machines);
        self.current.set(None);
        self.pending_session.set(None);
        self.connected.set(false);
        self.refresh_hosts();
    }
    pub(crate) fn refresh_hosts(self) {
        wasm_bindgen_futures::spawn_local(async move {
            match api::read::<Vec<HostView>>("/api/hosts").await {
                Ok(hosts) => {
                    self.keep_tabs_of(&hosts);
                    self.hosts.set(hosts);
                }
                Err(e) => self.error.set(e),
            }
        });
    }
    async fn load(self) -> Result<(), String> {
        self.me.set(Some(api::read::<Me>("/api/me").await?));
        let hosts = api::read::<Vec<HostView>>("/api/hosts").await?;
        let target = hosts::initial_host(
            &hosts,
            self.pending_host.get_untracked().as_ref(),
            hosts::last_host().as_ref(),
        );
        if self.pending_host.get_untracked().is_some()
            && self.pending_host.get_untracked() != target
        {
            self.pending_session.set(None);
        }
        self.keep_tabs_of(&hosts);
        self.hosts.set(hosts);
        self.pending_host.set(None);
        if let Some(id) = target {
            self.connect(id);
        } else {
            self.screen.set(Screen::Machines);
        }
        Ok(())
    }
    pub(crate) fn target(self, host: Option<HostId>, session: Option<SessionName>) {
        if self.me.get_untracked().is_none() {
            self.pending_host.set(host);
            self.pending_session.set(session);
            return;
        }
        if let Some(id) = host {
            if !self.hosts.get_untracked().iter().any(|h| h.id == id) {
                return;
            }
            self.pending_session.set(session);
            if self.current.get_untracked().as_ref() != Some(&id) {
                self.connect(id);
            } else {
                self.list();
            }
        } else {
            self.pending_session.set(session);
            self.list();
        }
    }
    pub(crate) fn connect(self, id: HostId) {
        self.screen.set(Screen::Sessions);
        self.current.set(Some(id.clone()));
        hosts::remember_host(&id);
        self.host.set(
            self.hosts
                .get_untracked()
                .iter()
                .find(|h| h.id == id)
                .map(|h| h.name.to_string())
                .unwrap_or_default(),
        );
        self.error.set(String::new());
        self.used_at.update_value(|n| *n += 1);
        let used_at = self.used_at.get_value();
        let cached = self
            .machines
            .try_update_value(|machines| {
                machines.get_mut(&id).map(|m| {
                    m.used_at = used_at;
                    (m.agent.clone(), m.sessions.clone(), m.presets.clone())
                })
            })
            .flatten();
        let link = self
            .links
            .with_untracked(|links| links.get(&id).copied().unwrap_or_default());
        self.connected.set(link.connected);
        self.stopped.set(link.stopped);
        if let Some((agent, sessions, presets)) = cached {
            self.agent.set(agent);
            self.sessions.set(sessions);
            self.presets.set(presets);
            self.list_machine(&id);
            return;
        }
        self.agent.set(AgentStatus::default());
        self.sessions.set(Vec::new());
        self.presets.set(Vec::new());
        self.links.update(|links| {
            links.insert(id.clone(), Link::default());
        });
        let notice_id = id.clone();
        match Connection::new(id.clone(), move |notice| self.notice(&notice_id, notice)) {
            Ok(conn) => {
                self.machines.update_value(|machines| {
                    machines.insert(
                        id.clone(),
                        Machine {
                            conn,
                            pool: Pool::default(),
                            sessions: Vec::new(),
                            presets: Vec::new(),
                            agent: AgentStatus::default(),
                            used_at,
                        },
                    );
                });
                let oldest = self.machines.with_value(|machines| {
                    (machines.len() > 4)
                        .then(|| {
                            machines
                                .iter()
                                .filter(|(key, _)| *key != &id)
                                .min_by_key(|(_, m)| m.used_at)
                                .map(|(key, _)| key.clone())
                        })
                        .flatten()
                });
                if let Some(oldest) = oldest {
                    self.drop_machine(&oldest);
                }
            }
            Err(e) => {
                self.links.update(|links| {
                    links.remove(&id);
                });
                self.stopped.set(true);
                self.error.set(e);
            }
        }
    }

    fn notice(self, id: &HostId, notice: Notice) {
        let current = self.current.get_untracked().as_ref() == Some(id);
        match notice {
            Notice::Protocol(event) => {
                match &event {
                    Event::Connected { agent, .. } | Event::Status(agent) => {
                        self.links.update(|links| {
                            if let Some(link) = links.get_mut(id) {
                                if matches!(event, Event::Connected { .. }) {
                                    link.connected = true;
                                    link.stopped = false;
                                }
                                link.online = agent.online;
                            }
                        });
                        self.machines.update_value(|machines| {
                            if let Some(m) = machines.get_mut(id) {
                                m.agent = agent.clone();
                            }
                        });
                        if current {
                            self.agent.set(agent.clone());
                            if let Event::Connected { host, .. } = &event {
                                self.host.set(host.clone());
                                self.connected.set(true);
                                self.stopped.set(false);
                                self.error.set(String::new());
                            }
                        }
                        if agent.online || matches!(event, Event::Connected { .. }) {
                            self.list_machine(id);
                        }
                    }
                    Event::Sessions { items, presets, .. } => {
                        self.machines.update_value(|machines| {
                            if let Some(m) = machines.get_mut(id) {
                                m.sessions = items.clone();
                                m.presets = presets.clone();
                            }
                        });
                        if current {
                            self.sessions.set(items.clone());
                            self.presets.set(presets.clone());
                            self.error.set(String::new());
                        }
                        let live = self.links.with_untracked(|links| {
                            links
                                .get(id)
                                .is_some_and(|link| link.connected && link.online)
                        });
                        if live {
                            let showing = if current {
                                match self.screen.get_untracked() {
                                    Screen::Terminal(name, _) => Some(name),
                                    _ => None,
                                }
                            } else {
                                None
                            };
                            let mut closed = Vec::new();
                            self.tabs.update(|tabs| {
                                tabs.retain(|t| {
                                    let keep = &t.host != id
                                        || showing.as_ref() == Some(&t.session)
                                        || items.iter().any(|i| i.name == t.session.as_str());
                                    if !keep {
                                        closed.push(t.session.clone());
                                    }
                                    keep
                                })
                            });
                            for session in closed {
                                self.close_terminal(id, &session);
                            }
                            if current
                                && let Some(name) = self.pending_session.get_untracked()
                                && items.iter().any(|i| i.name == name.as_str())
                            {
                                self.pending_session.set(None);
                                self.screen.set(Screen::Terminal(name, None));
                            }
                        }
                    }
                    Event::Error { ch: None, msg, .. } if current => self.error.set(msg.clone()),
                    _ => {}
                }
                self.with_pool(id, |pool| {
                    pool.on_event(&event, current.then_some(self.banner))
                });
            }
            Notice::Down => {
                self.links.update(|links| {
                    if let Some(link) = links.get_mut(id) {
                        link.connected = false;
                    }
                });
                if current {
                    self.connected.set(false);
                }
            }
            Notice::Error(e) => {
                if current {
                    self.error.set(e);
                }
            }
            Notice::Stopped(code, reason) => {
                self.links.update(|links| {
                    if let Some(link) = links.get_mut(id) {
                        link.stopped = true;
                    }
                });
                if current {
                    self.stopped.set(true);
                    self.error.set(reason);
                }
                if code == 4401 {
                    self.disconnect_all();
                    self.screen.set(Screen::SignIn);
                    self.me.set(None);
                } else if code == 4403 {
                    self.drop_machine(id);
                    if current {
                        self.machines();
                    } else {
                        self.refresh_hosts();
                    }
                }
            }
        }
    }
}

/// Mount the authenticated application after checking the session cookie.
#[component]
pub fn App() -> impl IntoView {
    let (pending_host, pending_session) = pwa::launch_target();
    let model = Model {
        me: RwSignal::new(None),
        hosts: RwSignal::new(Vec::new()),
        current: RwSignal::new(None),
        pending_host: RwSignal::new(pending_host),
        screen: RwSignal::new(Screen::Loading),
        machines: StoredValue::new_local(HashMap::new()),
        links: RwSignal::new(HashMap::new()),
        used_at: StoredValue::new_local(0),
        banner: RwSignal::new(String::new()),
        ctrl: RwSignal::new(false),
        paused: RwSignal::new(false),
        pause_timer: StoredValue::new_local(None),
        host: RwSignal::new(String::new()),
        connected: RwSignal::new(false),
        agent: RwSignal::new(AgentStatus::default()),
        stopped: RwSignal::new(false),
        error: RwSignal::new(String::new()),
        sessions: RwSignal::new(Vec::new()),
        presets: RwSignal::new(Vec::new()),
        pending_session: RwSignal::new(pending_session),
        tabs: RwSignal::new(tabs::load()),
    };
    Effect::new(move |_| model.tabs.with(|t| tabs::save(t)));
    let listener =
        StoredValue::new_local(pwa::listen(move |host, session| model.target(host, session)).ok());
    on_cleanup(move || {
        listener.try_update_value(|value| *value = None);
    });
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = model.load().await {
            model.error.set(e);
            model.screen.set(Screen::SignIn);
        }
    });
    on_cleanup(move || {
        model.disconnect_all();
        if let Some(timer) = model.pause_timer.get_value() {
            timer.clear();
        }
    });
    view! {
        <main class="app">
            <p class="error" role="alert" hidden=move || model.error.get().is_empty()>{move || model.error.get()}</p>
            {move || match model.screen.get() {
                Screen::Loading => view! { <p class="signin">"Checking sign-in…"</p> }.into_any(),
                Screen::SignIn => view! { <SignIn model /> }.into_any(),
                Screen::Machines => view! { <hosts::Machines model /> }.into_any(),
                Screen::Account => view! { <account::Account model /> }.into_any(),
                Screen::AddMachine => view! { <hosts::Pair model /> }.into_any(),
                Screen::MachineSettings(id) => view! { <hosts::Settings model id /> }.into_any(),
                Screen::Sessions => view! { <Sessions model /> }.into_any(),
                Screen::Terminal(name, create) => view! { <TerminalView model name create /> }.into_any(),
            }}
        </main>
    }
}

pub(crate) async fn authenticate(
    enroll: Option<(String, String, Option<String>)>,
) -> Result<(), String> {
    let create = enroll.is_some();
    let kind = if create { "register" } else { "login" };
    let body = enroll.as_ref().map(|(code, _, _)| json!({"code": code}));
    let (status, options) =
        api::request("POST", &format!("/api/passkeys/{kind}/options"), body).await?;
    if status != 200 {
        return Err(api::failure(status, &options));
    }
    let flow = options.get("flow").ok_or("Missing passkey flow")?;
    let public_key = options
        .get("publicKey")
        .or_else(|| options.get("options"))
        .ok_or("Missing passkey options")?;
    let credential = passkey::credential(public_key, create).await?;
    let mut body = json!({"flow": flow, "credential": credential});
    if let Some((_, label, account_name)) = enroll {
        if let Some(name) = account_name {
            body["account_name"] = name.into();
        }
        body["label"] = label.into();
    }
    let (status, result) =
        api::request("POST", &format!("/api/passkeys/{kind}/verify"), Some(body)).await?;
    if status == if create { 201 } else { 204 } {
        Ok(())
    } else {
        Err(api::failure(status, &result))
    }
}

#[component]
fn SignIn(model: Model) -> impl IntoView {
    let enroll = RwSignal::new(false);
    let invited = RwSignal::new(false);
    let account_name = RwSignal::new(String::new());
    let code = RwSignal::new(String::new());
    let label = RwSignal::new("This device".to_string());
    let busy = RwSignal::new(false);
    let run = move |registration: bool| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        model.error.set(String::new());
        let fields = registration.then(|| (code.get_untracked(), label.get_untracked()));
        wasm_bindgen_futures::spawn_local(async move {
            let result = async {
                let fields = if let Some((code, label)) = fields {
                    let (status, body) =
                        api::request("POST", "/api/codes/check", Some(json!({"code": code})))
                            .await?;
                    if status == 404 {
                        return Err("That code is wrong or has expired.".into());
                    }
                    if status != 200 {
                        return Err(api::failure(status, &body));
                    }
                    let check: CodeCheck =
                        serde_json::from_value(body).map_err(|_| "Invalid code response")?;
                    let name = if check.kind == CodeKind::Invite {
                        invited.set(true);
                        if account_name.get_untracked().trim().is_empty() {
                            return Ok(false);
                        }
                        Some(account_name.get_untracked())
                    } else {
                        invited.set(false);
                        None
                    };
                    Some((code, label, name))
                } else {
                    None
                };
                authenticate(fields).await?;
                model.load().await?;
                Ok(true)
            }
            .await;
            busy.try_set(false);
            match result {
                Ok(_) => {}
                Err(e) => model.error.set(e),
            }
        });
    };
    view! {
        <section class="signin">
            <h1>"anila"</h1><p class="muted">"Your tmux sessions, from any browser."</p>
            <button class="primary" disabled=move || busy.get() on:click=move |_| run(false)>"Sign in with passkey"</button>
            <button disabled=move || busy.get() on:click=move |_| enroll.update(|v| *v = !*v)>"First time on this device?"</button>
            <Show when=move || enroll.get()>
                <form on:submit=move |ev| { ev.prevent_default(); run(true); }>
                    <p class="muted small">"Get an enroll code by running "<code>"anila-service enroll"</code>" on the host machine."</p>
                    <label>"Enroll code"<input required prop:value=move || code.get() on:input=move |ev| code.set(event_target_value(&ev)) /></label>
                    <label>"Device label"<input required prop:value=move || label.get() on:input=move |ev| label.set(event_target_value(&ev)) /></label>
                    <label hidden=move || !invited.get()>"Account name"<input name="account_name" prop:value=move || account_name.get() on:input=move |ev| account_name.set(event_target_value(&ev)) /></label>
                    <p hidden=move || !invited.get()>"You were invited: name your account."</p>
                    <button class="primary" type="submit" disabled=move || busy.get()>"Enroll this device"</button>
                </form>
            </Show>
            <p class="muted" role="status">{move || if busy.get() { "Waiting for passkey…" } else { "" }}</p>
        </section>
    }
}

fn activity(seconds: i64) -> String {
    let elapsed = ((js_sys::Date::now() / 1000.0) as i64)
        .saturating_sub(seconds)
        .max(0);
    if elapsed < 60 {
        "active just now".into()
    } else if elapsed < 3600 {
        format!("active {} min ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("active {} h ago", elapsed / 3600)
    } else {
        format!("active {} d ago", elapsed / 86400)
    }
}

#[component]
fn Sessions(model: Model) -> impl IntoView {
    let creating = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let preset = RwSignal::new(String::new());
    let validation = RwSignal::new(String::new());
    view! {
        <section class="sessions">
            <TabBar model />
            <div class="bar"><span class="host">{move || model.host.get()}</span><button on:click=move |_| model.list()>"Refresh"</button></div>
            <pwa::Notifications />
            <ul class="list">
                {move || model.sessions.get().into_iter().map(|row| {
                    let name = row.name.clone();
                    view! { <li><button class="row" on:click=move |_| match SessionName::new(name.clone()) {
                        Ok(name) => { model.error.set(String::new()); model.screen.set(Screen::Terminal(name, None)); }
                        Err(e) => model.error.set(e.to_string()),
                    }><span class="name">{row.name}</span><span class="badge">{row.cmd}</span>
                    <span class="meta">{format!("{} windows · {} · {} attached", row.windows, activity(row.activity), row.attached)}</span></button></li> }
                }).collect_view()}
            </ul>
            <p class="muted" hidden=move || !model.sessions.get().is_empty()>"No sessions loaded. Refresh when the host is online, or create a session."</p>
            <button class="new" on:click=move |_| creating.update(|v| *v = !*v)>"New session"</button>
            <Show when=move || creating.get()>
                <form on:submit=move |ev| {
                    ev.prevent_default();
                    let name = match SessionName::new(name.get_untracked()) { Ok(name) => name, Err(_) => { validation.set("Use 1–32 letters (A–Z, a–z), digits, underscores, or hyphens.".into()); return; } };
                    let presets = model.presets.get_untracked();
                    let selected = preset.get_untracked();
                    let choice = presets.iter().find(|p| p.id == selected).or_else(|| presets.first());
                    let Some(choice) = choice else { validation.set("Refresh to load the host's presets first.".into()); return; };
                    model.error.set(String::new());
                    model.screen.set(Screen::Terminal(name, Some(Create { preset: choice.id.clone() })));
                }>
                    <label>"Session name"<input prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) /></label>
                    <label>"Preset"<select on:change=move |ev| preset.set(event_target_value(&ev))>
                        {move || model.presets.get().into_iter().map(|p| view! { <option value=p.id>{p.label}</option> }).collect_view()}
                    </select></label>
                    <p class="error" role="alert">{move || validation.get()}</p>
                    <button class="primary" type="submit">"Create session"</button>
                </form>
            </Show>
        </section>
    }
}

const KEYS: &[(&str, &str)] = &[
    ("Esc", "\x1b"),
    ("Tab", "\t"),
    ("⌃b", "\x02"),
    ("←", "\x1b[D"),
    ("↑", "\x1b[A"),
    ("↓", "\x1b[B"),
    ("→", "\x1b[C"),
];

#[component]
fn TerminalView(model: Model, name: SessionName, create: Option<Create>) -> impl IntoView {
    let host = NodeRef::<leptos::html::Div>::new();
    let mounted = StoredValue::new_local(false);
    let banner = model.banner;
    let ctrl = model.ctrl;
    let paused = model.paused;
    let machine = StoredValue::new_local(model.current.get_untracked());
    let send = move |data: &str| {
        let Some(id) = machine.get_value() else {
            return;
        };
        let ch = model.with_pool(&id, |pool| pool.shown_ch()).flatten();
        if let Some(ch) = ch {
            if let Some(conn) = model.connection(&id) {
                model.send(&conn, ch, data);
            }
            model.focus(&id);
        }
    };
    Effect::new(move |_| {
        let Some(div) = host.get() else {
            return;
        };
        if mounted.get_value() {
            return;
        }
        mounted.set_value(true);
        ctrl.set(false);
        paused.set(false);
        let Some(id) = machine.get_value() else {
            return;
        };
        let Some(conn) = model.connection(&id) else {
            return;
        };
        let result = model
            .with_pool(&id, |pool| {
                pool.mount(&div)
                    .and_then(|()| pool.show(model, &conn, name.clone(), create.clone()))
            })
            .unwrap_or_else(|| Err("Machine unavailable".into()));
        // Focus outside the pool borrow: focusing can emit input at once.
        model.focus(&id);
        match result {
            Ok(()) => {
                if let Some(host) = model.current.get_untracked() {
                    let tab = OpenTab {
                        host,
                        host_name: model.host.get_untracked(),
                        session: name.clone(),
                    };
                    model.tabs.update(|tabs| tabs::open(tabs, tab));
                }
            }
            Err(e) => banner.set(e),
        }
    });
    let listener = window_event_listener(leptos::ev::resize, move |_| {
        if let Some(id) = machine.get_value()
            && let Some(conn) = model.connection(&id)
        {
            model.with_pool(&id, |pool| pool.fit_shown(&conn));
        }
    });
    on_cleanup(move || {
        listener.remove();
        if let Some(id) = machine.get_value() {
            model.with_pool(&id, |pool| pool.detach());
        }
    });
    view! {
        <section class="terminal">
            <TabBar model />
            <Show when=move || !banner.get().is_empty()><div class="error" role="alert">{move || banner.get()}<button on:click=move |_| model.sessions()>"Back to sessions"</button></div></Show>
            <p class="muted small" role="status" hidden=move || !paused.get()>"Input paused while reconnecting"</p>
            <div class="terms-mount" node_ref=host></div>
            <nav class="keys" aria-label="Terminal keys">
                <button class:primary=move || ctrl.get() aria-pressed=move || ctrl.get().to_string() on:click=move |_| { ctrl.update(|v| *v = !*v); if let Some(id) = machine.get_value() { model.focus(&id); } }>"Ctrl"</button>
                {KEYS.iter().map(|(label, bytes)| view! { <button on:click=move |_| send(bytes)>{*label}</button> }).collect_view()}
            </nav>
        </section>
    }
}
