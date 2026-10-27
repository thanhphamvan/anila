//! Passkey-only integration points for the shared frontend.
//!
//! Downstream deployments can supply their own adapter without forking the
//! terminal, account, navigation or PWA implementation. This adapter does
//! not discover or contact any external identity provider.

use crate::{account, app::Model};
use anila_proto::api::{AccountView, Me};
use leptos::prelude::*;

#[derive(Clone, Copy)]
pub(crate) struct State;

impl State {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn initialize(self, _model: Model) {}
}

pub(crate) fn terminal_allowed(_me: &Me) -> bool {
    true
}

pub(crate) fn passkeys_allowed(_me: Option<&Me>) -> bool {
    true
}

pub(crate) fn after_load(_model: Model, _loaded: bool) {}

pub(crate) fn restricted(model: Model) -> AnyView {
    view! {
        <section class="signin">
            <h1>"anila"</h1>
            <p>"This account cannot access terminals."</p>
            <button on:click=move |_| account::sign_out(model)>"Sign out"</button>
        </section>
    }
    .into_any()
}

pub(crate) fn sign_in_button(_model: Model, _busy: RwSignal<bool>) -> AnyView {
    ().into_any()
}

pub(crate) fn account_section(_model: Model) -> AnyView {
    ().into_any()
}

pub(crate) fn account_suffix(_account: &AccountView) -> String {
    String::new()
}

pub(crate) async fn reauthenticate() -> Result<(), String> {
    crate::app::authenticate(None).await
}
