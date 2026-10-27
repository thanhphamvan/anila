//! Isolated tmux servers for process-level tests.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

/// Owns a private tmux server, terminated when this value is dropped.
pub struct TmuxServer {
    socket: String,
}

impl TmuxServer {
    /// Allocates a unique socket name; the first session starts the server.
    pub fn new(prefix: &str) -> Self {
        Self {
            socket: format!(
                "{prefix}-{}-{}",
                std::process::id(),
                NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    /// Returns the name passed to tmux's `-L` option.
    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// Runs a tmux command, returning stdout (empty when the server is gone).
    pub fn cmd(&self, args: &[&str]) -> String {
        match Command::new("tmux")
            .args(["-L", self.socket()])
            .args(args)
            .output()
        {
            Ok(output) => {
                if !output.status.success() {
                    eprintln!("tmux: {}", String::from_utf8_lossy(&output.stderr));
                }
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
            Err(error) => {
                eprintln!("run tmux: {error}");
                String::new()
            }
        }
    }

    /// Creates a detached session with `/tmp` as its working directory.
    pub fn new_session(&self, name: &str, cmd: &str) {
        self.cmd(&["new-session", "-d", "-s", name, "-c", "/tmp", cmd]);
    }

    /// Returns the width and height of every attached client as `WxH`.
    pub fn clients(&self) -> Vec<String> {
        self.cmd(&["list-clients", "-F", "#{client_width}x#{client_height}"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Captures the visible pane of the exactly named session.
    /// `<session> <client pid>` for every attached client, sorted. A new
    /// attach shows up as a new pid.
    pub fn attachments(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .cmd(&["list-clients", "-F", "#{session_name} #{client_pid}"])
            .lines()
            .map(str::to_owned)
            .collect();
        out.sort();
        out
    }

    /// Types `keys` and Enter into `session`, as if from another terminal.
    pub fn type_line(&self, session: &str, keys: &str) {
        self.cmd(&["send-keys", "-t", &format!("={session}:"), "-l", keys]);
        self.cmd(&["send-keys", "-t", &format!("={session}:"), "Enter"]);
    }

    pub fn capture(&self, session: &str) -> String {
        self.cmd(&["capture-pane", "-p", "-t", &format!("={session}:")])
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        self.cmd(&["kill-server"]);
    }
}
