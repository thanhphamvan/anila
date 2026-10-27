//! Acceptance tests for `anila_service::pty` (task T2). Do not edit.

use std::process::Command;
use std::time::{Duration, Instant};

use anila_proto::SessionName;
use anila_service::channel::Pty;
use anila_service::pty::{PtyEvent, spawn};
use anila_service::tmux::Tmux;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const KEY: (u32, u16, u32) = (7, 1, 99);

fn sh(script: &str) -> (String, Vec<String>) {
    ("/bin/sh".into(), vec!["-c".into(), script.into()])
}

/// Collects output until `needle` appears; panics after `secs`.
fn wait_for(rx: &mut UnboundedReceiver<PtyEvent>, needle: &str, secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(PtyEvent::Output {
                tab,
                ch,
                epoch,
                bytes,
            }) => {
                assert_eq!((tab, ch, epoch), KEY);
                assert!(bytes.len() <= 32 * 1024);
                seen.extend(bytes);
                let text = String::from_utf8_lossy(&seen).to_string();
                if text.contains(needle) {
                    return text;
                }
            }
            Ok(PtyEvent::Exit { .. }) => panic!(
                "exited before {needle:?}; saw {:?}",
                String::from_utf8_lossy(&seen)
            ),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    panic!(
        "timed out waiting for {needle:?}; saw {:?}",
        String::from_utf8_lossy(&seen)
    );
}

fn wait_exit(rx: &mut UnboundedReceiver<PtyEvent>, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(PtyEvent::Exit { tab, ch, epoch }) => {
                assert_eq!((tab, ch, epoch), KEY);
                // Exactly once: nothing may follow the exit.
                std::thread::sleep(Duration::from_millis(200));
                assert!(rx.try_recv().is_err(), "no events after exit");
                return;
            }
            Ok(PtyEvent::Output { .. }) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    panic!("no exit event within {secs}s");
}

#[test]
fn echoes_input_and_reports_exit() {
    let (tx, mut rx) = unbounded_channel();
    let (prog, args) = sh(r#"printf ready; read x; printf "got:%s" "$x""#);
    let mut p = spawn(&prog, &args, 80, 24, KEY, tx).unwrap();
    wait_for(&mut rx, "ready", 5);
    p.write(b"hi\r").unwrap();
    wait_for(&mut rx, "got:hi", 5);
    wait_exit(&mut rx, 5);
}

#[test]
fn sets_term() {
    let (tx, mut rx) = unbounded_channel();
    let (prog, args) = sh(r#"printf "term=%s;" "$TERM""#);
    let _p = spawn(&prog, &args, 80, 24, KEY, tx).unwrap();
    wait_for(&mut rx, "term=xterm-256color;", 5);
}

#[test]
fn initial_size_and_resize() {
    let (tx, mut rx) = unbounded_channel();
    let (prog, args) = sh(r#"stty size; read x; stty size; read y"#);
    let mut p = spawn(&prog, &args, 80, 24, KEY, tx).unwrap();
    wait_for(&mut rx, "24 80", 5);
    p.resize(100, 30).unwrap();
    p.write(b"\r").unwrap();
    wait_for(&mut rx, "30 100", 5);
    p.kill();
}

#[test]
fn kill_ends_the_child_and_reports_exit() {
    let (tx, mut rx) = unbounded_channel();
    let (prog, args) = sh("printf up; sleep 30");
    let mut p = spawn(&prog, &args, 80, 24, KEY, tx).unwrap();
    wait_for(&mut rx, "up", 5);
    let started = Instant::now();
    p.kill();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "kill must not block"
    );
    wait_exit(&mut rx, 5);
}

#[test]
fn large_output_arrives_complete_in_bounded_chunks() {
    let (tx, mut rx) = unbounded_channel();
    // 200,000 bytes of "x", then a marker.
    let (prog, args) = sh("head -c 200000 /dev/zero | tr '\\0' x; printf END");
    let _p = spawn(&prog, &args, 80, 24, KEY, tx).unwrap();
    let text = wait_for(&mut rx, "END", 10);
    assert_eq!(text.matches('x').count(), 200_000);
}

struct TmuxServer(String);

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .output();
    }
}

fn clients(socket: &str) -> Vec<String> {
    let out = Command::new("tmux")
        .args([
            "-L",
            socket,
            "list-clients",
            "-F",
            "#{client_width}x#{client_height}",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn attaches_to_tmux_and_detaches_on_kill() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed, skipping");
        return;
    }
    let server = TmuxServer(format!("anila-pty-{}", std::process::id()));
    let tmux = Tmux::new(Some(server.0.clone()));
    let name = SessionName::new("work").unwrap();
    tmux.new_session(&name, std::path::Path::new("/tmp"), Some("cat"))
        .unwrap();

    let (tx, mut rx) = unbounded_channel();
    let (prog, args) = tmux.attach_command(&name);
    let mut p = spawn(&prog, &args, 100, 30, KEY, tx).unwrap();
    // Wait for the client to attach first: before tmux takes over the
    // terminal, the PTY itself echoes typed bytes, which would satisfy
    // wait_for without tmux.
    let deadline = Instant::now() + Duration::from_secs(5);
    while clients(&server.0) != ["100x30"] && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(clients(&server.0), ["100x30"]);
    p.write(b"marker-123\r").unwrap();
    wait_for(&mut rx, "marker-123", 5);

    p.kill();
    wait_exit(&mut rx, 5);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !clients(&server.0).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(clients(&server.0).is_empty(), "client detached");
    assert!(
        tmux.has_session(&name).unwrap(),
        "session survives the client"
    );
}

/// portable-pty's writer types "\n" + Ctrl-D into the terminal when it is
/// dropped. While a tmux client is attached, tmux would forward those keys to
/// the user's pane: Enter would submit a half-typed Claude Code prompt and
/// Ctrl-D would end a shell. Closing a channel must never type anything.
#[test]
fn dropping_a_pty_never_types_into_the_session() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed, skipping");
        return;
    }
    for (name, script) in [
        ("eof", "cat"),
        ("enter", "sh -c 'read x; echo GOT-LINE; sleep 100'"),
    ] {
        let server = TmuxServer(format!("anila-pty-drop-{}-{name}", std::process::id()));
        let tmux = Tmux::new(Some(server.0.clone()));
        let session = SessionName::new("work").unwrap();
        tmux.new_session(&session, std::path::Path::new("/tmp"), Some(script))
            .unwrap();

        let (tx, mut rx) = unbounded_channel();
        let (prog, args) = tmux.attach_command(&session);
        let p = spawn(&prog, &args, 80, 24, KEY, tx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while clients(&server.0).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        while rx.try_recv().is_ok() {}
        drop(p);
        std::thread::sleep(Duration::from_millis(800));

        assert!(
            tmux.has_session(&session).unwrap(),
            "{name}: the session must survive closing its PTY"
        );
        let pane = Command::new("tmux")
            .args(["-L", &server.0, "capture-pane", "-p", "-t", "=work"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&pane.stdout).contains("GOT-LINE"),
            "{name}: no Enter may reach the pane"
        );
    }
}
