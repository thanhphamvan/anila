//! Persistence and unsupported-store guards without private release tags.

mod common;

use std::process::Command;

use anila_e2e::authn::SoftAuthenticator;
use anila_e2e::http::Http;
use anila_e2e::stack::{Worker, WorkerOpts};
use anila_e2e::tmux::TmuxServer;
use anila_e2e::util::run_dir;
use common::*;
use serde_json::json;

#[test]
fn passkey_store_survives_restart_and_rejects_newer_schema() {
    let state = run_dir("persisted-auth-state");
    let tmux = TmuxServer::new("e2e-persistence");
    let mut key = SoftAuthenticator::new(12);
    let start = || {
        let (_, public) = vector_key();
        Worker::start(WorkerOpts {
            host_keys: json!({ HOST: public }).to_string(),
            default_host: HOST.into(),
            state: Some(state.clone()),
            ..Default::default()
        })
        .expect("worker starts")
    };
    let (cookie, account) = {
        let worker = start();
        let code = enroll_code(&worker.http_base(), &tmux);
        let mut http = register(&worker, &code, &mut key);
        let (status, me) = http.get("/api/me");
        assert_eq!(status, 200);
        (http.cookie().unwrap(), me["account"]["id"].clone())
    };
    {
        let worker = start();
        let mut http = Http::new(&worker.http_base(), &worker.origin());
        http.set_cookie(Some(cookie.clone()));
        let (status, me) = http.get("/api/me");
        assert_eq!(status, 200, "persisted passkey session works");
        assert_eq!(me["account"]["id"], account);
        assert_eq!(login(&worker, &mut key).0, 204);
        let (status, hosts) = http.get("/api/hosts");
        assert_eq!(status, 200);
        assert_eq!(hosts[0]["role"], "owner");
    }
    // Only edit this test's isolated, stopped Durable Object store. Never
    // open a running or real deployment database.
    let output = Command::new("python3")
        .args([
            "-c",
            r#"
import pathlib, sqlite3, sys
found = 0
for path in pathlib.Path(sys.argv[1]).rglob('*.sqlite'):
    with sqlite3.connect(path) as db:
        if db.execute("SELECT name FROM sqlite_master WHERE name='meta'").fetchone():
            db.execute("UPDATE meta SET v='3' WHERE k='schema'")
            found += 1
assert found == 1, 'expected exactly one isolated auth store'
"#,
        ])
        .arg(&state)
        .output()
        .expect("python3 seeds an unsupported schema");
    assert!(
        output.status.success(),
        "fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let worker = start();
    let mut http = Http::new(&worker.http_base(), &worker.origin());
    http.set_cookie(Some(cookie));
    assert_eq!(http.get("/api/me").0, 500, "newer store fails closed");
    assert_eq!(http.post("/api/passkeys/login/options", json!({})).0, 500);
}
