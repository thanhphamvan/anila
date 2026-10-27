use anila_e2e::{
    authn::SoftAuthenticator,
    http::Http,
    probe::{Probe, ProbeOpts},
};
use anila_proto::{Data, client::ClientConfig};
use anila_worker::{
    auth_core,
    webauthn::{self, Expect, WebauthnError},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener as StdListener,
    time::Duration,
};
use tokio::{net::TcpListener, sync::mpsc};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Callback, ErrorResponse, Request, Response},
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

const ORIGIN: &str = "https://term.example.com";
const TIMEOUT: Duration = Duration::from_secs(3);

async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(TIMEOUT, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

struct BrowserHeaders;

impl Callback for BrowserHeaders {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        assert_eq!(request.headers()["origin"], ORIGIN);
        assert_eq!(request.headers()["cookie"], "__Host-term=test-cookie");
        Ok(response)
    }
}

async fn fake_room() -> (
    Probe,
    mpsc::Sender<u16>,
    mpsc::Receiver<u64>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake room");
    let url = format!("ws://{}/ws/term", listener.local_addr().unwrap());
    let (close_tx, mut close_rx) = mpsc::channel::<u16>(8);
    let (resume_tx, resume_rx) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = accept_hdr_async(tcp, BrowserHeaders).await.unwrap();
            let mut offset = 0;
            loop {
                let message = tokio::select! {
                    code = close_rx.recv() => {
                        let Some(code) = code else { return; };
                        if code != 1006 {
                            socket.send(Message::Close(Some(CloseFrame { code: CloseCode::from(code), reason: "test".into() }))).await.unwrap();
                        }
                        break;
                    }
                    message = socket.next() => message,
                };
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let value: Value = serde_json::from_str(&text).unwrap();
                        let response = match value["t"].as_str().unwrap() {
                            "hello" => {
                                json!({"t":"welcome","proto":1,"host":"host","agent":{"online":true}})
                            }
                            "list" => {
                                json!({"t":"sessions","tab":7,"req":value["req"],"items":[{"name":"shell","windows":1,"attached":0,"activity":0,"cmd":"sh"}],"presets":[{"id":"shell","label":"Shell"}]})
                            }
                            "open" => {
                                json!({"t":"opened","tab":7,"ch":value["ch"],"req":value["req"],"epoch":1})
                            }
                            "resume" => {
                                offset = value["from"].as_u64().unwrap();
                                resume_tx.send(offset).await.unwrap();
                                json!({"t":"resumed","tab":7,"ch":value["ch"],"epoch":1,"from":offset,"in_acked":1})
                            }
                            "ping" => json!({"t":"pong"}),
                            _ => continue,
                        };
                        socket
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .unwrap();
                        if value["t"] == "open" {
                            let out = Data::Out {
                                tab: 7,
                                ch: value["ch"].as_u64().unwrap() as u16,
                                epoch: 1,
                                off: 0,
                                compressed: false,
                                bytes: b"hi",
                            }
                            .to_vec()
                            .unwrap();
                            socket.send(Message::Binary(out.into())).await.unwrap();
                            offset = 2;
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        let Data::In { tab, ch, bytes, .. } = Data::decode(&bytes).unwrap() else {
                            panic!("expected input")
                        };
                        let out = Data::Out {
                            tab,
                            ch,
                            epoch: 1,
                            off: offset,
                            compressed: false,
                            bytes,
                        }
                        .to_vec()
                        .unwrap();
                        offset += bytes.len() as u64;
                        socket.send(Message::Binary(out.into())).await.unwrap();
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
        }
    });
    let probe = Probe::start(ProbeOpts {
        url,
        origin: ORIGIN.into(),
        cookie: Some("test-cookie".into()),
        tab: 7,
        config: ClientConfig::default(),
        backoff_scale: 0.1,
    })
    .await;
    until(|| probe.is_connected()).await;
    (probe, close_tx, resume_rx, task)
}

#[tokio::test]
async fn probe_round_trip_and_resume() {
    let (probe, close, mut resumes, room) = fake_room().await;
    assert_eq!(probe.host_online(), Some(true));
    let (sessions, presets) = probe.list(TIMEOUT).await.unwrap();
    assert_eq!(sessions[0].name, "shell");
    assert_eq!(presets[0].id, "shell");
    let ch = probe
        .open("shell", Some("shell"), 80, 24, TIMEOUT)
        .await
        .unwrap();
    probe.wait_output(ch, "hi", TIMEOUT).await.unwrap();
    assert!(probe.type_str(ch, "abc").await);
    probe.wait_output(ch, "hiabc", TIMEOUT).await.unwrap();
    close.send(1006).await.unwrap();
    assert_eq!(
        tokio::time::timeout(TIMEOUT, resumes.recv()).await.unwrap(),
        Some(5)
    );
    until(|| probe.stats().resumes == 1).await;
    assert_eq!(probe.stats().connects, 2);
    assert_eq!(probe.stats().welcomes, 2);
    assert_eq!(probe.stats().closes, [1006]);
    assert_eq!(probe.screen(ch), b"hiabc");
    probe.shutdown().await;
    room.abort();
}

#[tokio::test]
async fn unauthorized_stops_probe() {
    let (probe, close, _, room) = fake_room().await;
    close.send(4401).await.unwrap();
    until(|| !probe.stats().stops.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(probe.stats().stops, [4401]);
    assert_eq!(probe.stats().closes, [4401]);
    assert_eq!(probe.stats().connects, 1);
    assert!(!probe.is_connected());
    probe.shutdown().await;
    room.abort();
}

fn options() -> (Value, Value, Expect<'static>) {
    let challenge = URL_SAFE_NO_PAD.encode(b"challenge");
    (
        json!({"challenge":challenge,"rp":{"id":"term.example.com"},"user":{"id":URL_SAFE_NO_PAD.encode(b"user")}}),
        json!({"challenge":challenge,"rpId":"term.example.com"}),
        Expect {
            challenge: b"challenge",
            origin: ORIGIN,
            rp_id: "term.example.com",
        },
    )
}

fn decode(value: &Value, field: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD
        .decode(value["response"][field].as_str().unwrap())
        .unwrap()
}

#[test]
fn software_authenticator_signatures_flags_and_counters() {
    let (creation, request, expect) = options();
    let mut authenticator = SoftAuthenticator::new(3);
    assert_eq!(
        authenticator.credential_id(),
        SoftAuthenticator::new(3).credential_id()
    );
    assert_ne!(
        authenticator.credential_id(),
        SoftAuthenticator::new(4).credential_id()
    );
    let registration = authenticator.register(&creation, ORIGIN);
    let credential = webauthn::verify_registration(
        &decode(&registration, "clientDataJSON"),
        &decode(&registration, "attestationObject"),
        &expect,
    )
    .unwrap();
    assert_eq!(credential.id, authenticator.credential_id());
    assert_eq!(credential.sign_count, 0);
    assert_eq!(credential.alg, -7);
    for count in 1..=2 {
        let login = authenticator.login(&request, ORIGIN);
        assert_eq!(decode(&login, "userHandle"), b"user");
        assert_eq!(
            webauthn::verify_assertion(
                &decode(&login, "clientDataJSON"),
                &decode(&login, "authenticatorData"),
                &decode(&login, "signature"),
                &credential.public_key_cose,
                count - 1,
                &expect
            ),
            Ok(count)
        );
    }
    authenticator.set_uv(false);
    let login = authenticator.login(&request, ORIGIN);
    assert_eq!(
        webauthn::verify_assertion(
            &decode(&login, "clientDataJSON"),
            &decode(&login, "authenticatorData"),
            &decode(&login, "signature"),
            &credential.public_key_cose,
            2,
            &expect
        ),
        Err(WebauthnError::UserNotVerified)
    );
    let registration = authenticator.register(&creation, ORIGIN);
    assert_eq!(
        webauthn::verify_registration(
            &decode(&registration, "clientDataJSON"),
            &decode(&registration, "attestationObject"),
            &expect
        ),
        Err(WebauthnError::UserNotVerified)
    );
}

#[test]
fn software_authenticator_worker_json_parsers() {
    let (creation, request, expect) = options();
    let mut authenticator = SoftAuthenticator::new(7);
    let registration =
        auth_core::parse_registration(&authenticator.register(&creation, ORIGIN)).unwrap();
    let credential = webauthn::verify_registration(
        &registration.client_data_json,
        &registration.attestation_object,
        &expect,
    )
    .unwrap();
    assert_eq!(registration.id, credential.id);
    let login = auth_core::parse_authentication(&authenticator.login(&request, ORIGIN)).unwrap();
    assert_eq!(login.id, credential.id);
    assert_eq!(login.user_handle, Some(b"user".to_vec()));
    assert_eq!(
        webauthn::verify_assertion(
            &login.client_data_json,
            &login.authenticator_data,
            &login.signature,
            &credential.public_key_cose,
            0,
            &expect
        ),
        Ok(1)
    );
}

#[test]
fn http_cookie_status_and_non_json() {
    let listener = StdListener::bind("127.0.0.1:0").expect("bind HTTP responder");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for index in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.contains(&format!("origin: {}\r\n", ORIGIN)));
            if index == 1 || index == 2 {
                assert!(request.contains("cookie: __host-term=abc\r\n"));
            } else {
                assert!(!request.contains("cookie:"));
            }
            if index == 1 || index == 2 {
                assert!(request.contains("content-type: application/json\r\n"));
            }
            if index == 1 {
                assert!(request.starts_with("post /test "));
                let length: usize = request
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap(),
                    json!({"hello":"world"})
                );
            }
            if index == 2 {
                assert!(request.starts_with("delete /test "));
            }
            let (status, headers, body) = match index {
                0 => (
                    "200 OK",
                    "Set-Cookie: unrelated=x\r\nSet-Cookie: __Host-term=abc; HttpOnly\r\n",
                    "{}",
                ),
                1 => ("403 Forbidden", "", "{\"error\":true}"),
                2 => (
                    "200 OK",
                    "Set-Cookie: __Host-term=gone; Max-Age=0\r\n",
                    "not json",
                ),
                _ => (
                    "204 No Content",
                    "Set-Cookie: __Host-term=; HttpOnly\r\n",
                    "",
                ),
            };
            write!(stream,"HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    let mut http = Http::new(&base, ORIGIN);
    assert_eq!(http.get("/test"), (200, json!({})));
    assert_eq!(http.cookie().as_deref(), Some("abc"));
    assert_eq!(
        http.post("/test", json!({"hello":"world"})),
        (403, json!({"error":true}))
    );
    assert_eq!(http.delete("/test"), (200, Value::Null));
    assert_eq!(http.cookie(), None);
    assert_eq!(http.get("/test"), (204, Value::Null));
    server.join().unwrap();
    assert_eq!(http.get("/test"), (0, Value::Null));
    http.set_cookie(Some("manual".into()));
    assert_eq!(http.cookie().as_deref(), Some("manual"));
    http.set_cookie(None);
    assert_eq!(http.cookie(), None);
}
