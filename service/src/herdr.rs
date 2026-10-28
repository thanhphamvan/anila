//! Native herdr sessions. Use the CLI's JSON output, never a shell or tmux wrapper.
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anila_proto::{SessionBackend, SessionInfo, SessionName};
use serde::Deserialize;

use crate::channel::CreateError;

pub struct Herdr {
    binary: String,
}

#[derive(Deserialize)]
struct Listing {
    sessions: Vec<NativeSession>,
}
#[derive(Deserialize)]
struct NativeSession {
    name: String,
    running: bool,
}

impl Herdr {
    pub fn new(binary: String) -> Self {
        Self { binary }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        // Always target the explicitly named local session, even if the service
        // was started from inside herdr or a pane with an API socket override.
        command
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_SOCKET_PATH");
        command
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, String> {
        let output = self
            .command()
            .args(["session", "list", "--json"])
            .output()
            .map_err(|e| format!("could not run herdr: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "herdr session list failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        parse_sessions(&output.stdout)
    }

    pub fn has_session(&self, name: &SessionName) -> Result<bool, String> {
        Ok(self
            .list_sessions()?
            .iter()
            .any(|s| s.name == name.as_str()))
    }

    pub fn attach_command(&self, name: &SessionName) -> (String, Vec<String>) {
        (
            self.binary.clone(),
            vec!["--session".into(), name.native_name().into()],
        )
    }

    pub fn new_session(&self, name: &SessionName, dir: &Path) -> Result<(), CreateError> {
        // Saved sessions count as existing, including stopped sessions. Never
        // overwrite an existing herdr layout through Anila's creation form.
        if self.has_session(name).map_err(CreateError::Failed)? {
            return Err(CreateError::Exists);
        }
        let dir = dir
            .canonicalize()
            .map_err(|e| CreateError::Failed(format!("invalid herdr working directory: {e}")))?;
        let mut child = self
            .command()
            .args(["server", "--session", name.native_name()])
            .current_dir(&dir)
            .env("HERDR_STARTUP_CWD", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| CreateError::Failed(format!("could not start herdr: {e}")))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let result = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    break Err(CreateError::Failed(format!(
                        "herdr server exited: {status}"
                    )));
                }
                Err(e) => break Err(CreateError::Failed(e.to_string())),
                Ok(None) => {}
            }
            match self.running(name) {
                Ok(true) => break Ok(()),
                Err(error) => break Err(CreateError::Failed(error)),
                Ok(false) => {}
            }
            if Instant::now() >= deadline {
                break Err(CreateError::Failed(
                    "timed out waiting for herdr server".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        if result.is_err() {
            let _ = child.kill();
        }
        // Reap the server without tying its lifetime to any browser attachment.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        result
    }

    fn running(&self, name: &SessionName) -> Result<bool, String> {
        // A socket existing is weaker than readiness: wait until the server
        // event loop answers its status API before the browser can attach.
        let output = self
            .command()
            .args([
                "--session",
                name.native_name(),
                "status",
                "server",
                "--json",
            ])
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Ok(false);
        }
        #[derive(Deserialize)]
        struct Status {
            running: bool,
        }
        let status: Status = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("invalid herdr status JSON: {e}"))?;
        Ok(status.running)
    }
}

fn parse_sessions(bytes: &[u8]) -> Result<Vec<SessionInfo>, String> {
    let listing: Listing =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid herdr session JSON: {e}"))?;
    Ok(listing
        .sessions
        .into_iter()
        .filter_map(|s| {
            // The default session is always listed even before it exists.
            if s.name == "default" && !s.running {
                return None;
            }
            let name = SessionName::for_backend(s.name, SessionBackend::Herdr).ok()?;
            Some(SessionInfo {
                name: name.to_string(),
                windows: 0,
                attached: 0,
                activity: 0,
                cmd: "herdr".into(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn listing_preserves_stopped_sessions_and_ignores_unrepresentable_names() {
        let sessions = parse_sessions(br#"{"sessions":[{"name":"default","running":false},{"name":"work","running":true},{"name":"saved","running":false},{"name":"bad:name","running":true}]}"#).unwrap();
        assert_eq!(
            sessions.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["herdr:work", "herdr:saved"]
        );
        assert!(parse_sessions(b"not json").is_err());
    }
    #[test]
    fn attach_targets_exact_native_session() {
        let name = SessionName::new("herdr:work").unwrap();
        assert_eq!(
            Herdr::new("/opt/bin/herdr".into()).attach_command(&name),
            (
                "/opt/bin/herdr".into(),
                vec!["--session".into(), "work".into()]
            )
        );
    }
}
