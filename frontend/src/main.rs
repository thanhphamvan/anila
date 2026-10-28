#[cfg(feature = "bench")]
#[path = "shared/bench.rs"]
mod bench;

#[path = "shared/account.rs"]
mod account;
#[path = "shared/api.rs"]
mod api;
#[path = "shared/app.rs"]
mod app;
#[path = "shared/clipboard.rs"]
mod clipboard;
#[path = "shared/clipboard_payload.rs"]
mod clipboard_payload;
#[path = "shared/conn.rs"]
mod conn;
mod edition;
#[path = "shared/hosts.rs"]
mod hosts;
#[path = "shared/layout.rs"]
mod layout;
#[path = "shared/passkey.rs"]
mod passkey;
#[path = "shared/pool.rs"]
mod pool;
#[path = "shared/pwa.rs"]
mod pwa;
#[path = "shared/tab.rs"]
mod tab;
#[path = "shared/tabs.rs"]
mod tabs;
#[path = "shared/xterm.rs"]
mod xterm;

fn main() {
    console_error_panic_hook::set_once();
    #[cfg(feature = "bench")]
    bench::install();
    pwa::register();
    leptos::mount::mount_to_body(app::App);
}
