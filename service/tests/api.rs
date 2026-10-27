use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anila_proto::sig::http_sign_input;
use anila_service::{api, config::Config, keys::ServiceKey};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;
use sha2::{Digest, Sha256};

struct Request {
    line: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn responder(status: &str, body: &str) -> (Config, JoinHandle<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test HTTP responder");
    let config = config(&format!("http://{}", listener.local_addr().unwrap()));
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut headers = HashMap::new();
        loop {
            let mut header = String::new();
            assert_ne!(reader.read_line(&mut header).unwrap(), 0);
            if header == "\r\n" {
                break;
            }
            let (name, value) = header.trim_end().split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let len = headers["content-length"].parse().unwrap();
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        reader.get_mut().write_all(response.as_bytes()).unwrap();
        Request {
            line,
            headers,
            body,
        }
    });
    (config, handle)
}

fn config(worker: &str) -> Config {
    Config::parse(&format!("host = \"test-host\"\nworker = {worker:?}\n")).unwrap()
}

fn vector() -> Value {
    serde_json::from_str(include_str!("../../proto/vectors/agent_sig.json")).unwrap()
}

fn key() -> ServiceKey {
    let vector = vector();
    let hex = vector["seed_hex"].as_str().unwrap();
    let seed: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    ServiceKey::from_seed(seed.try_into().unwrap())
}

fn verify(request: Request, config: &Config, path: &str) {
    assert_eq!(request.line, format!("POST {path} HTTP/1.1\r\n"));
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(request.body, b"{}");
    assert_eq!(request.headers["term-host"], config.host.as_str());
    let ts: i64 = request.headers["term-ts"].parse().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(ts.abs_diff(now) <= 5);
    let hash = format!("{:x}", Sha256::digest(b"{}"));
    let input = http_sign_input("POST", path, ts, &hash);
    let public_key = URL_SAFE_NO_PAD
        .decode(vector()["public_key"].as_str().unwrap())
        .unwrap();
    let public_key = VerifyingKey::from_bytes(&public_key.try_into().unwrap()).unwrap();
    let signature = URL_SAFE_NO_PAD
        .decode(&request.headers["term-sig"])
        .unwrap();
    public_key
        .verify(
            input.as_bytes(),
            &Signature::from_slice(&signature).unwrap(),
        )
        .unwrap();
}

#[test]
fn enroll_sends_signed_json_and_parses_code() {
    let (config, responder) =
        responder("200 OK", r#"{"code":"ABCD-EFGH","expires_at":1792586134}"#);
    let code = api::enroll(&config, &key()).unwrap();
    assert_eq!(code.code, "ABCD-EFGH");
    assert_eq!(code.expires_at, 1792586134);
    verify(responder.join().unwrap(), &config, "/api/agent/enroll-code");
}

#[test]
fn revoke_all_sends_signed_json() {
    let (config, responder) = responder("204 No Content", "");
    api::revoke_all(&config, &key()).unwrap();
    verify(responder.join().unwrap(), &config, "/api/agent/revoke-all");
}

#[test]
fn worker_errors_are_actionable() {
    for (status, body, expected) in [
        (
            "401 Unauthorized",
            "",
            vec!["not paired", "anila-service pair", "clock"],
        ),
        (
            "500 Internal Server Error",
            r#"{"error":{"code":"internal","msg":"boom"}}"#,
            vec!["500", "boom"],
        ),
        ("502 Bad Gateway", "not JSON", vec!["502"]),
        ("302 Found", "", vec!["302"]),
    ] {
        for enroll in [true, false] {
            let (config, responder) = responder(status, body);
            let result = if enroll {
                api::enroll(&config, &key()).map(|_| ())
            } else {
                api::revoke_all(&config, &key())
            };
            let message = result.unwrap_err().to_string();
            responder.join().unwrap();
            for expected in &expected {
                assert!(message.contains(expected), "{message}");
            }
        }
    }
}

#[test]
fn unreachable_worker_reports_url() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = config(&format!("http://{}", listener.local_addr().unwrap()));
    drop(listener);
    let message = api::enroll(&config, &key()).unwrap_err().to_string();
    assert!(message.contains("could not reach"), "{message}");
    assert!(
        message.contains(&format!("{}/api/agent/enroll-code", config.worker)),
        "{message}"
    );
}

#[test]
fn trailing_slash_preserves_signed_paths() {
    for enroll in [true, false] {
        let (mut config, responder) =
            responder("200 OK", r#"{"code":"ABCD-EFGH","expires_at":1792586134}"#);
        config.worker.push('/');
        let path = if enroll {
            api::enroll(&config, &key()).unwrap();
            "/api/agent/enroll-code"
        } else {
            api::revoke_all(&config, &key()).unwrap();
            "/api/agent/revoke-all"
        };
        verify(responder.join().unwrap(), &config, path);
    }
}

#[test]
fn notify_signs_exact_json_and_parses_counts() {
    for session in [Some("claude"), None] {
        let (config, responder) = responder("200 OK", r#"{"sent":2,"removed":1,"failed":3}"#);
        let result =
            api::notify(&config, &key(), "Done ✓", "Say \"hello\"\nagain", session).unwrap();
        assert_eq!((result.sent, result.removed, result.failed), (2, 1, 3));
        let request = responder.join().unwrap();
        assert_eq!(request.line, "POST /api/agent/notify HTTP/1.1\r\n");
        assert_eq!(request.headers["content-type"], "application/json");
        assert_eq!(request.headers["term-host"], config.host.as_str());
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let mut expected = serde_json::json!({"title": "Done ✓", "body": "Say \"hello\"\nagain"});
        if let Some(session) = session {
            expected["session"] = session.into();
        }
        assert_eq!(body, expected);
        let ts = request.headers["term-ts"].parse().unwrap();
        let hash = format!("{:x}", Sha256::digest(&request.body));
        let input = http_sign_input("POST", "/api/agent/notify", ts, &hash);
        let public = URL_SAFE_NO_PAD.decode(key().public_key_b64()).unwrap();
        let public = VerifyingKey::from_bytes(&public.try_into().unwrap()).unwrap();
        let signature = URL_SAFE_NO_PAD
            .decode(&request.headers["term-sig"])
            .unwrap();
        public
            .verify(
                input.as_bytes(),
                &Signature::from_slice(&signature).unwrap(),
            )
            .unwrap();
    }
}

#[test]
fn notify_503_explains_vapid_setup() {
    let (config, responder) = responder("503 Service Unavailable", "");
    let error = api::notify(&config, &key(), "Done", "Ready", None).unwrap_err();
    responder.join().unwrap();
    assert_eq!(
        error.to_string(),
        "Push is not configured on the worker: run `anila-service vapid-keygen` and set VAPID_PRIVATE_KEY."
    );
}

#[test]
fn claude_hook_sends_expected_arguments_without_waiting() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let dir = std::env::temp_dir().join(format!(
        "anila-hook-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let fake = dir.join("anila-service");
    std::fs::write(&fake, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOOK_ARGS.tmp\"\nmv \"$HOOK_ARGS.tmp\" \"$HOOK_ARGS\"\nsleep 10\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (index, input, title, body) in [
        (
            0,
            r#"{"hook_event_name":"Stop"}"#,
            "Claude finished",
            "Ready for your next prompt",
        ),
        (
            1,
            r#"{"hook_event_name":"Notification","message":"Approve the edit?"}"#,
            "Claude needs you",
            "Approve the edit?",
        ),
    ] {
        let args = dir.join(format!("args-{index}"));
        let start = std::time::Instant::now();
        let mut child = Command::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/hooks/claude-notify.sh"
        ))
        .env(
            "PATH",
            format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
        )
        .env("HOOK_ARGS", &args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
        assert!(start.elapsed() < Duration::from_secs(2));
        while !args.exists() && start.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(args).unwrap(),
            format!("notify\n--title\n{title}\n--body\n{body}\n")
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}
