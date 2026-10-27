#[cfg(feature = "bench")]
mod bench;

mod account;
mod api;
mod app;
mod conn;
mod hosts;
mod passkey;
mod pool;
mod pwa;
mod tab;
mod tabs;
mod xterm;

fn main() {
    console_error_panic_hook::set_once();
    #[cfg(feature = "bench")]
    bench::install();
    pwa::register();
    leptos::mount::mount_to_body(app::App);
}
