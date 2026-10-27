//! Contract C5: how the service drives tmux. Always an argument list,
//! never a shell, and session names are validated before they get here.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use anila_proto::{SessionInfo, SessionName};

#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("session already exists")]
    Exists,
    #[error("tmux failed: {0}")]
    Failed(String),
    #[error("could not run tmux: {0}")]
    Io(#[from] std::io::Error),
}

const SESSIONS_FORMAT: &str =
    "#{session_name}\t#{session_windows}\t#{session_attached}\t#{session_activity}";
const PANES_FORMAT: &str =
    "#{session_name}\t#{window_active}\t#{pane_active}\t#{pane_current_command}";

pub struct Tmux {
    socket: Option<String>,
}

impl Tmux {
    /// `socket` is a `tmux -L` name; `None` means the default server.
    pub fn new(socket: Option<String>) -> Self {
        Self { socket }
    }

    fn base_args(&self) -> Vec<String> {
        match &self.socket {
            Some(s) => vec!["-L".into(), s.clone()],
            None => vec![],
        }
    }

    fn run(&self, args: &[&str]) -> Result<std::process::Output, TmuxError> {
        Ok(Command::new("tmux")
            .args(self.base_args())
            .args(args)
            .output()?)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, TmuxError> {
        let out = self.run(&["list-sessions", "-F", SESSIONS_FORMAT])?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if no_server(&err) {
                return Ok(Vec::new());
            }
            return Err(TmuxError::Failed(err.trim().into()));
        }
        let panes = self.run(&["list-panes", "-a", "-F", PANES_FORMAT])?;
        let cmds = parse_active_commands(&String::from_utf8_lossy(&panes.stdout));
        let mut sessions = parse_sessions(&String::from_utf8_lossy(&out.stdout));
        for s in &mut sessions {
            s.cmd = cmds.get(&s.name).cloned().unwrap_or_default();
        }
        Ok(sessions)
    }

    pub fn has_session(&self, name: &SessionName) -> Result<bool, TmuxError> {
        let target = exact(name);
        Ok(self.run(&["has-session", "-t", &target])?.status.success())
    }

    pub fn new_session(
        &self,
        name: &SessionName,
        dir: &Path,
        cmd: Option<&str>,
    ) -> Result<(), TmuxError> {
        let dir = dir.to_string_lossy();
        let mut args = vec!["new-session", "-d", "-s", name.as_str(), "-c", &dir];
        if let Some(cmd) = cmd {
            args.push(cmd);
        }
        let out = self.run(&args)?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("duplicate session") {
            Err(TmuxError::Exists)
        } else {
            Err(TmuxError::Failed(err.trim().into()))
        }
    }

    /// The program and arguments to run inside a PTY to attach. The `=`
    /// prefix makes tmux match the session name exactly.
    pub fn attach_command(&self, name: &SessionName) -> (String, Vec<String>) {
        let mut args = self.base_args();
        args.extend(["attach-session".into(), "-t".into(), exact(name)]);
        ("tmux".into(), args)
    }
}

fn exact(name: &SessionName) -> String {
    format!("={name}")
}

fn no_server(stderr: &str) -> bool {
    stderr.contains("no server running") || stderr.contains("error connecting to")
}

fn parse_sessions(stdout: &str) -> Vec<SessionInfo> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            Some(SessionInfo {
                name: f.next()?.into(),
                windows: f.next()?.parse().ok()?,
                attached: f.next()?.parse().ok()?,
                activity: f.next()?.parse().ok()?,
                cmd: String::new(),
            })
        })
        .collect()
}

/// Session name → command in the active pane of its active window.
fn parse_active_commands(stdout: &str) -> HashMap<String, String> {
    stdout
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                [session, "1", "1", cmd] => Some((session.to_string(), cmd.to_string())),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sessions() {
        let s = parse_sessions("claude\t3\t1\t1792585500\nnotes\t1\t0\t1792581900\nbroken line\n");
        assert_eq!(s.len(), 2);
        assert_eq!(
            (s[0].name.as_str(), s[0].windows, s[0].attached),
            ("claude", 3, 1)
        );
        assert_eq!(s[1].activity, 1792581900);
    }

    #[test]
    fn picks_active_pane_of_active_window() {
        let c = parse_active_commands(
            "claude\t0\t1\tzsh\nclaude\t1\t0\tzsh\nclaude\t1\t1\tclaude\nnotes\t1\t1\tvim\n",
        );
        assert_eq!(c["claude"], "claude");
        assert_eq!(c["notes"], "vim");
    }

    #[test]
    fn attach_uses_exact_match_and_socket() {
        let t = Tmux::new(Some("anila-test".into()));
        let (prog, args) = t.attach_command(&SessionName::new("claude").unwrap());
        assert_eq!(prog, "tmux");
        assert_eq!(
            args,
            ["-L", "anila-test", "attach-session", "-t", "=claude"]
        );
    }
}
