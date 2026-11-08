//! A terminal bell (Claude Code's `terminal_bell` on completion) chimes and
//! notifies only while the user is not watching that terminal.

mod common;

use std::{io::Write, process::Command, time::Duration};

use anila_e2e::{
    authn::SoftAuthenticator, browser::Browser, tmux::TmuxServer, util::workspace_root,
};
use common::*;

#[test]
fn bell_chimes_and_notifies_when_not_watching() {
    assert!(
        Command::new("trunk")
            .args(["build", "--release", "--locked"])
            .current_dir(workspace_root().join("frontend"))
            .status()
            .unwrap()
            .success()
    );
    let worker = start_worker(false);
    let tmux = TmuxServer::new("e2e-bell");
    tmux.new_session("bell", "cat");
    let _service = start_service(&worker.http_base(), &tmux);
    let code = enroll_code(&worker.http_base(), &tmux);
    let mut key = SoftAuthenticator::new(43);
    let http = register(&worker, &code, &mut key);
    let tty = tmux.cmd(&["display-message", "-p", "-t", "=bell:", "#{pane_tty}"]);
    let ring = |marker: &str| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(tty.trim())
            .unwrap()
            .write_all(format!("\x07{marker}\r\n").as_bytes())
            .unwrap();
    };

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let browser = Browser::launch().await.expect("isolated browser starts");
        let page = browser.new_page().await.unwrap();
        page.set_test_session(&worker.origin(), &http.cookie().unwrap())
            .await
            .unwrap();
        page.grant_notifications(&worker.origin()).await.unwrap();
        page.goto(&worker.origin()).await.unwrap();
        page.wait_for(
            "document.querySelector('button.row') !== null",
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        // Record chimes and notifications instead of playing or showing them.
        page.eval(
            r#"(() => {
            window.chimes = 0; window.shown = [];
            const Real = window.AudioContext;
            window.AudioContext = class extends Real {
                createOscillator() { window.chimes++; return super.createOscillator(); }
            };
            ServiceWorkerRegistration.prototype.showNotification = function (title, options) {
                window.shown.push({ title, data: options.data });
                return Promise.resolve();
            };
        })()"#,
        )
        .await
        .unwrap();
        page.click("button.row").await.unwrap();
        page.wait_for(
            "document.querySelector('.term:not([hidden]) .xterm-rows') !== null",
            Duration::from_secs(20),
        )
        .await
        .unwrap();

        ring("watched-bell");
        page.wait_for(
            "document.querySelector('.xterm-rows')?.innerText.includes('watched-bell')",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(
            page.eval("window.chimes + window.shown.length")
                .await
                .unwrap(),
            0,
            "no chime or notification while watching"
        );

        page.eval(
            "Object.defineProperty(document, 'hidden', { configurable: true, get: () => true })",
        )
        .await
        .unwrap();
        ring("background-bell");
        page.wait_for("window.shown.length === 1", Duration::from_secs(10))
            .await
            .expect("a background bell shows a notification");
        let shown = page.eval("window.shown[0]").await.unwrap();
        assert_eq!(shown["title"], "bell");
        assert_eq!(shown["data"]["session"], "bell");
        assert!(
            page.eval("window.chimes").await.unwrap().as_u64().unwrap() > 0,
            "a background bell chimes"
        );
    });
}
