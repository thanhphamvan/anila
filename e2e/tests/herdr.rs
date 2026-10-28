//! Browser creation and reconnect through the Worker to a real, isolated herdr.
mod common;

use anila_e2e::{
    authn::SoftAuthenticator, browser::Browser, stack::Service, tmux::TmuxServer,
    util::workspace_root,
};
use common::*;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Command, time::Duration};

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
struct Fixture {
    wrapper: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(&self.wrapper)
            .args(["session", "stop", "work", "--json"])
            .output();
    }
}

#[test]
fn browser_creates_herdr_alongside_tmux_and_restores_its_tab() {
    let Ok(binary) = std::env::var("ANILA_HERDR_BINARY") else {
        eprintln!("set ANILA_HERDR_BINARY to run the browser herdr regression");
        return;
    };
    assert!(
        Command::new("trunk")
            .args(["build", "--release", "--locked"])
            .current_dir(workspace_root().join("frontend"))
            .status()
            .unwrap()
            .success()
    );
    // herdr's client socket is seven characters longer than its API socket;
    // repository-relative harness paths can exceed Unix's 108-byte limit.
    let dir = std::env::temp_dir().join(format!("anila-hb-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let config_dir = dir.join("config/herdr");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "onboarding=false\n[terminal]\ndefault_shell='/bin/sh'\nshell_mode='non_login'\n",
    )
    .unwrap();
    let wrapper = dir.join("herdr-test");
    std::fs::write(&wrapper, format!("#!/bin/sh\nunset HERDR_SESSION HERDR_SOCKET_PATH HERDR_CONFIG_PATH HERDR_ENV\nexport XDG_CONFIG_HOME={}\nexport XDG_STATE_HOME={}\nexport XDG_CACHE_HOME={}\nexport HOME={}\nexport PS1='ANILA-HERDR> '\nexec {} \"$@\"\n", quote(dir.join("config").to_str().unwrap()), quote(dir.join("state").to_str().unwrap()), quote(dir.join("cache").to_str().unwrap()), quote(dir.to_str().unwrap()), quote(&binary))).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _fixture = Fixture {
        wrapper: wrapper.clone(),
    };
    let worker = start_worker(false);
    let tmux = TmuxServer::new("herdr-browser");
    tmux.new_session("work", "cat");
    tmux.type_line("work", "tmux-stays-separate");
    let config = format!(
        "herdr_binary={:?}\n{}\n[[preset]]\nid='herdr'\nlabel='Herdr workspace'\nbackend='herdr'\ndir={:?}\n",
        wrapper.to_str().unwrap(),
        service_config(&worker.http_base(), &tmux),
        dir.to_str().unwrap()
    );
    let _service = Service::start(&config, vector_key().0).unwrap();
    let code = enroll_code(&worker.http_base(), &tmux);
    let http = register(&worker, &code, &mut SoftAuthenticator::new(84));
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let browser = Browser::launch().await.unwrap();
        let page = browser.new_page().await.unwrap();
        page.set_test_session(&worker.origin(), &http.cookie().unwrap()).await.unwrap();
        page.goto(&worker.origin()).await.unwrap();
        let timeout = Duration::from_secs(30);
        page.wait_for("document.querySelector('button.row .name')?.textContent === 'work'", timeout).await.unwrap();
        page.click("button.new").await.unwrap();
        page.fill("section.sessions form input", "work").await.unwrap();
        page.eval("(() => { const e=document.querySelector('section.sessions form select'); e.value='herdr'; e.dispatchEvent(new Event('change',{bubbles:true})); })()").await.unwrap();
        page.click("section.sessions form button[type=submit]").await.unwrap();
        page.wait_for("document.querySelector('.term[data-session=\"herdr:work\"] .xterm-rows')?.textContent.includes('ANILA-HERDR>')", timeout).await.unwrap();
        page.click(".term:not([hidden]) .xterm-screen").await.unwrap();
        page.insert_text("ANILA_BROWSER_STATE=preserved; printf 'browser-%s\\n' herdr-ok").await.unwrap();
        page.press("Enter").await.unwrap();
        page.wait_for("document.querySelector('.term:not([hidden]) .xterm-rows')?.textContent.includes('browser-herdr-ok')", timeout).await.unwrap();
        assert!(!tmux.capture("work").contains("browser-herdr-ok"), "herdr input must not reach same-named tmux session");
        page.goto(&worker.origin()).await.unwrap();
        page.wait_for("document.querySelector('nav.tabbar .tab-name')?.textContent === 'herdr:work'", timeout).await.unwrap();
        page.click("nav.tabbar .tab-open").await.unwrap();
        page.wait_for("document.querySelector('.term[data-session=\"herdr:work\"] .xterm-rows')?.textContent.includes('browser-herdr-ok')", timeout).await.unwrap();
        page.click(".term:not([hidden]) .xterm-screen").await.unwrap();
        page.insert_text("printf 'state=%s\\n' \"$ANILA_BROWSER_STATE\"").await.unwrap();
        page.press("Enter").await.unwrap();
        page.wait_for("document.querySelector('.term:not([hidden]) .xterm-rows')?.textContent.includes('state=preserved')", timeout).await.unwrap();
        page.screenshot_to(&workspace_root().join("e2e/.runs/herdr-browser.png")).await.unwrap();
    });
}
