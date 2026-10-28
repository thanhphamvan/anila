#[path = "../../frontend/src/shared/layout.rs"]
mod layout;

mod common;

use anila_e2e::{
    authn::SoftAuthenticator, browser::Browser, tmux::TmuxServer, util::workspace_root,
};
use common::*;
use std::{process::Command, time::Duration};

#[test]
fn panels_reuse_terminals_and_route_input_to_the_active_session() {
    assert!(
        Command::new("trunk")
            .args(["build", "--release", "--locked"])
            .current_dir(workspace_root().join("frontend"))
            .status()
            .unwrap()
            .success()
    );
    let worker = start_worker(false);
    let tmux = TmuxServer::new("e2e-layout");
    for name in ["pane1", "pane2", "pane3", "pane4"] {
        tmux.new_session(name, "cat");
    }
    let _service = start_service(&worker.http_base(), &tmux);
    let code = enroll_code(&worker.http_base(), &tmux);
    let mut key = SoftAuthenticator::new(42);
    let mut http = register(&worker, &code, &mut key);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let browser = Browser::launch().await.unwrap();
        let page = browser.new_page().await.unwrap();
        page.set_viewport(1280, 900, false).await.unwrap();
        page.set_test_session(&worker.origin(), &http.cookie().unwrap()).await.unwrap();
        page.goto(&worker.origin()).await.unwrap();
        for name in ["pane1", "pane2", "pane3", "pane4"] {
            page.wait_for(&format!("[...document.querySelectorAll('button.row .name')].some(row => row.textContent === '{name}')"), Duration::from_secs(20)).await.unwrap();
            page.eval(&format!("[...document.querySelectorAll('button.row')].find(row => row.querySelector('.name').textContent === '{name}').setAttribute('data-open','target')")).await.unwrap();
            page.click("[data-open=target]").await.unwrap();
            page.wait_for(&format!("document.querySelector('.term:not([hidden])')?.dataset.session === '{name}'"), Duration::from_secs(20)).await.unwrap();
            assert_eq!(page.eval("document.querySelector('[aria-label=\"Terminal layout\"]').value").await.unwrap(), "single");
            if name != "pane4" { page.click(".tab-new").await.unwrap(); }
        }
        page.eval("document.querySelector('[aria-label=\"Terminal layout\"]').value='four';document.querySelector('[aria-label=\"Terminal layout\"]').dispatchEvent(new Event('change',{bubbles:true}))").await.unwrap();
        page.wait_for("document.querySelectorAll('.term:not([hidden])').length === 4", Duration::from_secs(10)).await.unwrap();
        assert!(anila_e2e::util::wait_until(Duration::from_secs(10), || tmux.attachments().len() == 4));
        let attached = tmux.attachments();
        assert_eq!(page.eval("(() => { const cells=[...document.querySelectorAll('.panel-slot:not([hidden])')].map(p=>p.getBoundingClientRect());return cells.every(r=>r.width>0&&r.height>0) && new Set(cells.map(r=>r.x)).size===2 && new Set(cells.map(r=>r.y)).size===2; })()").await.unwrap(), true, "four panels occupy four visible corners");
        page.screenshot_to(&workspace_root().join("e2e/.runs/layout-four.png")).await.ok();
        let sessions = page.eval("[...document.querySelectorAll('.panel-body')].map(body => body.querySelector('.term:not([hidden])')?.dataset.session)").await.unwrap();
        let target = sessions[1].as_str().unwrap();
        page.click("[aria-label='Session for panel 2']").await.unwrap();
        page.wait_for("document.activeElement === document.querySelector('[aria-label=\"Session for panel 2\"]')", Duration::from_secs(5)).await.unwrap();
        page.eval("new Promise(resolve => requestAnimationFrame(() => resolve(true)))").await.unwrap();
        assert_eq!(page.eval("document.activeElement === document.querySelector('[aria-label=\"Session for panel 2\"]')").await.unwrap(), true, "activating a session picker must not steal its focus");
        page.press("Escape").await.unwrap();
        page.click("[data-panel-body='1']").await.unwrap();
        if let Err(error) = page.wait_for("document.querySelector('.panel-slot.active [data-panel-body=\"1\"]') !== null", Duration::from_secs(5)).await {
            page.screenshot_to(&workspace_root().join("e2e/.runs/layout-focus.png")).await.ok();
            panic!("{error}: active={:?}, focused={:?}, panels={:?}", page.eval("document.querySelector('.panel-slot.active')?.getAttribute('aria-label')").await, page.eval("document.activeElement?.className").await, page.eval("[...document.querySelectorAll('.term')].map(t=>({session:t.dataset.session,panel:t.dataset.panel,active:t.dataset.active,hidden:t.hidden}))").await);
        }
        page.insert_text("only-active-panel").await.unwrap();
        page.press("Enter").await.unwrap();
        assert!(anila_e2e::util::wait_until(Duration::from_secs(5), || tmux.capture(target).contains("only-active-panel")));
        for name in ["pane1", "pane2", "pane3", "pane4"] {
            if name != target { assert!(!tmux.capture(name).contains("only-active-panel"), "input leaked to {name}"); }
        }
        for (mode, count) in [("dual",2), ("three",3), ("four",4), ("single",1), ("four",4)] {
            page.eval(&format!("(() => {{ const picker=document.querySelector('[aria-label=\"Terminal layout\"]');picker.value='{mode}';picker.dispatchEvent(new Event('change',{{bubbles:true}})); }})()")).await.unwrap();
            page.wait_for(&format!("document.querySelectorAll('.term:not([hidden])').length === {count}"), Duration::from_secs(10)).await.unwrap();
            if mode == "three" {
                page.click("[aria-label='Flip 2–1 layout']").await.unwrap();
                page.wait_for("document.querySelector('.terminal-grid').dataset.layout === 'three-left'", Duration::from_secs(5)).await.unwrap();
                let positions = page.eval("[...document.querySelectorAll('.panel-slot:not([hidden])')].map(p=>({x:p.getBoundingClientRect().x,y:p.getBoundingClientRect().y,h:p.getBoundingClientRect().height}))").await.unwrap();
                assert!(positions[0]["x"].as_f64().unwrap() < positions[1]["x"].as_f64().unwrap());
                assert!(positions[0]["h"].as_f64().unwrap() > positions[1]["h"].as_f64().unwrap()*1.8);
                page.screenshot_to(&workspace_root().join("e2e/.runs/layout-three-left.png")).await.ok();
                page.click("[aria-label='Flip 2–1 layout']").await.unwrap();
                page.wait_for("document.querySelector('.terminal-grid').dataset.layout === 'three-right'", Duration::from_secs(5)).await.unwrap();
                assert_eq!(page.eval("(() => {const big=document.querySelector('.panel-slot').getBoundingClientRect(), small=document.querySelectorAll('.panel-slot')[1].getBoundingClientRect();return big.x>small.x&&big.height>small.height*1.8})()").await.unwrap(), true, "flipped layout places the large panel on the right");
                page.screenshot_to(&workspace_root().join("e2e/.runs/layout-three-right.png")).await.ok();
            }
            assert_eq!(tmux.attachments(), attached, "layout changes must not reattach tmux clients");
        }
        page.eval("(() => { const picker=document.querySelector('[aria-label=\"Session for panel 2\"]');picker.value='3';picker.dispatchEvent(new Event('change',{bubbles:true})); })()").await.unwrap();
        page.wait_for("document.querySelector('[data-panel-body=\"1\"] .term')?.dataset.session === 'pane4'", Duration::from_secs(5)).await.unwrap();
        assert_eq!(page.eval("new Set([...document.querySelectorAll('.term:not([hidden])')].map(term=>term.dataset.session)).size").await.unwrap(), 4, "assigning an already visible session swaps its panel");
        assert_eq!(tmux.attachments(), attached);
        // Mix machines in one grid, then verify toolbar/context switches do
        // not redirect input or recreate the first machine's PTY clients.
        let tmux2 = TmuxServer::new("e2e-layout-other");
        tmux2.new_session("other", "cat");
        tmux2.type_line("other", "other-ready");
        let (status, pairing) = http.post("/api/pair-codes", serde_json::json!({}));
        assert_eq!(status, 200, "create an isolated machine pairing code");
        let (_, config) = pair_machine(&worker.http_base(), &tmux2, SEED2, pairing["code"].as_str().unwrap(), "other-machine").expect("pair second test machine");
        let _service2 = anila_e2e::stack::Service::start(&config, SEED2).unwrap();
        page.click(".home").await.unwrap();
        page.wait_for("[...document.querySelectorAll('button.machine')].some(b=>b.textContent.includes('other-machine'))", Duration::from_secs(15)).await.unwrap();
        page.eval("[...document.querySelectorAll('button.machine')].find(b=>b.textContent.includes('other-machine')).setAttribute('data-other-machine','target')").await.unwrap();
        page.click("[data-other-machine=target]").await.unwrap();
        page.wait_for("[...document.querySelectorAll('button.row .name')].some(row=>row.textContent==='other')", Duration::from_secs(15)).await.unwrap();
        page.click("button.row").await.unwrap();
        page.wait_for("document.querySelectorAll('.term:not([hidden])').length===4 && new Set([...document.querySelectorAll('.term:not([hidden])')].map(term=>term.dataset.host)).size===2", Duration::from_secs(10)).await.unwrap();
        page.wait_for("document.querySelector('.term[data-session=other] .xterm-rows')?.innerText.includes('other-ready')", Duration::from_secs(10)).await.expect("second machine attachment has delivered output before input");
        page.click(".term[data-session=other]").await.unwrap();
        page.wait_for("document.activeElement === document.querySelector('.term[data-session=other] textarea')", Duration::from_secs(5)).await.expect("clicking the second machine terminal focuses its input");
        page.insert_text("remote-only").await.unwrap();
        page.press("Enter").await.unwrap();
        assert!(anila_e2e::util::wait_until(Duration::from_secs(5), || tmux2.capture("other").contains("remote-only")));
        let local = page.eval("(() => {const term=[...document.querySelectorAll('.term:not([hidden])')].find(term=>term.dataset.session!=='other');term.closest('.panel-body').setAttribute('data-local-panel','target');return term.dataset.session})()").await.unwrap();
        page.click("[data-local-panel=target]").await.unwrap();
        page.insert_text("local-only").await.unwrap();
        page.press("Enter").await.unwrap();
        assert!(anila_e2e::util::wait_until(Duration::from_secs(5), || tmux.capture(local.as_str().unwrap()).contains("local-only")));
        assert!(!tmux2.capture("other").contains("local-only"));
        assert_eq!(tmux.attachments(), attached, "switching panels across machines reuses attached clients");
        page.click("button[aria-label='Close other']").await.unwrap();
        page.wait_for("document.querySelectorAll('.term:not([hidden])').length===3", Duration::from_secs(5)).await.unwrap();
        assert!(anila_e2e::util::wait_until(Duration::from_secs(5), || tmux2.attachments().is_empty()));
        assert!(tmux2.capture("other").contains("remote-only"), "closing a panel's tab does not kill its tmux session");
        page.set_viewport(390,844,true).await.unwrap();
        page.wait_for("document.querySelector('.app').getBoundingClientRect().width <= 390", Duration::from_secs(5)).await.unwrap();
        assert_eq!(page.eval("document.documentElement.scrollWidth <= innerWidth").await.unwrap(), true);
    });
}
