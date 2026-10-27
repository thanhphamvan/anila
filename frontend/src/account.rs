//! Account passkeys, device enrollment, and administrator controls.

use crate::{
    api,
    app::{Model, Screen},
    hosts::{confirm, date, issue},
    pwa,
};
use anila_proto::api::AccountView;
use leptos::prelude::*;
use serde::Deserialize;
use wasm_bindgen_futures::spawn_local;

// The existing passkey endpoint predates the shared v0.0.2 API structs.
#[derive(Clone, Deserialize)]
struct Passkey {
    id: String,
    label: String,
    created_at: i64,
    last_used_at: Option<i64>,
}

fn load<T: serde::de::DeserializeOwned + Send + Sync + 'static>(
    model: Model,
    path: &'static str,
    signal: RwSignal<Vec<T>>,
) {
    spawn_local(async move {
        match api::read(path).await {
            Ok(items) => {
                signal.try_set(items);
            }
            Err(e) => model.error.set(e),
        }
    });
}

/// Account settings and, for administrators, account management.
#[component]
pub(crate) fn Account(model: Model) -> impl IntoView {
    let me = model.me.get_untracked();
    let admin = me.as_ref().is_some_and(|m| m.account.admin);
    let own_id = StoredValue::new(
        me.as_ref()
            .map(|m| m.account.id.clone())
            .unwrap_or_default(),
    );
    let name = me.map(|m| m.account.name).unwrap_or_default();
    let passkeys = RwSignal::new(Vec::<Passkey>::new());
    let accounts = RwSignal::new(Vec::<AccountView>::new());
    let device_code = RwSignal::new(String::new());
    let invite_code = RwSignal::new(String::new());
    load(model, "/api/passkeys", passkeys);
    if admin {
        load(model, "/api/admin/accounts", accounts);
    }
    view! {
        <section class="account">
            <button on:click=move |_| model.machines()>"‹ Machines"</button>
            <header class="bar"><h1>{name}</h1>{admin.then(|| view! { <span class="badge">"Administrator"</span> })}</header>
            <h2>"Passkeys"</h2>
            <ul class="passkeys">{move || passkeys.get().into_iter().map(|key| view! {
                <li class="passkey"><span>{key.label}</span>
                    <span class="muted small">{format!("Created {} · Last used {}", date(key.created_at), key.last_used_at.map(date).unwrap_or_else(|| "Never".into()))}</span>
                    <button on:click=move |_| {
                        let path = format!("/api/passkeys/{}", js_sys::encode_uri_component(&key.id));
                        spawn_local(async move {
                            match api::change("DELETE", &path, None, true).await {
                                Ok(_) => { model.error.set(String::new()); load(model, "/api/passkeys", passkeys); }
                                Err(e) => model.error.set(e),
                            }
                        });
                    }>"Delete"</button>
                </li>
            }).collect_view()}</ul>
            <button class="add-device" on:click=move |_| issue(model, "/api/device-codes".into(), device_code)>"Add another device"</button>
            <Show when=move || !device_code.get().is_empty()>
                <code class="device-code">{move || device_code.get()}</code>
                <p>"On the other device, open this site, choose \"First time on this device?\" and enter the code. It works once, for 10 minutes."</p>
            </Show>
            <pwa::Notifications />
            <button class="signout" on:click=move |_| spawn_local(async move {
                match api::change("POST", "/api/logout", None, false).await {
                    Ok(_) => {
                        model.disconnect_all();
                        model.me.set(None);
                        model.hosts.set(Vec::new());
                        model.current.set(None);
                        model.pending_session.set(None);
                        model.error.set(String::new());
                        model.screen.set(Screen::SignIn);
                    }
                    Err(e) => model.error.set(e),
                }
            })>"Sign out"</button>
            {admin.then(|| view! {
                <section class="admin">
                    <h2>"Administration"</h2>
                    <button class="invite" on:click=move |_| issue(model, "/api/admin/invites".into(), invite_code)>"Invite someone"</button>
                    <Show when=move || !invite_code.get().is_empty()>
                        <code class="invite-code">{move || invite_code.get()}</code><p>"Valid for 7 days, once."</p>
                    </Show>
                    <ul class="accounts">{move || accounts.get().into_iter().map(|account| {
                        let other = account.id != own_id.get_value();
                        view! {
                            <li class="account-row"><span>{account.name}</span>
                                {account.admin.then(|| view! { <span class="badge">"admin"</span> })}
                                <span class="muted small">{format!("{} machines · {} passkeys · Created {}", account.hosts, account.passkeys, date(account.created_at))}</span>
                                {other.then(|| view! { <button on:click=move |_| {
                                    if !confirm("Delete this account and its machines?") { return; }
                                    let path = format!("/api/admin/accounts/{}", js_sys::encode_uri_component(&account.id));
                                    spawn_local(async move {
                                        match api::change("DELETE", &path, None, true).await {
                                            Ok(_) => load(model, "/api/admin/accounts", accounts), Err(e) => model.error.set(e),
                                        }
                                    });
                                }>"Delete"</button> })}
                            </li>
                        }
                    }).collect_view()}</ul>
                </section>
            })}
        </section>
    }
}
