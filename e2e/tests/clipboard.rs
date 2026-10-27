//! Real pointer selection, OS clipboard shortcuts and terminal input behavior.

mod common;

use std::{process::Command, time::Duration};

use anila_e2e::{
    authn::SoftAuthenticator, browser::Browser, tmux::TmuxServer, util::workspace_root,
};
use common::*;

#[test]
fn terminal_clipboard_shortcuts() {
    assert!(
        Command::new("trunk")
            .args(["build", "--release", "--locked"])
            .current_dir(workspace_root().join("frontend"))
            .status()
            .unwrap()
            .success()
    );
    let worker = start_worker(false);
    let tmux = TmuxServer::new("e2e-clipboard");
    tmux.new_session("clipboard", "cat");
    tmux.cmd(&["set-option", "-g", "mouse", "off"]);
    let _service = start_service(&worker.http_base(), &tmux);
    let code = enroll_code(&worker.http_base(), &tmux);
    let mut key = SoftAuthenticator::new(41);
    let http = register(&worker, &code, &mut key);
    tmux.type_line("clipboard", "copy-this-terminal-text");

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let browser = Browser::launch().await.expect("isolated browser starts");
        let page = browser.new_page().await.unwrap();
        page.set_test_session(&worker.origin(), &http.cookie().unwrap()).await.unwrap();
        page.grant_clipboard(&worker.origin()).await.unwrap();
        page.goto(&worker.origin()).await.unwrap();
        page.wait_for("document.querySelector('button.row') !== null", Duration::from_secs(20)).await.unwrap();
        page.click("button.row").await.unwrap();
        page.wait_for("document.querySelector('.xterm-rows')?.innerText.includes('copy-this-terminal-text')", Duration::from_secs(20)).await.unwrap();
        let points = page.eval(r#"(() => {
            const text = 'copy-this-terminal-text';
            const row = [...document.querySelectorAll('.term:not([hidden]) .xterm-rows > div')].find(e => e.textContent.includes(text));
            const start = row.textContent.indexOf(text), end = start + text.length;
            const walk = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
            const range = document.createRange();
            let offset = 0, node;
            while ((node = walk.nextNode())) {
                const next = offset + node.length;
                if (offset <= start && start < next) range.setStart(node, start - offset);
                if (offset < end && end <= next) { range.setEnd(node, end - offset); break; }
                offset = next;
            }
            const r = range.getBoundingClientRect();
            return { x1:r.left + 0.1, x2:r.right + 0.1, y:r.top + r.height / 2 };
        })()"#).await.unwrap();
        let x1 = points["x1"].as_f64().unwrap();
        let x2 = points["x2"].as_f64().unwrap();
        let y = points["y"].as_f64().unwrap();
        page.drag((x1, y), (x2, y), 0).await.unwrap();
        page.wait_for("document.querySelectorAll('.xterm-selection div').length > 0", Duration::from_secs(5)).await.unwrap();
        for modifiers in [10, 2, 4] {
            page.eval("navigator.clipboard.writeText('clipboard-sentinel')").await.unwrap();
            page.shortcut('c', modifiers).await.unwrap();
            let copied = page.wait_for("navigator.clipboard.readText().then(text => text === 'copy-this-terminal-text')", Duration::from_secs(5)).await;
            assert!(copied.is_ok(), "copy shortcut {modifiers}: clipboard={:?}, selection_rects={:?}, focused={:?}, error={:?}",
                page.eval("navigator.clipboard.readText()").await,
                page.eval("document.querySelectorAll('.xterm-selection div').length").await,
                page.eval("document.activeElement?.className").await,
                page.text("section.terminal .error").await);
        }
        // With mouse reporting active, Shift-drag must still select locally.
        tmux.cmd(&["set-option", "-g", "mouse", "on"]);
        page.wait_for("document.querySelector('.xterm.enable-mouse-events') !== null", Duration::from_secs(5)).await.expect("tmux mouse reporting enabled");
        page.drag((x1, y), (x2, y), 8).await.unwrap();
        page.wait_for("document.querySelectorAll('.xterm-selection div').length > 0", Duration::from_secs(5)).await.unwrap();
        page.eval("navigator.clipboard.writeText('clipboard-sentinel')").await.unwrap();
        page.shortcut('c', 10).await.unwrap();
        page.wait_for("navigator.clipboard.readText().then(text => text === 'copy-this-terminal-text')", Duration::from_secs(5)).await.expect("Shift-selection in mouse mode copies");
        // Denied clipboard permission must be visible and must not become Ctrl+C input.
        page.eval("Object.defineProperty(navigator.clipboard, 'writeText', { configurable:true, value:() => Promise.reject(new DOMException('denied', 'NotAllowedError')) })").await.unwrap();
        page.shortcut('c', 10).await.unwrap();
        page.wait_for("document.querySelector('section.terminal .error[role=alert]')?.textContent.includes('Allow clipboard access')", Duration::from_secs(5)).await.expect("copy denial is shown without a passkey error");
        page.eval("delete navigator.clipboard.writeText").await.unwrap();
        tmux.cmd(&["set-option", "-g", "mouse", "off"]);
        page.wait_for("document.querySelector('.xterm.enable-mouse-events') === null", Duration::from_secs(5)).await.expect("tmux mouse reporting disabled");
        // Clear the selection with a click, then verify browser paste is intact.
        page.click(".xterm-screen").await.unwrap();
        page.wait_for("document.querySelectorAll('.xterm-selection div').length === 0", Duration::from_secs(5)).await.expect("selection cleared");
        // Ctrl+Shift+C with no selection must not interrupt cat either.
        page.shortcut('c', 10).await.unwrap();
        page.eval("navigator.clipboard.writeText('paste-still-works')").await.unwrap();
        page.shortcut('v', 10).await.unwrap();
        page.press("Enter").await.unwrap();
        page.wait_for("document.querySelector('.xterm-rows').innerText.includes('paste-still-works')", Duration::from_secs(10)).await.expect("paste still reaches the terminal");
        assert!(tmux.capture("clipboard").contains("paste-still-works"));
        // With no selection Ctrl+C must reach cat, which exits its tmux session.
        page.shortcut('c', 2).await.unwrap();
        assert!(anila_e2e::util::wait_until(Duration::from_secs(10), || {
            !tmux.cmd(&["list-sessions", "-F", "#{session_name}"]).lines().any(|name| name == "clipboard")
        }), "Ctrl+C without selection still interrupts the process");
    });
}
