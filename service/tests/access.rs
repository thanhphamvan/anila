//! Cloudflare Access service tokens on the service's calls to the worker.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};

use anila_service::{api, config::Config, keys::ServiceKey, link};

/// Answers one request with `head` (status line and headers) and `body`,
/// returning the request's headers.
fn responder(head: &str, body: &str) -> (String, JoinHandle<HashMap<String, String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut headers = HashMap::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" {
                break;
            }
            let (name, value) = header.trim_end().split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let len = headers
            .get("content-length")
            .map_or(0, |l| l.parse().unwrap());
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        reader.get_mut().write_all(response.as_bytes()).unwrap();
        headers
    });
    (url, handle)
}

fn config(worker: &str, access: bool) -> Config {
    let token = if access {
        "access_client_id = \"abc.access\"\naccess_client_secret = \"s3cr3t\"\n"
    } else {
        ""
    };
    Config::parse(&format!(
        "host = \"test-host\"\nworker = {worker:?}\n{token}"
    ))
    .unwrap()
}

#[test]
fn signed_calls_carry_the_service_token() {
    let (url, handle) = responder(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json",
        r#"{"code":"ABCD-EFGH","expires_at":1}"#,
    );
    let code = api::enroll(&config(&url, true), &ServiceKey::from_seed([1; 32])).unwrap();
    assert_eq!(code.code, "ABCD-EFGH");
    let headers = handle.join().unwrap();
    assert_eq!(headers["cf-access-client-id"], "abc.access");
    assert_eq!(headers["cf-access-client-secret"], "s3cr3t");
    assert!(headers.contains_key("term-sig"), "still signed");
}

#[test]
fn no_token_no_headers() {
    let (url, handle) = responder(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json",
        r#"{"code":"ABCD-EFGH","expires_at":1}"#,
    );
    api::enroll(&config(&url, false), &ServiceKey::from_seed([1; 32])).unwrap();
    let headers = handle.join().unwrap();
    assert!(!headers.contains_key("cf-access-client-id"));
}

fn access_redirect_error(token: bool) -> String {
    let (url, handle) = responder(
        "HTTP/1.1 302 Found\r\nLocation: https://team.cloudflareaccess.com/cdn-cgi/access/login/x",
        "",
    );
    let error = api::enroll(&config(&url, token), &ServiceKey::from_seed([1; 32]))
        .unwrap_err()
        .to_string();
    handle.join().unwrap();
    error
}

#[test]
fn an_access_redirect_says_what_to_do() {
    let missing = access_redirect_error(false);
    assert_eq!(missing, api::access_blocked(false));
    for hint in [
        "access_client_id",
        "access_client_secret",
        "before any [[preset]]",
        "ANILA_ACCESS_CLIENT_ID",
        "restart",
    ] {
        assert!(missing.contains(hint), "{hint}: {missing}");
    }
    let rejected = access_redirect_error(true);
    assert_eq!(rejected, api::access_blocked(true));
    assert!(rejected.contains("Service Auth policy"), "{rejected}");
    assert!(!rejected.contains("s3cr3t"), "never echoes the secret");
}

#[test]
fn the_agent_socket_carries_the_service_token() {
    let request = link::upgrade_request(
        "wss://sys.example.com/ws/agent?host=h1",
        &[
            ("cf-access-client-id", "abc.access".into()),
            ("cf-access-client-secret", "s3cr3t".into()),
        ],
    )
    .unwrap();
    assert_eq!(request.headers()["cf-access-client-id"], "abc.access");
    assert_eq!(request.headers()["cf-access-client-secret"], "s3cr3t");
    assert_eq!(request.uri().path(), "/ws/agent");
    let plain = link::upgrade_request("wss://sys.example.com/ws/agent?host=h1", &[]).unwrap();
    assert!(plain.headers().get("cf-access-client-id").is_none());
}
