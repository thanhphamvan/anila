//! Acceptance tests for `anila_proto::client` (task T8). Do not edit.

use anila_proto::client::{Client, ClientConfig, CloseAction, Event, Frame, backoff_ms};
use anila_proto::control::PING_TEXT;
use anila_proto::{AgentStatus, Create, Data, ErrorCode, Hop, PROTO, SessionName, ToAgent};
use serde_json::{Value, json};

const TAB: u32 = 7;

fn client() -> Client {
    Client::new(TAB, ClientConfig::default())
}

fn name(s: &str) -> SessionName {
    SessionName::new(s).unwrap()
}

fn text(f: &Frame) -> Value {
    match f {
        Frame::Text(t) => serde_json::from_str(t).unwrap(),
        Frame::Binary(_) => panic!("expected text, got binary"),
    }
}

fn texts(frames: &[Frame]) -> Vec<Value> {
    frames
        .iter()
        .filter(|f| matches!(f, Frame::Text(_)))
        .map(text)
        .collect()
}

fn ins(frames: &[Frame]) -> Vec<(u32, Vec<u8>)> {
    frames
        .iter()
        .filter_map(|f| match f {
            Frame::Binary(b) => match Data::decode(b).unwrap() {
                Data::In { tab, ch, id, bytes } => {
                    assert_eq!((tab, ch), (TAB, 1));
                    Some((id, bytes.to_vec()))
                }
                Data::Out { .. } => panic!("client never sends OUT"),
            },
            _ => None,
        })
        .collect()
}

fn welcome(online: bool) -> String {
    json!({ "t": "welcome", "proto": PROTO, "host": "my-mac",
            "agent": { "online": online, "codecs": ["none"] } })
    .to_string()
}

fn out(epoch: u32, off: u64, bytes: &[u8]) -> Vec<u8> {
    Data::Out {
        tab: TAB,
        ch: 1,
        epoch,
        off,
        compressed: false,
        bytes,
    }
    .to_vec()
    .unwrap()
}

/// Connected client with channel 1 open on "claude" at epoch 50.
fn opened() -> Client {
    let mut c = client();
    c.on_open(0);
    c.on_text(&welcome(true), 0);
    let (ch, req, frame) = c.open(name("claude"), None, 80, 24);
    assert_eq!(ch, 1);
    assert!(frame.is_some());
    let (_, events) = c.on_text(
        &json!({ "t": "opened", "tab": TAB, "ch": 1, "req": req, "epoch": 50 }).to_string(),
        0,
    );
    assert_eq!(events, vec![Event::Opened { ch: 1, req }]);
    c
}

#[test]
fn hello_welcome_and_list() {
    let mut c = client();
    assert!(!c.is_connected());
    let hello = c.on_open(0);
    assert_eq!(
        texts(&hello),
        vec![json!({ "t": "hello", "proto": PROTO, "tab": TAB })]
    );
    assert_eq!(c.list().1, None, "nothing is sent before welcome");

    let (frames, events) = c.on_text(&welcome(true), 5);
    assert!(frames.is_empty());
    assert!(c.is_connected());
    match events.as_slice() {
        [Event::Connected { host, agent }] => {
            assert_eq!(host, "my-mac");
            assert!(agent.online);
        }
        other => panic!("unexpected {other:?}"),
    }

    let (req, frame) = c.list();
    let v = text(&frame.unwrap());
    assert_eq!(v, json!({ "t": "list", "tab": TAB, "req": req }));
    let (_, events) = c.on_text(
        &json!({ "t": "sessions", "tab": TAB, "req": req, "items": [], "presets": [{ "id": "shell", "label": "Shell" }] }).to_string(),
        6,
    );
    assert!(matches!(events.as_slice(), [Event::Sessions { req: r, .. }] if *r == req));
}

#[test]
fn open_frame_shape() {
    let mut c = client();
    c.on_open(0);
    c.on_text(&welcome(true), 0);
    let (ch, req, frame) = c.open(
        name("notes"),
        Some(Create {
            preset: "shell".into(),
        }),
        100,
        30,
    );
    let v = text(&frame.unwrap());
    let parsed: ToAgent = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(
        parsed,
        ToAgent::Open {
            tab: TAB,
            ch,
            req,
            session: name("notes"),
            create: Some(Create {
                preset: "shell".into()
            }),
            cols: 100,
            rows: 30,
            codec: Default::default()
        }
    );
    let (ch2, req2, _) = c.open(name("notes"), None, 100, 30);
    assert_ne!(ch, ch2, "each open gets a new channel");
    assert_ne!(req, req2, "and a new request id");
}

#[test]
fn output_in_order_trims_duplicates_and_ignores_other_epochs() {
    let mut c = opened();
    let (_, e) = c.on_binary(&out(50, 0, b"hello "), 1);
    assert_eq!(
        e,
        vec![Event::Output {
            ch: 1,
            bytes: b"hello ".to_vec()
        }]
    );
    // Overlap after a replay: only the new tail is rendered.
    let (_, e) = c.on_binary(&out(50, 3, b"lo world"), 2);
    assert_eq!(
        e,
        vec![Event::Output {
            ch: 1,
            bytes: b"world".to_vec()
        }]
    );
    // Fully duplicate, other epoch, and a gap: all ignored.
    assert!(c.on_binary(&out(50, 0, b"hel"), 3).1.is_empty());
    assert!(c.on_binary(&out(49, 11, b"zzz"), 3).1.is_empty());
    assert!(c.on_binary(&out(50, 99, b"gap"), 3).1.is_empty());
    assert_eq!(c.rendered(1), Some(11));
    assert_eq!(c.epoch(1), Some(50));
}

#[test]
fn acks_on_timer_and_on_volume() {
    let mut c = opened();
    c.on_binary(&out(50, 0, b"abc"), 10);
    assert!(
        texts(&c.tick(100).0).iter().all(|v| v["t"] != "ack"),
        "not before ack_ms"
    );
    let frames = c.tick(250).0;
    let acks: Vec<Value> = texts(&frames)
        .into_iter()
        .filter(|v| v["t"] == "ack")
        .collect();
    assert_eq!(
        acks,
        vec![json!({ "t": "ack", "tab": TAB, "ch": 1, "epoch": 50, "off": 3 })]
    );
    assert!(
        texts(&c.tick(600).0).iter().all(|v| v["t"] != "ack"),
        "nothing new, no ack"
    );

    // 64 KiB rendered triggers an ack at once (two maximum-size frames).
    let half = vec![b'x'; 32 * 1024];
    let (frames, _) = c.on_binary(&out(50, 3, &half), 601);
    assert!(
        texts(&frames).iter().all(|v| v["t"] != "ack"),
        "32 KiB is below ack_bytes"
    );
    let (frames, _) = c.on_binary(&out(50, 3 + 32 * 1024, &half), 602);
    let acks: Vec<Value> = texts(&frames)
        .into_iter()
        .filter(|v| v["t"] == "ack")
        .collect();
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0]["off"], 3 + 64 * 1024);
}

#[test]
fn input_ids_and_splitting() {
    let mut c = opened();
    assert_eq!(ins(&c.input(1, b"ls")), vec![(1, b"ls".to_vec())]);
    assert_eq!(ins(&c.input(1, b"\r")), vec![(2, b"\r".to_vec())]);
    let paste = vec![b'p'; 4096 + 10];
    let frames = c.input(1, &paste);
    let parts = ins(&frames);
    assert_eq!(parts.len(), 2, "IN payloads are at most 4 KiB");
    assert_eq!((parts[0].0, parts[1].0), (3, 4));
    assert_eq!(parts[0].1.len() + parts[1].1.len(), paste.len());
    assert!(c.input(9, b"x").is_empty(), "unknown channel");
}

#[test]
fn reconnect_resumes_and_resends_unacked_input() {
    let mut c = opened();
    c.on_binary(&out(50, 0, b"0123456789"), 1);
    c.input(1, b"a");
    c.input(1, b"b");
    c.input(1, b"c");
    c.on_text(
        &json!({ "t": "in_ack", "tab": TAB, "ch": 1, "id": 1 }).to_string(),
        2,
    );

    assert_eq!(c.on_close(1006), CloseAction::Reconnect);
    assert!(!c.is_connected());
    assert!(
        c.input(1, b"lost").is_empty(),
        "input is paused while disconnected"
    );

    let hello = c.on_open(1000);
    assert_eq!(texts(&hello)[0]["t"], "hello");
    let (frames, _) = c.on_text(&welcome(true), 1001);
    assert_eq!(
        texts(&frames),
        vec![
            json!({ "t": "resume", "tab": TAB, "ch": 1, "session": "claude", "epoch": 50, "from": 10, "cols": 80, "rows": 24, "codec": "none" })
        ]
    );
    assert!(
        c.input(1, b"early").is_empty(),
        "paused until the channel resumes"
    );

    // The service applied id 2 before the drop.
    let (frames, events) = c.on_text(
        &json!({ "t": "resumed", "tab": TAB, "ch": 1, "epoch": 50, "from": 10, "in_acked": 2 })
            .to_string(),
        1002,
    );
    assert!(events.is_empty() || events.iter().all(|e| !matches!(e, Event::Reset { .. })));
    assert_eq!(
        ins(&frames),
        vec![(3, b"c".to_vec())],
        "only unacked input is resent, same id"
    );
    assert_eq!(ins(&c.input(1, b"d")), vec![(4, b"d".to_vec())]);
    let (_, e) = c.on_binary(&out(50, 10, b"ABC"), 1003);
    assert_eq!(
        e,
        vec![Event::Output {
            ch: 1,
            bytes: b"ABC".to_vec()
        }]
    );
}

#[test]
fn reset_clears_screen_offsets_and_pending_input() {
    let mut c = opened();
    c.on_binary(&out(50, 0, b"old"), 1);
    c.input(1, b"x");
    c.on_close(1001);
    c.on_open(10);
    c.on_text(&welcome(true), 11);
    let (frames, events) = c.on_text(
        &json!({ "t": "reset", "tab": TAB, "ch": 1, "epoch": 77 }).to_string(),
        12,
    );
    assert_eq!(events, vec![Event::Reset { ch: 1 }]);
    assert!(
        ins(&frames).is_empty(),
        "input from before a reset is dropped, never replayed"
    );
    assert_eq!(c.epoch(1), Some(77));
    assert_eq!(c.rendered(1), Some(0));
    let (_, e) = c.on_binary(&out(77, 0, b"new"), 13);
    assert_eq!(
        e,
        vec![Event::Output {
            ch: 1,
            bytes: b"new".to_vec()
        }]
    );
    assert_eq!(
        ins(&c.input(1, b"y")),
        vec![(2, b"y".to_vec())],
        "ids keep increasing"
    );
}

#[test]
fn resume_waits_for_the_host_to_come_online() {
    let mut c = opened();
    c.on_close(1006);
    c.on_open(0);
    let (frames, events) = c.on_text(&welcome(false), 1);
    assert!(
        texts(&frames).is_empty(),
        "no resume while the host is offline"
    );
    assert!(matches!(events.as_slice(), [Event::Connected { agent, .. }] if !agent.online));

    let online = serde_json::to_string(&Hop::Status(AgentStatus {
        online: true,
        since: Some(5),
        proto: Some(PROTO),
        codecs: vec![],
    }))
    .unwrap();
    let (frames, events) = c.on_text(&online, 2);
    assert_eq!(texts(&frames).len(), 1);
    assert_eq!(texts(&frames)[0]["t"], "resume");
    assert!(matches!(events.as_slice(), [Event::Status(s)] if s.online));

    // A repeated online status while a resume is already pending sends nothing more.
    let (frames, _) = c.on_text(&online, 3);
    assert!(texts(&frames).is_empty());

    // Offline then online again after resuming: resume again.
    c.on_text(
        &json!({ "t": "resumed", "tab": TAB, "ch": 1, "epoch": 50, "from": 0, "in_acked": 0 })
            .to_string(),
        4,
    );
    let offline = serde_json::to_string(&Hop::Status(AgentStatus {
        online: false,
        since: Some(6),
        proto: None,
        codecs: vec![],
    }))
    .unwrap();
    c.on_text(&offline, 5);
    assert!(
        c.input(1, b"x").is_empty(),
        "paused while the host is offline"
    );
    let (frames, _) = c.on_text(&online, 6);
    assert_eq!(texts(&frames)[0]["t"], "resume");
}

#[test]
fn open_lost_in_a_drop_is_sent_again() {
    let mut c = client();
    c.on_open(0);
    c.on_text(&welcome(true), 0);
    let (ch, req, _) = c.open(name("claude"), None, 80, 24);
    c.on_close(1006);
    c.on_open(10);
    let (frames, _) = c.on_text(&welcome(true), 11);
    let v = texts(&frames);
    assert_eq!(v.len(), 1);
    assert_eq!(
        (v[0]["t"].clone(), v[0]["ch"].clone(), v[0]["req"].clone()),
        (json!("open"), json!(ch), json!(req))
    );

    // Opening while disconnected is sent on the next welcome too.
    c.on_close(1006);
    let (ch2, _, frame) = c.open(name("notes"), None, 80, 24);
    assert!(frame.is_none());
    c.on_open(20);
    let (frames, _) = c.on_text(&welcome(true), 21);
    assert!(
        texts(&frames)
            .iter()
            .any(|v| v["t"] == "open" && v["ch"] == ch2)
    );
}

#[test]
fn exited_and_failed_open_remove_the_channel() {
    let mut c = opened();
    let (_, e) = c.on_text(
        &json!({ "t": "exited", "tab": TAB, "ch": 1, "reason": "session ended" }).to_string(),
        1,
    );
    assert_eq!(
        e,
        vec![Event::Exited {
            ch: 1,
            reason: "session ended".into()
        }]
    );
    assert_eq!(c.epoch(1), None);
    assert!(c.input(1, b"x").is_empty());

    let (ch, req, _) = c.open(name("ghost"), None, 80, 24);
    let (_, e) = c.on_text(
        &json!({ "t": "error", "tab": TAB, "ch": ch, "req": req, "code": "not_found", "msg": "session not found" }).to_string(),
        2,
    );
    assert!(matches!(
        e.as_slice(),
        [Event::Error {
            code: ErrorCode::NotFound,
            ..
        }]
    ));
    c.on_close(1006);
    c.on_open(3);
    let (frames, _) = c.on_text(&welcome(true), 4);
    assert!(texts(&frames).is_empty(), "a failed open is not retried");
}

#[test]
fn pings_and_dead_link() {
    let mut c = opened();
    let (frames, _) = c.tick(9_000);
    assert!(!frames.contains(&Frame::Text(PING_TEXT.into())));
    let (frames, _) = c.tick(10_000);
    assert!(
        frames.contains(&Frame::Text(PING_TEXT.into())),
        "ping every 10 s"
    );
    let (frames, _) = c.tick(10_100);
    assert!(!frames.contains(&Frame::Text(PING_TEXT.into())));

    // Any frame from the room counts as life.
    c.on_text(r#"{"t":"pong"}"#, 20_000);
    assert!(!c.tick(44_000).1.contains(&Event::LinkDead));
    assert!(
        c.tick(45_000).1.contains(&Event::LinkDead),
        "dead 25 s after the last frame"
    );
    assert!(
        !c.tick(46_000).1.contains(&Event::LinkDead),
        "reported once"
    );
}

#[test]
fn close_codes() {
    let mut c = opened();
    for code in [1000, 1001, 1006, 1009, 1011, 4408] {
        assert_eq!(c.on_close(code), CloseAction::Reconnect, "code {code}");
    }
    for code in [4400, 4401, 4403, 4409] {
        assert!(
            matches!(c.on_close(code), CloseAction::Stop { code: c2, .. } if c2 == code),
            "code {code}"
        );
    }
}

#[test]
fn backoff_schedule() {
    assert_eq!(backoff_ms(0, 0.5), 500);
    assert_eq!(backoff_ms(1, 0.5), 1000);
    assert_eq!(backoff_ms(2, 0.5), 2000);
    assert_eq!(backoff_ms(3, 0.5), 4000);
    assert_eq!(backoff_ms(4, 0.5), 5000);
    assert_eq!(backoff_ms(40, 0.5), 5000);
    assert_eq!(backoff_ms(0, 0.0), 400);
    assert_eq!(backoff_ms(4, 1.0), 6000);
}

#[test]
fn garbage_frames_never_panic() {
    let mut c = opened();
    let mut x: u32 = 12345;
    for len in 0..300 {
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        c.on_binary(&bytes, 1);
        c.on_text(&String::from_utf8_lossy(&bytes), 1);
    }
    c.on_text(r#"{"t":"sessions","tab":7}"#, 1);
    c.on_text(r#"{"t":"opened","tab":8,"ch":1,"req":1,"epoch":1}"#, 1);
}

#[test]
fn deflate_negotiation_output_reset_and_recovery() {
    use anila_proto::{Codec, codec::Encoder};
    let mut c = client();
    c.on_open(0);
    c.on_text(&json!({"t":"welcome", "proto":PROTO, "agent":{"online":true,"codecs":["deflate_stream","none"]}}).to_string(), 0);
    let (ch, req, frame) = c.open(name("claude"), None, 80, 24);
    assert_eq!(text(&frame.unwrap())["codec"], "deflate_stream");
    c.on_text(
        &json!({"t":"opened","tab":TAB,"ch":ch,"req":req,"epoch":50}).to_string(),
        0,
    );
    let mut encoder = Encoder::new(Codec::DeflateStream);
    let mut off = 0;
    for bytes in [b"hello terminal".as_slice(), b"hello terminal again"] {
        let frame = Data::Out {
            tab: TAB,
            ch,
            epoch: 50,
            off,
            compressed: true,
            bytes: &encoder.encode(bytes),
        }
        .to_vec()
        .unwrap();
        assert_eq!(
            c.on_binary(&frame, 1).1,
            vec![Event::Output {
                ch,
                bytes: bytes.to_vec()
            }]
        );
        off += bytes.len() as u64;
        assert_eq!(c.rendered(ch), Some(off));
    }
    c.on_text(
        &json!({"t":"reset","tab":TAB,"ch":ch,"epoch":51}).to_string(),
        2,
    );
    let bytes = b"new terminal";
    let frame = Data::Out {
        tab: TAB,
        ch,
        epoch: 51,
        off: 0,
        compressed: true,
        bytes: &Encoder::new(Codec::DeflateStream).encode(bytes),
    }
    .to_vec()
    .unwrap();
    assert_eq!(
        c.on_binary(&frame, 3).1,
        vec![Event::Output {
            ch,
            bytes: bytes.to_vec()
        }]
    );
    let corrupt = Data::Out {
        tab: TAB,
        ch,
        epoch: 51,
        off: bytes.len() as u64,
        compressed: true,
        bytes: &[255, 0, 0, 255, 255],
    }
    .to_vec()
    .unwrap();
    assert!(c.on_binary(&corrupt, 4).1.is_empty());
    assert_eq!(c.rendered(ch), Some(bytes.len() as u64));
    let frames = texts(&c.tick(5).0);
    assert_eq!(
        frames,
        vec![
            json!({"t":"resume","tab":TAB,"ch":ch,"session":"claude","epoch":51,"from":bytes.len(),"cols":80,"rows":24,"codec":"deflate_stream"})
        ]
    );
    assert!(c.tick(6).0.is_empty());
    c.on_text(
        &json!({"t":"resumed","tab":TAB,"ch":ch,"epoch":51,"from":bytes.len(),"in_acked":0})
            .to_string(),
        7,
    );
    let frame = Data::Out {
        tab: TAB,
        ch,
        epoch: 51,
        off: bytes.len() as u64,
        compressed: true,
        bytes: &Encoder::new(Codec::DeflateStream).encode(b"recovered"),
    }
    .to_vec()
    .unwrap();
    assert_eq!(
        c.on_binary(&frame, 8).1,
        vec![Event::Output {
            ch,
            bytes: b"recovered".to_vec()
        }]
    );
}

#[test]
fn codec_negotiation_tracks_status_and_browser_preferences() {
    use anila_proto::Codec;
    let mut c = client();
    c.on_open(0);
    c.on_text(&welcome(true), 0);
    assert_eq!(
        text(&c.open(name("claude"), None, 80, 24).2.unwrap())["codec"],
        "none"
    );
    c.on_text(
        &json!({"t":"status","online":true,"codecs":["deflate_stream","none"]}).to_string(),
        1,
    );
    assert_eq!(
        text(&c.open(name("notes"), None, 80, 24).2.unwrap())["codec"],
        "deflate_stream"
    );
    let mut c = Client::new(
        TAB,
        ClientConfig {
            codecs: vec![Codec::None],
            ..ClientConfig::default()
        },
    );
    c.on_open(0);
    c.on_text(&json!({"t":"welcome","proto":PROTO,"agent":{"online":true,"codecs":["deflate_stream","none"]}}).to_string(), 0);
    assert_eq!(
        text(&c.open(name("claude"), None, 80, 24).2.unwrap())["codec"],
        "none"
    );
}

#[test]
fn compressed_gaps_and_duplicates_require_resume() {
    use anila_proto::{Codec, codec::Encoder};
    for bad_off in [0, 2, 4] {
        let mut c = client();
        c.on_open(0);
        c.on_text(&json!({"t":"welcome","proto":PROTO,"agent":{"online":true,"codecs":["deflate_stream"]}}).to_string(), 0);
        c.open(name("claude"), None, 80, 24);
        c.on_text(
            &json!({"t":"opened","tab":TAB,"ch":1,"req":1,"epoch":50}).to_string(),
            0,
        );
        let mut encoder = Encoder::new(Codec::DeflateStream);
        let frame = Data::Out {
            tab: TAB,
            ch: 1,
            epoch: 50,
            off: 0,
            compressed: true,
            bytes: &encoder.encode(b"abc"),
        }
        .to_vec()
        .unwrap();
        assert_eq!(c.on_binary(&frame, 1).1.len(), 1);
        let frame = Data::Out {
            tab: TAB,
            ch: 1,
            epoch: 50,
            off: bad_off,
            compressed: true,
            bytes: &encoder.encode(b"def"),
        }
        .to_vec()
        .unwrap();
        assert!(c.on_binary(&frame, 2).1.is_empty());
        assert_eq!(c.rendered(1), Some(3));
        let frames = texts(&c.tick(3).0);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["t"], "resume");
        assert_eq!(frames[0]["from"], 3);
    }
}
