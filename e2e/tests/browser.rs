//! F11: the real web app in a headless Chromium browser (Edge or Chrome),
//! with passkeys from the browser's virtual authenticator.
//! Needs `trunk` (it builds the frontend) and a Chromium browser.
//! Run with `cargo test -p anila-e2e --test browser -- --nocapture`.

mod common;

use std::process::Command;
use std::time::Duration;

use anila_e2e::browser::{Browser, Page};
use anila_e2e::tmux::TmuxServer;
use anila_e2e::util::workspace_root;
use common::*;

const T: Duration = Duration::from_secs(20);

fn step(name: &str) {
    eprintln!("\n=== {name}");
}

fn build_frontend() {
    let status = Command::new("trunk")
        .args(["build", "--release"])
        .current_dir(workspace_root().join("frontend"))
        .status()
        .expect("trunk is installed");
    assert!(status.success(), "trunk build --release");
}

/// Clicks the first button whose text contains `label`, with a real mouse click.
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

/// JS that is true when `session`'s terminal is the one on screen.
fn shown(session: &str) -> String {
    format!(
        "document.querySelector('nav.tabbar .tab.active .tab-name')?.textContent === {session:?} \
         && document.querySelector('.term:not([hidden])')?.dataset.session === {session:?} \
         && document.querySelector('.term:not([hidden]) .xterm-helper-textarea') !== null"
    )
}

async fn open_row(page: &Page, session: &str) {
    wait(page, &format!("[...document.querySelectorAll('button.row .name')].some(e => e.textContent === {session:?})"), "the session row").await;
    page.eval(&format!("[...document.querySelectorAll('button.row')].find(b => b.querySelector('.name').textContent === {session:?}).setAttribute('data-e2e','row')"))
        .await
        .unwrap();
    page.click("[data-e2e=row]").await.unwrap();
    page.eval("document.querySelector('[data-e2e=row]')?.removeAttribute('data-e2e')")
        .await
        .unwrap();
}

async fn click_tab(page: &Page, session: &str) {
    page.eval(&format!("[...document.querySelectorAll('nav.tabbar .tab')].find(t => t.querySelector('.tab-name').textContent === {session:?}).querySelector('.tab-open').setAttribute('data-e2e','tab')"))
        .await
        .unwrap();
    page.click("[data-e2e=tab]").await.unwrap();
    page.eval("document.querySelector('[data-e2e=tab]')?.removeAttribute('data-e2e')")
        .await
        .unwrap();
}

async fn type_into(page: &Page, text: &str) {
    page.eval("document.querySelector('.term:not([hidden]) .xterm-helper-textarea').focus()")
        .await
        .unwrap();
    page.insert_text(text).await.unwrap();
    page.press("Enter").await.unwrap();
}

async fn wait(page: &Page, js: &str, what: &str) {
    if let Err(e) = page.wait_for(js, T).await {
        panic!("{what}: {e}\nconsole: {:#?}", page.console());
    }
}

#[test]
fn real_browser() {
    if std::env::var_os("ANILA_SKIP_BROWSER").is_some() {
        eprintln!("ANILA_SKIP_BROWSER set, skipping");
        return;
    }
    build_frontend();
    let worker = start_worker(false);
    let tmux = TmuxServer::new("e2e-browser");
    tmux.new_session("work", "cat");
    let mut service1 = Some(start_service(&worker.http_base(), &tmux));
    let tmux2 = TmuxServer::new("e2e-browser2");
    tmux2.new_session("two", "cat");
    tmux2.new_session("three", "cat");
    for i in 1..=8 {
        tmux2.new_session(&format!("s{i}"), "cat");
    }
    let code = enroll_code(&worker.http_base(), &tmux);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let browser = Browser::launch().await.expect("a Chromium browser starts");
        let page = browser.new_page().await.unwrap();
        page.add_virtual_authenticator().await.unwrap();
        let origin = worker.origin();

        step("F11 enroll this device with a passkey");
        page.goto(&origin).await.unwrap();
        wait(&page, "document.querySelector('section.signin button.primary') !== null", "sign-in screen").await;
        click_button(&page, "First time on this device?").await;
        wait(&page, "document.querySelector('form') !== null", "enroll form").await;
        page.fill("form label:nth-of-type(1) input", &code).await.unwrap();
        page.fill("form label:nth-of-type(2) input", "headless").await.unwrap();
        click_button(&page, "Enroll this device").await;
        wait(&page, "[...document.querySelectorAll('button.row .name')].some(e => e.textContent === 'work')", "session list shows work").await;
        assert_eq!(page.text(".pill").await.unwrap(), "Live");

        step("F11 open a session and type");
        page.click("button.row").await.unwrap();
        wait(&page, "document.querySelector('.xterm-helper-textarea') !== null", "terminal").await;
        assert!(anila_e2e::util::wait_until(Duration::from_secs(10), || tmux.clients().len() == 1), "browser attached a tmux client");
        page.eval("document.querySelector('.xterm-helper-textarea').focus()").await.unwrap();
        page.insert_text("from-the-browser").await.unwrap();
        page.press("Enter").await.unwrap();
        wait(&page, "document.querySelector('.xterm-rows').innerText.includes('from-the-browser')", "typed text on screen").await;
        assert!(tmux.capture("work").contains("from-the-browser"), "the keys reached tmux");

        step("F11 the browser goes offline and comes back");
        page.set_offline(true).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        // Chromium's offline emulation does not close open websockets; the
        // client's ping and dead-link detection must notice within ~25 s.
        let noticed = page
            .wait_for("document.querySelector('.pill').textContent.includes('Reconnecting')", Duration::from_secs(40))
            .await
            .is_ok();
        eprintln!("offline noticed by the app: {noticed}");
        page.set_offline(false).await.unwrap();
        page.wait_for("document.querySelector('.pill').textContent === 'Live'", Duration::from_secs(40)).await.expect("live again");
        page.eval("document.querySelector('.xterm-helper-textarea').focus()").await.unwrap();
        page.insert_text("after-offline").await.unwrap();
        page.press("Enter").await.unwrap();
        wait(&page, "document.querySelector('.xterm-rows').innerText.includes('after-offline')", "typing after reconnect").await;

        step("F11 a reload keeps the session; cleared cookies need the passkey");
        page.goto(&origin).await.unwrap();
        wait(&page, "document.querySelectorAll('button.row').length > 0", "still signed in after reload").await;

        step("F11 installable: manifest, service worker, offline shell, notifications control");
        let display = page
            .eval("fetch('/manifest.webmanifest').then(r => r.json()).then(m => m.display)")
            .await
            .unwrap();
        assert_eq!(display, serde_json::json!("standalone"), "manifest is served");
        wait(&page, "navigator.serviceWorker.controller !== null", "service worker controls the page").await;
        assert!(page.text("body").await.unwrap().contains("Notifications"), "notifications control is shown");
        page.set_offline(true).await.unwrap();
        page.goto(&origin).await.unwrap();
        wait(&page, "document.querySelector('main.app') !== null", "the app shell loads from cache while offline").await;
        page.set_offline(false).await.unwrap();
        page.goto(&origin).await.unwrap();
        wait(&page, "document.querySelectorAll('button.row').length > 0", "back online after the offline load").await;
        page.clear_cookies().await.unwrap();
        page.goto(&origin).await.unwrap();
        wait(&page, "document.querySelector('section.signin button.primary') !== null", "sign-in after clearing cookies").await;
        click_button(&page, "Sign in with passkey").await;
        wait(&page, "document.querySelectorAll('button.row').length > 0", "signed in again with the passkey").await;

        step("F12 machines: the list, adding a machine with the code the page shows");
        page.click("nav.tabbar button.home").await.unwrap();
        wait(&page, "document.querySelectorAll('section.machines button.machine').length === 1", "machine list").await;
        wait(&page, "document.querySelector('button.machine .dot.on') !== null", "the machine shows online").await;
        click_button(&page, "Add machine").await;
        wait(&page, "(document.querySelector('code.pair-code')?.textContent || '').length >= 8", "a pairing code").await;
        let pair_code = page.text("code.pair-code").await.unwrap();
        let command = page.text("code.pair-command").await.unwrap();
        assert_eq!(
            command,
            format!("anila-service pair {pair_code} --worker {origin}"),
            "the exact command to run"
        );
        let (_, config2) = pair_machine(&worker.http_base(), &tmux2, SEED2, &pair_code, "second")
            .expect("the code from the page pairs a machine");
        let _service2 = anila_e2e::stack::Service::start(&config2, SEED2).expect("second service");
        wait(&page, "document.querySelector('p.paired')?.textContent.includes('second')", "the page notices the new machine").await;
        click_button(&page, "Open it").await;
        wait(&page, "[...document.querySelectorAll('button.row .name')].some(e => e.textContent === 'two')", "machine 2's sessions").await;
        assert_eq!(page.text("span.host").await.unwrap(), "second");

        step("tabs: one row of tabs across machines; closing one only detaches");
        page.eval("[...document.querySelectorAll('button.row')].find(b => b.querySelector('.name').textContent === 'two').setAttribute('data-e2e','row')")
            .await
            .unwrap();
        page.click("[data-e2e=row]").await.unwrap();
        wait(&page, "document.querySelector('.xterm-helper-textarea') !== null", "machine 2's terminal").await;
        assert!(anila_e2e::util::wait_until(Duration::from_secs(10), || tmux2.clients().len() == 1), "attached to two");
        // The tab for `work` survived the reloads above; both tabs name their machine.
        let tab_text = "[...document.querySelectorAll('nav.tabbar .tab')].map(t => (t.querySelector('.tab-host').hidden ? '' : t.querySelector('.tab-host').textContent + '/') + t.querySelector('.tab-name').textContent).join(',')";
        if page.wait_for(&format!("{tab_text} === 'test-host/work,second/two'"), T).await.is_err() {
            panic!("two tabs across machines: {}", page.eval(tab_text).await.unwrap());
        }
        assert_eq!(page.text("nav.tabbar .tab.active .tab-name").await.unwrap(), "two");
        assert_eq!(page.eval("document.querySelectorAll('section.terminal .bar').length").await.unwrap(), serde_json::json!(0), "no header rows above the terminal");
        page.eval("document.querySelector('nav.tabbar .tab:not(.active) .tab-open').setAttribute('data-e2e','tab')").await.unwrap();
        page.click("[data-e2e=tab]").await.unwrap();
        wait(&page, "document.querySelector('nav.tabbar .tab.active .tab-name')?.textContent === 'work' && document.querySelector('.xterm-helper-textarea') !== null", "the work tab switches machines").await;
        assert!(anila_e2e::util::wait_until(Duration::from_secs(10), || tmux.clients().len() == 1), "attached to work again");
        page.eval("document.querySelector('.xterm-helper-textarea').focus()").await.unwrap();
        page.insert_text("via-tab").await.unwrap();
        page.press("Enter").await.unwrap();
        wait(&page, "document.querySelector('.xterm-rows').innerText.includes('via-tab')", "typing in the switched tab").await;
        page.screenshot_to(&anila_e2e::util::run_dir("browser").join("tabs.png")).await.ok();
        page.click("nav.tabbar .tab.active .tab-close").await.unwrap();
        wait(&page, "document.querySelector('nav.tabbar .tab.active .tab-name')?.textContent === 'two'", "closing shows the neighbouring tab").await;
        wait(&page, "document.querySelectorAll('nav.tabbar .tab').length === 1", "one tab left").await;
        assert!(tmux.capture("work").contains("via-tab"), "the closed tab's session keeps running");
        page.click("nav.tabbar button.tab-new").await.unwrap();
        wait(&page, "document.querySelectorAll('button.row').length > 0 && document.querySelector('nav.tabbar .tab-new.active') !== null", "+ shows this machine's sessions").await;

        step("O2a background tabs stay attached: switching re-attaches nothing");
        open_row(&page, "three").await;
        wait(&page, &shown("three"), "three on screen").await;
        // Input before the channel is live is paused, not queued (contract).
        assert!(anila_e2e::util::wait_until(Duration::from_secs(10), || tmux2.attachments().iter().any(|a| a.starts_with("three "))), "three attached");
        type_into(&page, "in-three").await;
        wait(&page, "document.querySelector('.term:not([hidden]) .xterm-rows').innerText.includes('in-three')", "typed in three").await;
        let attached = tmux2.attachments();
        assert_eq!(attached.len(), 2, "two stays attached in the background: {attached:?}");
        tmux2.type_line("two", "while-hidden");
        click_tab(&page, "two").await;
        wait(&page, &shown("two"), "two on screen").await;
        wait(&page, "document.querySelector('.term:not([hidden]) .xterm-rows').innerText.includes('while-hidden')", "output that arrived while hidden").await;
        assert_eq!(tmux2.attachments(), attached, "no re-attach on a switch");
        click_tab(&page, "three").await;
        wait(&page, &shown("three"), "three again").await;
        wait(&page, "document.querySelector('.term:not([hidden]) .xterm-rows').innerText.includes('in-three')", "three kept its screen").await;
        assert_eq!(tmux2.attachments(), attached, "no re-attach switching back");
        assert_eq!(
            page.eval("document.querySelectorAll('.term[data-session]').length").await.unwrap(),
            serde_json::json!(2),
            "one terminal element per attached tab"
        );

        step("O2a at most 8 attached per machine: the least recently shown detaches");
        for i in 1..=7 {
            page.click("nav.tabbar button.tab-new").await.unwrap();
            open_row(&page, &format!("s{i}")).await;
            wait(&page, &shown(&format!("s{i}")), "a new tab on screen").await;
        }
        // two, three, s1..s7 = 9 tabs, 8 attached: `two` was shown longest ago.
        assert!(anila_e2e::util::wait_until(Duration::from_secs(10), || tmux2.attachments().len() == 8), "8 attached: {:?}", tmux2.attachments());
        assert!(!tmux2.attachments().iter().any(|a| a.starts_with("two ")), "two detached: {:?}", tmux2.attachments());
        wait(&page, "document.querySelectorAll('nav.tabbar .tab').length === 9", "its tab stays").await;
        click_tab(&page, "two").await;
        wait(&page, &shown("two"), "two re-attaches when shown").await;
        assert!(anila_e2e::util::wait_until(Duration::from_secs(10), || {
            let a = tmux2.attachments();
            a.len() == 8 && a.iter().any(|a| a.starts_with("two ")) && !a.iter().any(|a| a.starts_with("three "))
        }), "two back, three (now the oldest) detached: {:?}", tmux2.attachments());
        for name in ["three", "s1", "s2", "s3", "s4", "s5", "s6", "s7"] {
            page.eval(&format!("[...document.querySelectorAll('nav.tabbar .tab')].find(t => t.querySelector('.tab-name').textContent === {name:?}).querySelector('.tab-close').setAttribute('data-e2e','x')")).await.unwrap();
            page.click("[data-e2e=x]").await.unwrap();
            page.eval("document.querySelector('[data-e2e=x]')?.removeAttribute('data-e2e')").await.unwrap();
        }
        wait(&page, "document.querySelectorAll('nav.tabbar .tab').length === 1", "closed the extra tabs").await;
        assert!(anila_e2e::util::wait_until(Duration::from_secs(10), || tmux2.attachments().len() == 1), "closing a tab detaches it: {:?}", tmux2.attachments());

        step("O2b one socket per machine: switching machines re-attaches nothing");
        let two = tmux2.attachments();
        assert_eq!(two.len(), 1, "{two:?}");
        page.click("nav.tabbar button.home").await.unwrap();
        wait(&page, "document.querySelectorAll('section.machines button.machine').length === 2", "machine list").await;
        page.eval(&format!("[...document.querySelectorAll('section.machines button.machine')].find(b => b.querySelector('.name')?.textContent === {HOST:?}).setAttribute('data-e2e','m1')")).await.unwrap();
        page.click("[data-e2e=m1]").await.unwrap();
        open_row(&page, "work").await;
        wait(&page, &shown("work"), "work on the first machine").await;
        assert!(anila_e2e::util::wait_until(Duration::from_secs(10), || tmux.attachments().len() == 1), "work attached: {:?}", tmux.attachments());
        let one = tmux.attachments();
        assert_eq!(tmux2.attachments(), two, "the machine list and machine 1 kept machine 2's tab attached");
        for _ in 0..2 {
            click_tab(&page, "two").await;
            wait(&page, &shown("two"), "two, on machine 2").await;
            click_tab(&page, "work").await;
            wait(&page, &shown("work"), "work, on machine 1").await;
        }
        type_into(&page, "across-machines").await;
        wait(&page, "document.querySelector('.term:not([hidden]) .xterm-rows').innerText.includes('across-machines')", "typing after switching machines").await;
        assert_eq!(tmux.attachments(), one, "no re-attach on machine 1");
        assert_eq!(tmux2.attachments(), two, "no re-attach on machine 2");

        step("O2b each tab shows its own machine's status");
        let tab_offline = |name: &str| format!("[...document.querySelectorAll('nav.tabbar .tab')].find(t => t.querySelector('.tab-name').textContent === {name:?})?.classList.contains('offline')");
        assert_eq!(page.eval(&tab_offline("work")).await.unwrap(), serde_json::json!(false));
        drop(service1.take());
        wait(&page, &format!("{} === true", tab_offline("work")), "machine 1's tab shows it is offline").await;
        assert_eq!(page.eval(&tab_offline("two")).await.unwrap(), serde_json::json!(false), "machine 2's tab is unaffected");
        service1 = Some(start_service(&worker.http_base(), &tmux));
        wait(&page, &format!("{} === false", tab_offline("work")), "machine 1 back").await;
        page.eval("[...document.querySelectorAll('nav.tabbar .tab')].find(t => t.querySelector('.tab-name').textContent === 'work').querySelector('.tab-close').setAttribute('data-e2e','x')").await.unwrap();
        page.click("[data-e2e=x]").await.unwrap();
        page.eval("document.querySelector('[data-e2e=x]')?.removeAttribute('data-e2e')").await.unwrap();
        wait(&page, "document.querySelectorAll('nav.tabbar .tab').length === 1", "back to one tab").await;
        page.click("nav.tabbar button.home").await.unwrap();
        wait(&page, "document.querySelectorAll('section.machines button.machine').length === 2", "two machines").await;

        step("F12 machine settings: share code");
        page.eval("[...document.querySelectorAll('section.machines li')].find(li => li.querySelector('.name')?.textContent === 'second').querySelector('button.settings').setAttribute('data-e2e','gear')")
            .await
            .unwrap();
        page.click("[data-e2e=gear]").await.unwrap();
        wait(&page, "document.querySelector('section.machine-settings') !== null", "machine settings").await;
        click_button(&page, "Share").await;
        wait(&page, "(document.querySelector('code.share-code')?.textContent || '').length >= 8", "a share code").await;
        assert!(page.text("section.machine-settings").await.unwrap().contains("Not shared with anyone"));

        step("F12 account page: passkeys, another device, invites");
        click_button(&page, "‹ Machines").await;
        click_button(&page, "Account").await;
        wait(&page, "document.querySelectorAll('li.passkey').length === 1", "one passkey listed").await;
        click_button(&page, "Add another device").await;
        wait(&page, "(document.querySelector('code.device-code')?.textContent || '').length >= 8", "a device code").await;
        wait(&page, "document.querySelector('section.admin') !== null", "the admin section").await;
        click_button(&page, "Invite someone").await;
        wait(&page, "(document.querySelector('code.invite-code')?.textContent || '').length >= 8", "an invite code").await;
        wait(&page, "document.querySelectorAll('li.account-row').length === 1", "the accounts list").await;
        click_button(&page, "Sign out").await;
        wait(&page, "document.querySelector('section.signin button.primary') !== null", "signed out").await;

        eprintln!("browser console: {:#?}", page.console());
    });
    eprintln!("\nreal browser suite passed");
}
