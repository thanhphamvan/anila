//! Real push delivery against a live deployment: a desktop Chromium browser
//! subscribes through the web app's Notifications control, so its endpoint
//! belongs to a real push service (Google's or Microsoft's); then
//! `anila-service notify` on this machine goes service → live worker → that
//! push service → the browser's service worker, which must show it.
//!
//! Registers one passkey labelled "push check <pid>" from a throwaway virtual
//! authenticator, then unsubscribes and deletes that passkey at the end.
//! Skipped unless `ANILA_PROD_ORIGIN` is set (see `production.rs`):
//! `ANILA_PROD_ORIGIN=https://anila.<sub>.workers.dev cargo test -p anila-e2e --test production_push -- --nocapture`

use std::process::Command;
use std::time::Duration;

use anila_e2e::browser::{Browser, Page};
use anila_e2e::util::workspace_root;

const T: Duration = Duration::from_secs(40);

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
fn real_push_delivery_against_production() {
    let Some(origin) = std::env::var("ANILA_PROD_ORIGIN")
        .ok()
        .filter(|o| o.starts_with("https://"))
    else {
        eprintln!("ANILA_PROD_ORIGIN not set, skipping");
        return;
    };
    let bin = workspace_root().join("target/release/anila-service");
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
    let title = format!("anila push check {}", std::process::id());
    let label = format!("push check {}", std::process::id());

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let browser = Browser::launch().await.expect("a Chromium browser starts");
        let page = browser.new_page().await.unwrap();
        page.add_virtual_authenticator().await.unwrap();
        page.grant_notifications(&origin).await.unwrap();

        page.goto(&origin).await.unwrap();
        wait(&page, "document.querySelector('section.signin button.primary') !== null", "sign-in screen").await;
        click_button(&page, "First time on this device?").await;
        wait(&page, "document.querySelector('form') !== null", "enroll form").await;
        page.fill("form label:nth-of-type(1) input", &code).await.unwrap();
        page.fill("form label:nth-of-type(2) input", &label).await.unwrap();
        click_button(&page, "Enroll this device").await;
        wait(&page, "[...document.querySelectorAll('button')].some(b => b.textContent.includes('Notifications: Off'))", "notifications control").await;

        eprintln!("\n=== subscribe through the app");
        click_button(&page, "Notifications: Off").await;
        wait(&page, "[...document.querySelectorAll('button')].some(b => b.textContent.includes('Notifications: On'))", "notifications turned on").await;
        let host = page
            .eval("navigator.serviceWorker.ready.then(r => r.pushManager.getSubscription()).then(s => s && new URL(s.endpoint).host)")
            .await
            .unwrap();
        eprintln!("push service: {host}");
        assert!(host.as_str().is_some_and(|h| !h.contains("localhost")), "a real push service endpoint");

        eprintln!("\n=== anila-service notify → worker → push service → browser");
        let out = Command::new(&bin).args(["notify", "--title", &title, "--body", "delivered by a real push service"]).output().unwrap();
        let out = String::from_utf8_lossy(&out.stdout).to_string();
        eprintln!("notify: {}", out.trim());
        assert!(out.contains("Sent to 1 device"), "the worker accepted the push: {out}");
        wait(
            &page,
            &format!("navigator.serviceWorker.ready.then(r => r.getNotifications()).then(ns => ns.some(n => n.title.endsWith({title:?})))"),
            "the service worker showed the notification",
        )
        .await;

        eprintln!("\n=== clean up");
        click_button(&page, "Notifications: On").await;
        wait(&page, "[...document.querySelectorAll('button')].some(b => b.textContent.includes('Notifications: Off'))", "notifications turned off").await;
        assert_eq!(page.delete_passkeys_labelled(&label).await.unwrap(), 1, "this test's passkey deleted");
    });
    eprintln!("\nreal push delivery passed");
}
