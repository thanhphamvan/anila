//! Machine selection, pairing, and owner settings.

use crate::{
    api,
    app::{Model, Screen},
    pwa,
};
use anila_proto::{
    HostId,
    api::{CodeIssued, HostView, Member, Role},
};
use leptos::prelude::*;
use serde_json::json;
use wasm_bindgen_futures::spawn_local;

pub(crate) fn last_host() -> Option<HostId> {
    web_sys::window()?
        .local_storage()
        .ok()
        .flatten()?
        .get_item("anila.lastHost")
        .ok()
        .flatten()
        .and_then(|id| HostId::new(id).ok())
}

pub(crate) fn remember_host(id: &HostId) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item("anila.lastHost", id.as_str());
    }
}

pub(crate) fn initial_host(
    hosts: &[HostView],
    target: Option<&HostId>,
    last: Option<&HostId>,
) -> Option<HostId> {
    target
        .and_then(|id| hosts.iter().find(|h| &h.id == id))
        .or_else(|| {
            if hosts.len() == 1 {
                hosts.first()
            } else {
                None
            }
        })
        .or_else(|| last.and_then(|id| hosts.iter().find(|h| &h.id == id)))
        .map(|h| h.id.clone())
}

pub(crate) fn confirm(message: &str) -> bool {
    web_sys::window().is_some_and(|w| w.confirm_with_message(message).unwrap_or(false))
}

pub(crate) fn date(seconds: i64) -> String {
    let date = js_sys::Date::new(&((seconds as f64) * 1000.0).into());
    format!(
        "{:04}-{:02}-{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date()
    )
}

pub(crate) fn poll(seconds: u64, callback: impl Fn() + 'static) {
    if let Ok(timer) = leptos::leptos_dom::helpers::set_interval_with_handle(
        callback,
        std::time::Duration::from_secs(seconds),
    ) {
        on_cleanup(move || timer.clear());
    }
}

pub(crate) fn issue(model: Model, path: String, code: RwSignal<String>) {
    spawn_local(async move {
        let result = async {
            let body = api::change("POST", &path, None, true).await?;
            serde_json::from_value::<CodeIssued>(body)
                .map_err(|_| "Invalid code response".to_string())
        }
        .await;
        match result {
            Ok(issued) => {
                code.try_set(issued.code);
            }
            Err(e) => model.error.set(e),
        }
    });
}

/// Machines available to the signed-in account.
#[component]
pub(crate) fn Machines(model: Model) -> impl IntoView {
    let joining = RwSignal::new(false);
    let code = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    poll(10, move || model.refresh_hosts());
    view! {
        <section class="machines">
            <header class="bar"><span class="host">{move || model.me.get().map(|m| m.account.name).unwrap_or_default()}</span><button class="account" on:click=move |_| model.screen.set(Screen::Account)>"Account"</button></header>
            <ul class="machines">
                {move || model.hosts.get().into_iter().map(|host| {
                    let id = host.id.clone();
                    let settings_id = host.id.clone();
                    let owner = host.role == Role::Owner;
                    view! { <li>
                        <button class="machine" on:click=move |_| { model.pending_session.set(None); model.connect(id.clone()); }>
                            <span class="dot" class:on=host.online aria-label=if host.online { "Online" } else { "Offline" }></span>
                            <span class="name">{host.name.to_string()}</span>
                            {(!owner).then(|| view! { <span class="shared">{format!("shared by {}", host.owner_name)}</span> })}
                        </button>
                        <button class="settings" on:click=move |_| {
                            if owner { model.screen.set(Screen::MachineSettings(settings_id.clone())); }
                            else if confirm("Leave this shared machine?") {
                                let path = format!("/api/hosts/{settings_id}/members/me");
                                spawn_local(async move {
                                    match api::change("DELETE", &path, None, false).await {
                                        Ok(_) => model.refresh_hosts(), Err(e) => model.error.set(e),
                                    }
                                });
                            }
                        }>{if owner { "Settings" } else { "Leave" }}</button>
                    </li> }
                }).collect_view()}
            </ul>
            <p class="empty" hidden=move || !model.hosts.get().is_empty()>"No machines yet. Add one, or join a machine someone shared with you."</p>
            <button class="add-machine" on:click=move |_| model.screen.set(Screen::AddMachine)>"Add machine"</button>
            <button class="join" on:click=move |_| joining.update(|v| *v = !*v)>"Join a shared machine"</button>
            <Show when=move || joining.get()>
                <form on:submit=move |ev| {
                    ev.prevent_default();
                    if busy.get_untracked() { return; }
                    busy.set(true);
                    let body = json!({"code": code.get_untracked()});
                    spawn_local(async move {
                        match api::change("POST", "/api/share/redeem", Some(body), false).await {
                            Ok(_) => { model.error.set(String::new()); model.refresh_hosts(); joining.try_set(false); }
                            Err(e) => model.error.set(e),
                        }
                        busy.try_set(false);
                    });
                }>
                    <label>"Share code"<input name="share_code" required on:input=move |ev| code.set(event_target_value(&ev)) /></label>
                    <button type="submit" disabled=move || busy.get()>"Join"</button>
                </form>
            </Show>
        </section>
    }
}

/// Issue a pairing code and wait for a newly registered machine.
#[component]
pub(crate) fn Pair(model: Model) -> impl IntoView {
    let before = model.hosts.get_untracked();
    let code = RwSignal::new(String::new());
    let added = RwSignal::new(None::<HostView>);
    let fetching = RwSignal::new(false);
    issue(model, "/api/pair-codes".into(), code);
    poll(3, move || {
        if fetching.get_untracked() || added.get_untracked().is_some() {
            return;
        }
        fetching.set(true);
        let before = before.clone();
        spawn_local(async move {
            match api::read::<Vec<HostView>>("/api/hosts").await {
                Ok(hosts) => {
                    let new = hosts
                        .iter()
                        .find(|h| !before.iter().any(|old| old.id == h.id))
                        .cloned();
                    added.try_set(new);
                    model.hosts.set(hosts);
                }
                Err(e) => model.error.set(e),
            }
            fetching.try_set(false);
        });
    });
    let command = move || {
        format!(
            "anila-service pair {} --worker {}",
            code.get(),
            pwa::origin()
        )
    };
    view! {
        <section class="pair">
            <button on:click=move |_| model.machines()>"‹ Machines"</button>
            <h1>"Add machine"</h1>
            <code class="pair-code">{move || code.get()}</code>
            <code class="pair-command">{command}</code>
            <button disabled=move || code.get().is_empty() on:click=move |_| pwa::copy(command())>"Copy"</button>
            <p>"Run this on the machine you want to add. The code works once, for 10 minutes."</p>
            {move || added.get().map(|host| view! {
                <p class="paired">{format!("Added {}.", host.name)}</p>
                <button on:click=move |_| model.connect(host.id.clone())>"Open it"</button>
            })}
        </section>
    }
}

fn load_members(model: Model, path: String, members: RwSignal<Vec<Member>>) {
    spawn_local(async move {
        match api::read(&path).await {
            Ok(value) => {
                members.try_set(value);
            }
            Err(e) => model.error.set(e),
        }
    });
}

/// Owner controls for the machine name, sharing, and removal.
#[component]
pub(crate) fn Settings(model: Model, id: HostId) -> impl IntoView {
    let host = model.hosts.get_untracked().into_iter().find(|h| h.id == id);
    let name = RwSignal::new(host.map(|h| h.name.to_string()).unwrap_or_default());
    let path = StoredValue::new(format!("/api/hosts/{id}"));
    let members = RwSignal::new(Vec::<Member>::new());
    let code = RwSignal::new(String::new());
    load_members(model, format!("{}/members", path.get_value()), members);
    view! {
        <section class="machine-settings">
            <button on:click=move |_| model.machines()>"‹ Machines"</button>
            <h1>"Machine settings"</h1>
            <form on:submit=move |ev| {
                ev.prevent_default();
                let Ok(name) = HostId::new(name.get_untracked()) else { model.error.set("Use lowercase letters, digits and dashes (up to 32).".into()); return; };
                let path = path.get_value();
                spawn_local(async move {
                    match api::change("PATCH", &path, Some(json!({"name": name})), false).await {
                        Ok(_) => { model.error.set(String::new()); model.refresh_hosts(); }
                        Err(e) => model.error.set(e),
                    }
                });
            }>
                <label>"Machine name"<input name="machine_name" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) /></label>
                <button type="submit">"Rename"</button>
            </form>
            <button class="share" on:click=move |_| issue(model, format!("{}/share-codes", path.get_value()), code)>"Share"</button>
            <Show when=move || !code.get().is_empty()>
                <code class="share-code">{move || code.get()}</code>
                <p>"Give this code to the person. They choose \"Join a shared machine\" after signing in. They will get a full shell on this machine. It works once, for 24 hours."</p>
            </Show>
            <h2>"Members"</h2>
            <ul class="members">{move || members.get().into_iter().map(|member| {
                view! { <li class="member"><span>{member.name}</span><span class="muted">{date(member.since)}</span>
                    <button on:click=move |_| {
                        let base = format!("{}/members", path.get_value());
                        let target = format!("{}/{}", base, js_sys::encode_uri_component(&member.account_id));
                        spawn_local(async move {
                            match api::change("DELETE", &target, None, false).await {
                                Ok(_) => load_members(model, base, members), Err(e) => model.error.set(e),
                            }
                        });
                    }>"Remove"</button>
                </li> }
            }).collect_view()}</ul>
            <p hidden=move || !members.get().is_empty()>"Not shared with anyone."</p>
            <button class="remove-machine" on:click=move |_| {
                if !confirm("Remove this machine?") { return; }
                let path = path.get_value();
                spawn_local(async move {
                    match api::change("DELETE", &path, None, true).await {
                        Ok(_) => model.machines(), Err(e) => model.error.set(e),
                    }
                });
            }>"Remove machine"</button>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str) -> HostView {
        HostView {
            id: HostId::new(id).expect("test id"),
            name: HostId::new(id).expect("test name"),
            role: Role::Owner,
            owner_name: "Owner".into(),
            online: false,
            since: None,
        }
    }

    #[test]
    fn startup_prefers_target_then_only_machine_then_last() {
        let a = host("a");
        let b = host("b");
        let hosts = [a.clone(), b.clone()];
        assert_eq!(
            initial_host(&hosts, Some(&a.id), Some(&b.id)),
            Some(a.id.clone())
        );
        assert_eq!(
            initial_host(&[a.clone()], None, Some(&b.id)),
            Some(a.id.clone())
        );
        assert_eq!(initial_host(&hosts, None, Some(&b.id)), Some(b.id.clone()));
        assert_eq!(initial_host(&hosts, None, None), None);
    }

    #[test]
    fn stale_machine_ids_are_never_selected() {
        let hosts = [host("a"), host("b")];
        let missing = HostId::new("removed").expect("test id");
        assert_eq!(initial_host(&hosts, Some(&missing), Some(&missing)), None);
        assert_eq!(
            initial_host(&hosts, Some(&missing), Some(&hosts[1].id)),
            Some(hosts[1].id.clone())
        );
        assert_eq!(initial_host(&[], Some(&missing), Some(&missing)), None);
    }
}
