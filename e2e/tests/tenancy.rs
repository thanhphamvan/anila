//! v0.0.2 accounts, machines and sharing, end to end (`docs/multi-tenant.md`):
//! a real worker, two real services on two private tmux servers, two
//! accounts. Run with `cargo test -p anila-e2e --test tenancy -- --nocapture`.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use anila_e2e::authn::SoftAuthenticator;
use anila_e2e::http::Http;
use anila_e2e::stack::Service;
use anila_e2e::tmux::TmuxServer;
use anila_e2e::util::wait_until;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use p256::{SecretKey, elliptic_curve::sec1::ToEncodedPoint};
use serde_json::{Value, json};

const T: Duration = Duration::from_secs(10);

fn step(name: &str) {
    eprintln!("\n=== {name}");
}

/// A fake push service that answers 201 and reports each request path.
fn fake_push_service() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let mut length = 0;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end().to_string();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let _ = write!(
                stream,
                "HTTP/1.1 201 Created\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            let _ = tx.send(path);
        }
    });
    (port, rx)
}

fn subscribe(http: &mut Http, port: u16, device: &str) {
    let ua = SecretKey::from_slice(&[7u8; 32]).unwrap();
    let (status, body) = http.post(
        "/api/push/subscribe",
        json!({ "endpoint": format!("http://127.0.0.1:{port}/push/{device}"),
                "keys": { "p256dh": B64.encode(ua.public_key().to_encoded_point(false).as_bytes()),
                          "auth": B64.encode([9u8; 16]) } }),
    );
    assert_eq!(status, 204, "subscribe {device}: {body}");
}

/// Every delivery path that arrives within a short window, sorted.
fn deliveries(rx: &mpsc::Receiver<String>) -> Vec<String> {
    let mut got = Vec::new();
    while let Ok(path) = rx.recv_timeout(Duration::from_secs(3)) {
        got.push(path);
    }
    got.sort();
    got
}

fn hosts(http: &mut Http) -> Vec<Value> {
    let (status, body) = http.get("/api/hosts");
    assert_eq!(status, 200, "GET /api/hosts: {body}");
    body.as_array().expect("a list of machines").clone()
}

fn host_names(http: &mut Http) -> Vec<String> {
    hosts(http)
        .iter()
        .map(|h| h["name"].as_str().unwrap().to_string())
        .collect()
}

fn wait_online(http: &mut Http, id: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let list = hosts(http);
        if list.iter().any(|h| h["id"] == id && h["online"] == true) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "machine {id} online within 20 s: {list:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn code(http: &mut Http, path: &str) -> String {
    let (status, body) = http.post(path, json!({}));
    assert_eq!(status, 200, "POST {path}: {body}");
    assert!(body["expires_at"].as_i64().is_some(), "{body}");
    body["code"].as_str().expect("a code").to_string()
}

#[test]
fn tenancy_suite() {
    let worker = start_worker_with(
        false,
        vec![
            (
                "VAPID_PRIVATE_KEY".into(),
                B64.encode(core::array::from_fn::<u8, 32, _>(|i| 200 - i as u8)),
            ),
            ("VAPID_SUBJECT".into(), "mailto:e2e@example.com".into()),
        ],
    );
    let url = worker.http_base();
    let tmux1 = TmuxServer::new("e2e-ten1");
    tmux1.new_session("one", "cat");
    let tmux2 = TmuxServer::new("e2e-ten2");
    tmux2.new_session("two", "cat");
    let _service1 = start_service(&url, &tmux1);
    let rt = tokio::runtime::Runtime::new().unwrap();

    step("T1 bootstrap: the first enroll code creates the admin account");
    let enroll = enroll_code(&url, &tmux1);
    let mut alice_key = SoftAuthenticator::new(1);
    let (status, body, mut alice) = try_register(&worker, &enroll, &mut alice_key, Some("Alice"));
    assert_eq!(status, 201, "bootstrap registration: {body}");
    let (status, me) = alice.get("/api/me");
    assert_eq!(status, 200, "{me}");
    assert_eq!(me["account"]["name"], "Alice");
    assert_eq!(me["account"]["admin"], true);
    let alice_id = me["account"]["id"].as_str().unwrap().to_string();
    wait_online(&mut alice, HOST);
    let list = hosts(&mut alice);
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["role"], "owner");
    assert_eq!(list[0]["owner_name"], "Alice");

    step("T2 pair a second machine from the web app");
    let pair = code(&mut alice, "/api/pair-codes");
    let (out, config2) =
        pair_machine(&url, &tmux2, SEED2, &pair, "second").expect("pairing succeeds");
    assert!(
        out.contains("second"),
        "pair output names the machine: {out}"
    );
    let id2 = config_host(&config2);
    assert!(
        id2.starts_with('h') && id2.len() == 16,
        "a generated machine id: {id2}"
    );
    assert!(
        pair_machine(&url, &tmux2, [43; 32], &pair, "third").is_err(),
        "a pair code works once"
    );
    let again = code(&mut alice, "/api/pair-codes");
    let err = pair_machine(&url, &tmux2, SEED2, &again, "dup").expect_err("same key twice");
    assert!(
        err.contains("409") || err.to_lowercase().contains("already"),
        "{err}"
    );
    let mut service2 = Service::start(&config2, SEED2).expect("second service starts");
    wait_online(&mut alice, &id2);
    let mut names = host_names(&mut alice);
    names.sort();
    assert_eq!(names, ["second", HOST]);
    let alice_cookie = alice.cookie().unwrap();
    rt.block_on(async {
        let p = probe_host(
            &worker.ws_base(),
            &worker.origin(),
            Some(alice_cookie.clone()),
            1,
            &id2,
        )
        .await;
        wait_connected(&p, 15).await;
        let (items, _) = p.list(T).await.expect("list on the second machine");
        assert!(items.iter().any(|i| i.name == "two"), "{items:?}");
        assert!(
            !items.iter().any(|i| i.name == "one"),
            "only machine 2's sessions: {items:?}"
        );
        let ch = p.open("two", None, 80, 24, T).await.expect("open two");
        assert!(p.type_str(ch, "on-machine-2\r").await);
        p.wait_output(ch, "on-machine-2", T)
            .await
            .expect("echo from machine 2");
        p.shutdown().await;
        assert_eq!(
            ws_status(
                &format!("{}/ws/term", worker.ws_base()),
                &worker.origin(),
                Some(&alice_cookie)
            )
            .await,
            400,
            "two machines: the socket must name one"
        );
    });

    step("T3 invite a second account");
    let invite = code(&mut alice, "/api/admin/invites");
    let mut anon = Http::new(&url, &worker.origin());
    let (status, kind) = anon.post("/api/codes/check", json!({ "code": invite }));
    assert_eq!(
        (status, kind["kind"].as_str()),
        (200, Some("invite")),
        "{kind}"
    );
    let (status, _) = anon.post("/api/codes/check", json!({ "code": pair }));
    assert_eq!(status, 404, "a used pair code is not a sign-in code");
    let mut bob_key = SoftAuthenticator::new(2);
    let (status, body, _) = try_register(&worker, &invite, &mut bob_key, None);
    assert_eq!(status, 400, "an invite needs an account name: {body}");
    let mut bob_key = SoftAuthenticator::new(3);
    let (status, body, mut bob) = try_register(&worker, &invite, &mut bob_key, Some("Bob"));
    assert_eq!(status, 201, "invite registration: {body}");
    let (_, me) = bob.get("/api/me");
    assert_eq!(
        (
            me["account"]["name"].as_str(),
            me["account"]["admin"].as_bool()
        ),
        (Some("Bob"), Some(false))
    );
    let bob_id = me["account"]["id"].as_str().unwrap().to_string();
    assert_ne!(bob_id, alice_id);
    assert!(hosts(&mut bob).is_empty(), "a new account sees no machines");
    let (status, _) = bob.post("/api/admin/invites", json!({}));
    assert_eq!(status, 403, "only admins invite");
    let (status, _) = bob.get("/api/admin/accounts");
    assert_eq!(status, 403, "only admins list accounts");
    let mut fresh = Http::new(&url, &worker.origin());
    let (status, body) = fresh.post("/api/passkeys/register/options", json!({ "code": invite }));
    assert_eq!(status, 401, "an invite works once: {body}");

    step("T4 isolation: Bob cannot reach Alice's machines");
    let bob_cookie = bob.cookie().unwrap();
    rt.block_on(async {
        for id in [HOST, id2.as_str(), "hnosuchmachine00"] {
            let status = ws_status(
                &format!("{}/ws/term?host={id}", worker.ws_base()),
                &worker.origin(),
                Some(&bob_cookie),
            )
            .await;
            assert_eq!(status, 404, "Bob's socket to {id}");
        }
        assert_eq!(
            ws_status(
                &format!("{}/ws/term", worker.ws_base()),
                &worker.origin(),
                Some(&bob_cookie)
            )
            .await,
            400,
            "no machines: nothing to pick"
        );
        assert_eq!(
            ws_status(
                &format!("{}/ws/agent?host=hnosuchmachine00", worker.ws_base()),
                &worker.origin(),
                None
            )
            .await,
            404,
            "an unknown machine's service socket"
        );
    });
    for (method, path) in [
        ("GET", format!("/api/hosts/{id2}/members")),
        ("POST", format!("/api/hosts/{id2}/share-codes")),
        ("PATCH", format!("/api/hosts/{id2}")),
        ("DELETE", format!("/api/hosts/{id2}")),
        ("DELETE", format!("/api/hosts/{id2}/members/{alice_id}")),
    ] {
        let (status, body) = match method {
            "GET" => bob.get(&path),
            "POST" => bob.post(&path, json!({})),
            "PATCH" => bob.patch(&path, json!({ "name": "mine" })),
            _ => bob.delete(&path),
        };
        assert_eq!(status, 404, "Bob {method} {path}: {body}");
    }
    // The first machine's enroll code adds a device to Alice, not Bob.
    let enroll = enroll_code(&url, &tmux1);
    let mut alice_phone = SoftAuthenticator::new(5);
    let mut alice2 = register(&worker, &enroll, &mut alice_phone);
    let (_, me) = alice2.get("/api/me");
    assert_eq!(
        me["account"]["id"],
        alice_id.as_str(),
        "enroll adds a passkey to the owner"
    );
    // A service with machine 2's key cannot sign in as machine 1.
    let mut impostor =
        Service::start(&service_config(&url, &tmux2), SEED2).expect("impostor process starts");
    rt.block_on(async {
        let p = probe_host(
            &worker.ws_base(),
            &worker.origin(),
            Some(alice_cookie.clone()),
            2,
            HOST,
        )
        .await;
        wait_connected(&p, 15).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            p.host_online(),
            Some(true),
            "the real service stays connected"
        );
        let (items, _) = p.list(T).await.expect("list still reaches machine 1");
        assert!(items.iter().any(|i| i.name == "one"), "{items:?}");
        p.shutdown().await;
    });
    assert!(
        wait_until(Duration::from_secs(5), || impostor.log().contains("4401")
            || impostor.log().to_lowercase().contains("signature")),
        "the impostor was refused:\n{}",
        impostor.log()
    );
    impostor.stop();

    step("T5 share machine 2 with Bob");
    let share = code(&mut alice, &format!("/api/hosts/{id2}/share-codes"));
    let (status, _) = anon.post("/api/codes/check", json!({ "code": share }));
    assert_eq!(status, 404, "share codes are not sign-in codes");
    let (status, _) = alice.post("/api/share/redeem", json!({ "code": share }));
    assert_eq!(status, 409, "the owner cannot join their own machine");
    let (status, view) = bob.post("/api/share/redeem", json!({ "code": share }));
    assert_eq!(status, 200, "redeem: {view}");
    assert_eq!(
        (view["id"].as_str(), view["role"].as_str()),
        (Some(id2.as_str()), Some("member"))
    );
    assert_eq!(view["owner_name"], "Alice");
    let (status, _) = bob.post("/api/share/redeem", json!({ "code": share }));
    assert_eq!(status, 404, "a share code works once");
    assert_eq!(host_names(&mut bob), ["second"]);
    let (status, members) = alice.get(&format!("/api/hosts/{id2}/members"));
    assert_eq!(status, 200);
    assert_eq!(members[0]["name"], "Bob");
    let (status, _) = bob.delete(&format!("/api/hosts/{id2}"));
    assert_eq!(status, 403, "a member cannot remove the machine");
    let (status, _) = bob.post(&format!("/api/hosts/{id2}/share-codes"), json!({}));
    assert_eq!(status, 403, "a member cannot share it on");
    let (status, view) = alice.patch(&format!("/api/hosts/{id2}"), json!({ "name": "studio" }));
    assert_eq!(
        (status, view["name"].as_str()),
        (200, Some("studio")),
        "{view}"
    );
    let (status, _) = alice.patch(&format!("/api/hosts/{id2}"), json!({ "name": HOST }));
    assert_eq!(status, 409, "machine names are unique per owner");

    step("T6 push goes to the owner and members of the machine");
    let (port, rx) = fake_push_service();
    subscribe(&mut alice, port, "alice");
    subscribe(&mut bob, port, "bob");
    let notify = |config: &str, seed: [u8; 32]| {
        let dir = anila_e2e::util::run_dir("notify");
        let key = dir.join("k");
        std::fs::write(&key, format!("{}\n", B64.encode(seed))).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let path = dir.join("service.toml");
        std::fs::write(&path, format!("key_file = \"{}\"\n{config}", key.display())).unwrap();
        let out = std::process::Command::new(
            anila_e2e::util::workspace_root().join("target/debug/anila-service"),
        )
        .arg("--config")
        .arg(&path)
        .args(["notify", "--title", "done", "--body", "b"])
        .output()
        .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let out = notify(&config2, SEED2);
    assert!(out.contains("Sent to 2"), "{out}");
    assert_eq!(deliveries(&rx), ["/push/alice", "/push/bob"]);
    let (seed1, _) = vector_key();
    let out = notify(&service_config(&url, &tmux1), seed1);
    assert!(out.contains("Sent to 1"), "{out}");
    assert_eq!(
        deliveries(&rx),
        ["/push/alice"],
        "machine 1 is not shared with Bob"
    );

    step("T7 Bob uses machine 2, then Alice removes him");
    rt.block_on(async {
        let p = probe_host(
            &worker.ws_base(),
            &worker.origin(),
            Some(bob_cookie.clone()),
            3,
            &id2,
        )
        .await;
        wait_connected(&p, 15).await;
        let ch = p.open("two", None, 80, 24, T).await.expect("Bob opens two");
        assert!(p.type_str(ch, "from-bob\r").await);
        p.wait_output(ch, "from-bob", T)
            .await
            .expect("Bob sees the echo");
        let (status, _) = alice.delete(&format!("/api/hosts/{id2}/members/{bob_id}"));
        assert_eq!(status, 200);
        wait_stopped(&p, 4403, 10).await;
        p.shutdown().await;
        assert_eq!(
            ws_status(
                &format!("{}/ws/term?host={id2}", worker.ws_base()),
                &worker.origin(),
                Some(&bob_cookie)
            )
            .await,
            404,
            "Bob lost access"
        );
    });
    assert!(hosts(&mut bob).is_empty());
    let share = code(&mut alice, &format!("/api/hosts/{id2}/share-codes"));
    let (status, _) = bob.post("/api/share/redeem", json!({ "code": share }));
    assert_eq!(status, 200);
    let (status, _) = bob.delete(&format!("/api/hosts/{id2}/members/me"));
    assert_eq!(status, 200, "a member can leave");
    assert!(hosts(&mut bob).is_empty());

    step("T8 Alice removes machine 2");
    rt.block_on(async {
        let p = probe_host(
            &worker.ws_base(),
            &worker.origin(),
            Some(alice_cookie.clone()),
            4,
            &id2,
        )
        .await;
        wait_connected(&p, 15).await;
        let (status, _) = alice.delete(&format!("/api/hosts/{id2}"));
        assert_eq!(status, 200);
        wait_stopped(&p, 4403, 10).await;
        p.shutdown().await;
    });
    assert_eq!(host_names(&mut alice), [HOST]);
    assert!(
        wait_until(Duration::from_secs(10), || service2.log().contains("4403")
            || service2.log().contains("not paired")),
        "the removed machine's service was told:\n{}",
        service2.log()
    );
    service2.stop();

    step("T9 the admin deletes Bob's account");
    let (status, accounts) = alice.get("/api/admin/accounts");
    assert_eq!(status, 200);
    assert_eq!(accounts.as_array().unwrap().len(), 2, "{accounts}");
    let (status, _) = alice.delete(&format!("/api/admin/accounts/{alice_id}"));
    assert_eq!(status, 409, "an admin cannot delete their own account");
    let (status, _) = alice.delete(&format!("/api/admin/accounts/{bob_id}"));
    assert_eq!(status, 200);
    let (status, _) = bob.get("/api/me");
    assert_eq!(status, 401, "Bob's session ended");
    let (status, _) = login(&worker, &mut bob_key);
    assert_eq!(status, 401, "Bob's passkey is gone");
    let (status, accounts) = alice.get("/api/admin/accounts");
    assert_eq!((status, accounts.as_array().unwrap().len()), (200, 1));

    eprintln!("\ntenancy suite passed");
}
