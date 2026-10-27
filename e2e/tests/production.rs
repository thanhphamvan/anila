//! Emulated-phone check against a live deployment: headless Chromium as an
//! iPhone-sized touch device on a throttled 4G-like network, against the real
//! worker on Cloudflare and the real service on this machine.
//!
//! This is the closest automated stand-in for `docs/phone-checklist.md`; it
//! does not replace a real phone radio. It registers one passkey labelled
//! "emulated-phone check <pid>" from a throwaway virtual authenticator,
//! creates and then kills its own tmux session `anila-e2e-phone`, and deletes
//! that passkey at the end.
//!
//! Skipped unless `ANILA_PROD_ORIGIN` is set and `anila-service run` is
//! connected to that origin with the default config:
//! `ANILA_PROD_ORIGIN=https://anila.<sub>.workers.dev cargo test -p anila-e2e --test production -- --nocapture`

use std::process::Command;
use std::time::Duration;

use anila_e2e::browser::{Browser, Page};
use anila_e2e::util::workspace_root;

const T: Duration = Duration::from_secs(40);
const SESSION: &str = "anila-e2e-phone";

fn step(name: &str) {
    eprintln!("\n=== {name}");
}

fn tmux(args: &[&str]) -> String {
    let out = Command::new("tmux").args(args).output().expect("tmux runs");
    String::from_utf8_lossy(&out.stdout).to_string()
}

struct TestSession;

impl Drop for TestSession {
    fn drop(&mut self) {
        // Only ever remove the session this test created.
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &format!("={SESSION}")])
            .output();
    }
}

async fn click_button(page: &Page, label: &str) {
    let found = page
        .eval(&format!(
            "(() => {{ const b = [...document.querySelectorAll('button')].find(b => b.textContent.includes({label:?}));
                       if (!b) return false; b.setAttribute('data-e2e', 'target'); return true; }})()"
        ))
        .await
        .unwrap();
    assert_eq!(
        found,
        serde_json::json!(true),
        "a button labelled {label:?}"
    );
    page.click("[data-e2e=target]").await.unwrap();
    page.eval("document.querySelector('[data-e2e=target]')?.removeAttribute('data-e2e')")
        .await
        .unwrap();
}

async fn wait(page: &Page, js: &str, what: &str) {
    if let Err(e) = page.wait_for(js, T).await {
        panic!("{what}: {e}\nconsole: {:#?}", page.console());
    }
}

#[test]
fn emulated_phone_against_production() {
    let Some(origin) = std::env::var("ANILA_PROD_ORIGIN")
        .ok()
        .filter(|o| o.starts_with("https://"))
    else {
        eprintln!("ANILA_PROD_ORIGIN not set, skipping");
        return;
    };
    let bin = workspace_root().join("target/release/anila-service");
    assert!(
        bin.exists(),
        "build it first: cargo build --release -p anila-service"
    );

    let _cleanup = TestSession;
    tmux(&["new-session", "-d", "-s", SESSION, "-c", "/tmp", "cat"]);
    let enroll = Command::new(&bin).arg("enroll").output().unwrap();
    assert!(
        enroll.status.success(),
        "enroll: {}",
        String::from_utf8_lossy(&enroll.stderr)
    );
    let code = String::from_utf8_lossy(&enroll.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("Enroll code: ").map(str::to_string))
        .expect("enroll prints a code");
    let label = format!("emulated-phone check {}", std::process::id());

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let browser = Browser::launch().await.expect("a Chromium browser starts");
        let page = browser.new_page().await.unwrap();
        page.emulate_phone().await.unwrap();
        // Poor 4G: ~75 ms added latency, 125 KB/s down, 60 KB/s up.
        page.set_network(75, 125_000, 60_000).await.unwrap();
        page.add_virtual_authenticator().await.unwrap();

        step("enroll on the live site with a passkey");
        page.goto(&origin).await.unwrap();
        wait(&page, "document.querySelector('section.signin button.primary') !== null", "sign-in screen").await;
        click_button(&page, "First time on this device?").await;
        wait(&page, "document.querySelector('form') !== null", "enroll form").await;
        page.fill("form label:nth-of-type(1) input", &code).await.unwrap();
        page.fill("form label:nth-of-type(2) input", &label).await.unwrap();
        click_button(&page, "Enroll this device").await;
        wait(&page, &format!("[...document.querySelectorAll('button.row .name')].some(e => e.textContent === {SESSION:?})"), "session list from the host").await;
        assert_eq!(page.text(".pill").await.unwrap(), "Live");

        step("open the session and type over throttled 4G");
        page.eval(&format!(
            "[...document.querySelectorAll('button.row')].find(b => b.querySelector('.name').textContent === {SESSION:?}).setAttribute('data-e2e','row')"
        ))
        .await
        .unwrap();
        page.click("[data-e2e=row]").await.unwrap();
        wait(&page, "document.querySelector('.xterm-helper-textarea') !== null", "terminal").await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        page.eval("document.querySelector('.xterm-helper-textarea').focus()").await.unwrap();
        page.insert_text("phone-check-1").await.unwrap();
        page.press("Enter").await.unwrap();
        wait(&page, "document.querySelector('.xterm-rows').innerText.includes('phone-check-1')", "typed text on screen").await;
        assert!(tmux(&["capture-pane", "-p", "-t", &format!("={SESSION}:")]).contains("phone-check-1"), "keys reached tmux");

        step("the phone drops off the network for 10 s");
        page.set_offline(true).await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        page.set_network(75, 125_000, 60_000).await.unwrap();
        wait(&page, "document.querySelector('.pill').textContent === 'Live'", "live again").await;
        page.eval("document.querySelector('.xterm-helper-textarea').focus()").await.unwrap();
        page.insert_text("phone-check-2").await.unwrap();
        page.press("Enter").await.unwrap();
        wait(&page, "document.querySelector('.xterm-rows').innerText.includes('phone-check-2')", "typing after the dropout").await;
        let pane = tmux(&["capture-pane", "-p", "-t", &format!("={SESSION}:")]);
        assert_eq!(pane.matches("phone-check-2").count(), 2, "typed once, echoed once by cat: no duplicate keystrokes\n{pane}");

        step("installable app and a reload on the live site");
        let display = page.eval("fetch('/manifest.webmanifest').then(r => r.json()).then(m => m.display)").await.unwrap();
        assert_eq!(display, serde_json::json!("standalone"));
        page.goto(&origin).await.unwrap();
        wait(&page, "navigator.serviceWorker.controller !== null", "service worker in control").await;
        wait(&page, "document.querySelectorAll('button.row').length > 0", "still signed in after a reload").await;

        step("delete this test's passkey");
        assert_eq!(page.delete_passkeys_labelled(&label).await.unwrap(), 1, "this test's passkey deleted");
        eprintln!("browser console: {:#?}", page.console());
    });
    eprintln!("\nemulated-phone production check passed");
}
