//! Contract C5 against a real tmux on a private socket, so these tests
//! never touch your own sessions.

use std::path::Path;
use std::process::Command;

use anila_proto::SessionName;
use anila_service::tmux::{Tmux, TmuxError};

struct Server(String);

impl Server {
    fn start() -> Option<Self> {
        let ok = Command::new("tmux")
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("tmux not installed, skipping");
            return None;
        }
        Some(Server(format!("anila-test-{}", std::process::id())))
    }

    fn tmux(&self) -> Tmux {
        Tmux::new(Some(self.0.clone()))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .output();
    }
}

fn name(s: &str) -> SessionName {
    SessionName::new(s).unwrap()
}

#[test]
fn create_list_and_detect_duplicates() {
    let Some(server) = Server::start() else {
        return;
    };
    let tmux = server.tmux();

    assert!(
        tmux.list_sessions().unwrap().is_empty(),
        "no server yet means an empty list"
    );

    tmux.new_session(&name("work"), Path::new("/tmp"), Some("sleep 60"))
        .unwrap();
    tmux.new_session(&name("notes"), Path::new("/tmp"), None)
        .unwrap();
    assert!(matches!(
        tmux.new_session(&name("work"), Path::new("/tmp"), None),
        Err(TmuxError::Exists)
    ));

    let sessions = tmux.list_sessions().unwrap();
    let work = sessions
        .iter()
        .find(|s| s.name == "work")
        .expect("work listed");
    assert_eq!(work.windows, 1);
    assert_eq!(work.attached, 0);
    assert_eq!(work.cmd, "sleep");
    assert!(sessions.iter().any(|s| s.name == "notes"));

    assert!(tmux.has_session(&name("work")).unwrap());
    assert!(
        !tmux.has_session(&name("wor")).unwrap(),
        "prefix must not match"
    );
}
