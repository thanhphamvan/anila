//! Real herdr on isolated XDG directories. Never connects to the user's server.
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anila_proto::SessionName;
use anila_service::{
    channel::{CreateError, Spawner},
    config::Config,
    herdr::Herdr,
    pty::PtyEvent,
    spawner::SessionSpawner,
    tmux::Tmux,
};

#[test]
fn real_herdr_lifecycle() {
    let Ok(binary) = std::env::var("ANILA_HERDR_BINARY") else {
        eprintln!("set ANILA_HERDR_BINARY to run the isolated real-herdr regression");
        return;
    };
    let dir = std::env::temp_dir().join(format!(
        "anila-herdr-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = format!("anila-herdr-{}", std::process::id());
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "isolated_herdr_fixture",
            "--nocapture",
        ])
        .env("ANILA_HERDR_BINARY", &binary)
        .env("PS1", "ANILA-PROMPT> ")
        .env("ANILA_HERDR_FIXTURE", &dir)
        .env("ANILA_HERDR_TMUX_SOCKET", &socket)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("HOME", &dir)
        .env_remove("HERDR_CONFIG_PATH")
        .env_remove("HERDR_SESSION")
        .env_remove("HERDR_SOCKET_PATH")
        .env("HERDR_ENV", "1")
        .status()
        .unwrap();
    // Clean up even when an assertion fails in the fixture.
    let _ = Command::new(&binary)
        .args(["session", "stop", "work", "--json"])
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("HOME", &dir)
        .env_remove("HERDR_CONFIG_PATH")
        .env_remove("HERDR_SESSION")
        .env_remove("HERDR_SOCKET_PATH")
        .output();
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .output();
    let _ = std::fs::remove_dir_all(dir);
    assert!(result.success(), "isolated herdr fixture failed");
}

fn wait_output(rx: &mut tokio::sync::mpsc::UnboundedReceiver<PtyEvent>, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut bytes = Vec::new();
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(PtyEvent::Output { bytes: chunk, .. }) => {
                bytes.extend(chunk);
                if String::from_utf8_lossy(&bytes).contains(needle) {
                    return;
                }
            }
            Ok(PtyEvent::Exit { .. }) => panic!("herdr client exited before {needle}"),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    panic!(
        "no {needle} in herdr output: {}",
        String::from_utf8_lossy(&bytes)
    );
}

#[test]
#[ignore = "launched only by real_herdr_lifecycle with isolated environment"]
fn isolated_herdr_fixture() {
    let dir = std::env::var("ANILA_HERDR_FIXTURE").expect("fixture must run in isolation");
    let binary = std::env::var("ANILA_HERDR_BINARY").unwrap();
    let config_dir = Path::new(&dir).join("config/herdr");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "onboarding = false\n[terminal]\ndefault_shell = \"/bin/sh\"\nshell_mode = \"non_login\"\n",
    )
    .unwrap();
    let config = Config::parse(&format!("host='test'\nworker='https://example.com'\nherdr_binary={binary:?}\n[[preset]]\nid='herdr'\nlabel='Herdr'\nbackend='herdr'\ndir={dir:?}\n")).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let socket = std::env::var("ANILA_HERDR_TMUX_SOCKET").unwrap();
    let mut spawner = SessionSpawner::new(Tmux::new(Some(socket.clone())), config.presets, tx)
        .with_herdr(Some(binary.clone()));
    let session = SessionName::new("herdr:work").unwrap();
    assert!(spawner.list().unwrap().is_empty());
    assert!(
        spawner
            .create(&SessionName::new("work").unwrap(), "herdr")
            .is_err()
    );
    assert!(!spawner.session_exists(&session));
    spawner.create(&session, "herdr").unwrap();
    assert!(matches!(
        spawner.create(&session, "herdr"),
        Err(CreateError::Exists)
    ));
    assert!(spawner.session_exists(&session));
    assert_eq!(spawner.list().unwrap()[0].name, "herdr:work");
    let tmux = Tmux::new(Some(socket));
    tmux.new_session(
        &SessionName::new("work").unwrap(),
        Path::new(&dir),
        Some("sleep 60"),
    )
    .unwrap();
    let listed = spawner.list().unwrap();
    assert!(listed.iter().any(|s| s.name == "work"));
    assert!(listed.iter().any(|s| s.name == "herdr:work"));
    let mut client = spawner.spawn(1, 1, 1, &session, 100, 30).unwrap();
    // Wait for the actual shell prompt before injecting input.
    wait_output(&mut rx, "ANILA-PROMPT>");
    client.resize(120, 40).unwrap();
    client
        .write(b"ANILA_CONTINUITY=still-here; pwd > \"$HOME/observed-cwd\"; printf 'anila-%s\\n' herdr-ready\r")
        .unwrap();
    wait_output(&mut rx, "anila-herdr-ready");
    assert_eq!(
        std::fs::read_to_string(Path::new(&dir).join("observed-cwd"))
            .unwrap()
            .trim(),
        dir,
        "herdr shell must start in preset directory"
    );
    client.kill();
    drop(client);
    // Wait for the killed client's final event before reusing the receiver.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if matches!(rx.try_recv(), Ok(PtyEvent::Exit { .. })) {
            break;
        }
        assert!(Instant::now() < deadline, "client did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        spawner.session_exists(&session),
        "detaching must preserve the server"
    );
    let mut reattached = spawner.spawn(1, 2, 2, &session, 80, 24).unwrap();
    wait_output(&mut rx, "anila-herdr-ready");
    reattached
        .write(b"printf 'continuity=%s\\n' \"$ANILA_CONTINUITY\"\r")
        .unwrap();
    wait_output(&mut rx, "continuity=still-here");
    assert!(
        Herdr::new(binary)
            .new_session(&session, Path::new(&dir))
            .is_err()
    );
}
