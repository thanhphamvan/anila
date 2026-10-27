//! Part 1 of the end-to-end plan: the whole system, every component real.
//! Run with `cargo test -p anila-e2e --test functional -- --nocapture`.

mod common;

use std::time::Duration;

use anila_e2e::authn::SoftAuthenticator;
use anila_e2e::http::Http;
use anila_e2e::tmux::TmuxServer;
use anila_proto::client::Event;
use common::*;
use serde_json::json;

const T: Duration = Duration::from_secs(10);

fn step(name: &str) {
    eprintln!("\n=== {name}");
}

#[test]
fn functional_suite() {
    let worker = start_worker(false);
    let tmux = TmuxServer::new("e2e-func");
    tmux.new_session("work", "cat");
    let mut service = start_service(&worker.http_base(), &tmux);

    // F1: enroll the first passkey with a code from the host machine.
    step("F1 enroll");
    let code = enroll_code(&worker.http_base(), &tmux);
    assert_eq!(code.len(), 9, "code looks like XXXX-XXXX: {code}");
    let mut key = SoftAuthenticator::new(1);
    let mut http = register(&worker, &code, &mut key);
    let (status, me) = http.get("/api/me");
    assert_eq!(status, 200, "signed in after registering: {me}");
    assert_eq!(
        me["account"]["admin"], true,
        "the first passkey makes the admin: {me}"
    );
    let (status, hosts) = http.get("/api/hosts");
    assert_eq!(status, 200, "{hosts}");
    assert_eq!(
        hosts[0]["id"], HOST,
        "the bootstrap machine belongs to the admin"
    );
    let mut again = Http::new(&worker.http_base(), &worker.origin());
    let (status, _) = again.post("/api/passkeys/register/options", json!({ "code": code }));
    assert_eq!(status, 401, "an enroll code works once");

    // F2: sign in with the passkey; user verification is required.
    step("F2 login");
    let (status, signed_in) = login(&worker, &mut key);
    assert_eq!(status, 204, "login succeeds");
    let cookie = signed_in.cookie().expect("login sets the session cookie");
    key.set_uv(false);
    let (status, _) = login(&worker, &mut key);
    assert_eq!(status, 401, "login without user verification is refused");
    key.set_uv(true);
    let mut stranger = SoftAuthenticator::new(9);
    let (status, _) = login(&worker, &mut stranger);
    assert_eq!(status, 401, "an unknown passkey is refused");

    // F9 (part): the terminal socket needs the cookie and the right Origin.
    step("F9 socket guards");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        use tokio_tungstenite::tungstenite::{Error, client::IntoClientRequest};
        let url = format!("{}/ws/term", worker.ws_base());
        let mut req = url.clone().into_client_request().unwrap();
        req.headers_mut()
            .insert("Origin", worker.origin().parse().unwrap());
        match tokio_tungstenite::connect_async(req).await {
            Err(Error::Http(resp)) => assert_eq!(resp.status(), 401, "no cookie → 401"),
            other => panic!("expected 401 without a cookie, got {:?}", other.map(|_| ())),
        }
        let mut req = url.clone().into_client_request().unwrap();
        req.headers_mut()
            .insert("Origin", "https://evil.example".parse().unwrap());
        req.headers_mut()
            .insert("Cookie", format!("__Host-term={cookie}").parse().unwrap());
        match tokio_tungstenite::connect_async(req).await {
            Err(Error::Http(resp)) => assert_eq!(resp.status(), 403, "wrong Origin → 403"),
            other => panic!(
                "expected 403 for a wrong Origin, got {:?}",
                other.map(|_| ())
            ),
        }
    });

    rt.block_on(async {
        // F3, F4: presence and listing.
        step("F3 presence, F4 list");
        let p = probe(&worker.ws_base(), &worker.origin(), Some(cookie.clone()), 7).await;
        wait_connected(&p, 15).await;
        let (items, presets) = p.list(T).await.expect("list");
        assert!(
            items.iter().any(|i| i.name == "work" && i.cmd == "cat"),
            "work/cat listed: {items:?}"
        );
        assert!(presets.iter().any(|p| p.id == "ticker"));

        // F6: attach, type, resize.
        step("F6 attach and type");
        let ch = p.open("work", None, 80, 24, T).await.expect("open work");
        assert!(
            anila_e2e::util::wait_until(Duration::from_secs(5), || tmux.clients() == ["80x24"]),
            "tmux client attached at 80x24"
        );
        assert!(p.type_str(ch, "hello-e2e\r").await, "input accepted");
        p.wait_output(ch, "hello-e2e", T)
            .await
            .expect("echo comes back");
        p.resize(ch, 100, 30).await;
        assert!(
            anila_e2e::util::wait_until(Duration::from_secs(5), || tmux.clients() == ["100x30"]),
            "resize reached tmux"
        );

        // F7: a second tab on the same session sees the same output.
        step("F7 two tabs");
        let p2 = probe(&worker.ws_base(), &worker.origin(), Some(cookie.clone()), 8).await;
        wait_connected(&p2, 15).await;
        let ch2 = p2
            .open("work", None, 80, 24, T)
            .await
            .expect("second tab opens work");
        assert!(
            anila_e2e::util::wait_until(Duration::from_secs(5), || tmux.clients().len() == 2),
            "two tmux clients"
        );
        p.type_str(ch, "from-tab-7\r").await;
        p2.wait_output(ch2, "from-tab-7", T)
            .await
            .expect("tab 8 sees tab 7's typing");

        // F5: create a session from a preset; a duplicate name is refused.
        step("F5 create");
        let tk = p
            .open("ticks", Some("ticker"), 80, 24, T)
            .await
            .expect("create from preset");
        p.wait_output(tk, "tick 3", T).await.expect("ticker runs");
        assert!(
            p.open("ticks", Some("ticker"), 80, 24, T).await.is_err(),
            "duplicate name refused"
        );

        // F8: closing a channel detaches only its client; exit ends it.
        step("F8 close and exit");
        p.close_channel(tk).await;
        tmux.new_session("brief", "sh -c 'sleep 2'");
        let br = p.open("brief", None, 80, 24, T).await.expect("open brief");
        p.wait_event(
            Duration::from_secs(10),
            |e| matches!(e, Event::Exited { ch, .. } if *ch == br),
        )
        .await
        .expect("exited when the session ends");

        // F3 continued: the host goes away and comes back.
        step("F3 host offline and back");
        let resets = p.stats().resets;
        service.stop();
        wait_host(&p, false, 10).await;
        service.restart().expect("service restarts");
        wait_host(&p, true, 15).await;
        // After a service restart the PTY is new: the channel resets and works.
        wait_reset(&p, resets, 10).await;
        p.type_str(ch, "after-restart\r").await;
        p.wait_output(ch, "after-restart", T)
            .await
            .expect("typing works after restart");

        // F9 (rest): sign-out ends sockets; revoke-all ends every session.
        step("F9 logout and revoke-all");
        let mut out = Http::new(&worker.http_base(), &worker.origin());
        out.set_cookie(Some(cookie.clone()));
        let (status, _) = out.post("/api/logout", json!({}));
        assert_eq!(status, 204);
        wait_stopped(&p, 4401, 5).await;
        wait_stopped(&p2, 4401, 5).await;
        let (status, _) = out.get("/api/me");
        assert_eq!(status, 401, "the cookie is dead after logout");

        let (status, fresh) = login(&worker, &mut key);
        assert_eq!(status, 204);
        let p3 = probe(&worker.ws_base(), &worker.origin(), fresh.cookie(), 9).await;
        wait_connected(&p3, 15).await;
        service_cli(&worker.http_base(), &tmux, &["revoke-all"]);
        wait_stopped(&p3, 4401, 5).await;

        // Deleting the last passkey is refused.
        let (_, mut admin) = login(&worker, &mut key);
        let (status, list) = admin.get("/api/passkeys");
        assert_eq!(status, 200, "{list}");
        let id = list[0]["id"].as_str().unwrap().to_string();
        let (status, _) = admin.delete(&format!("/api/passkeys/{id}"));
        assert_eq!(status, 409, "the last passkey cannot be deleted");

        for x in [p, p2, p3] {
            x.shutdown().await;
        }
    });
    drop(service);
    eprintln!("\nfunctional suite passed");
}
