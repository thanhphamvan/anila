//! Native tmux and herdr sessions attached through PTYs.

use anila_proto::{Ch, Epoch, SessionBackend, SessionInfo, SessionName, Tab};
use tokio::sync::mpsc::UnboundedSender;

use crate::channel::{CreateError, Pty, Spawner};
use crate::config::{PresetConfig, expand_home};
use crate::pty::{self, PtyEvent};
use crate::tmux::{Tmux, TmuxError};

/// Attaches configured backend sessions to PTYs owned by the channel manager.
pub struct SessionSpawner {
    tmux: Tmux,
    herdr: Option<crate::herdr::Herdr>,
    presets: Vec<PresetConfig>,
    events: UnboundedSender<PtyEvent>,
}

impl SessionSpawner {
    pub fn with_herdr(mut self, binary: Option<String>) -> Self {
        self.herdr = binary.map(crate::herdr::Herdr::new);
        self
    }
    fn herdr(&self) -> Result<&crate::herdr::Herdr, String> {
        self.herdr
            .as_ref()
            .ok_or_else(|| "herdr is not enabled on this machine".into())
    }
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, String> {
        // Herdr-only hosts do not need tmux installed. When tmux presets are
        // configured, retain tmux failures instead of hiding a broken backend.
        let mut sessions = match self.tmux.list_sessions() {
            Ok(sessions) => sessions,
            Err(_)
                if !self
                    .presets
                    .iter()
                    .any(|p| p.backend == SessionBackend::Tmux)
                    && self.herdr.is_some() =>
            {
                Vec::new()
            }
            Err(error) => return Err(error.to_string()),
        };
        if let Some(herdr) = &self.herdr {
            sessions.extend(herdr.list_sessions()?);
        }
        Ok(sessions)
    }
    /// `events` receives output and exit events from every PTY it spawns.
    pub fn new(tmux: Tmux, presets: Vec<PresetConfig>, events: UnboundedSender<PtyEvent>) -> Self {
        Self {
            tmux,
            herdr: None,
            presets,
            events,
        }
    }
}

impl Spawner for SessionSpawner {
    fn spawn(
        &mut self,
        tab: Tab,
        ch: Ch,
        epoch: Epoch,
        session: &SessionName,
        cols: u16,
        rows: u16,
    ) -> Result<Box<dyn Pty>, String> {
        let (program, args) = match session.backend() {
            SessionBackend::Tmux => self.tmux.attach_command(session),
            SessionBackend::Herdr => self.herdr()?.attach_command(session),
        };
        let process = if session.backend() == SessionBackend::Herdr {
            let mut command = portable_pty::CommandBuilder::new(&program);
            command.args(&args);
            for key in [
                "HERDR_ENV",
                "HERDR_SESSION",
                "HERDR_SOCKET_PATH",
                "HERDR_PANE_ID",
            ] {
                command.env_remove(key);
            }
            pty::spawn_command(command, cols, rows, (tab, ch, epoch), self.events.clone())
        } else {
            pty::spawn(
                &program,
                &args,
                cols,
                rows,
                (tab, ch, epoch),
                self.events.clone(),
            )
        };
        process
            .map(|pty| Box::new(pty) as Box<dyn Pty>)
            .map_err(|err| err.to_string())
    }

    fn session_exists(&mut self, session: &SessionName) -> bool {
        match session.backend() {
            SessionBackend::Tmux => self.tmux.has_session(session).unwrap_or(false),
            SessionBackend::Herdr => self
                .herdr()
                .and_then(|h| h.has_session(session))
                .unwrap_or(false),
        }
    }

    fn create(&mut self, session: &SessionName, preset: &str) -> Result<(), CreateError> {
        let preset = self
            .presets
            .iter()
            .find(|p| p.id == preset)
            .ok_or(CreateError::UnknownPreset)?;
        if session.backend() != preset.backend {
            return Err(CreateError::Failed(
                "session backend does not match preset".into(),
            ));
        }
        if preset.backend == SessionBackend::Herdr {
            return self
                .herdr()
                .map_err(CreateError::Failed)?
                .new_session(session, &expand_home(&preset.dir));
        }
        self.tmux
            .new_session(session, &expand_home(&preset.dir), preset.cmd.as_deref())
            .map_err(|err| match err {
                TmuxError::Exists => CreateError::Exists,
                other => CreateError::Failed(other.to_string()),
            })
    }

    fn list(&mut self) -> Result<Vec<SessionInfo>, String> {
        self.list_sessions()
    }
}
