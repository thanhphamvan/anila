//! The real `Spawner`: tmux sessions attached through PTYs.

use anila_proto::{Ch, Epoch, SessionInfo, SessionName, Tab};
use tokio::sync::mpsc::UnboundedSender;

use crate::channel::{CreateError, Pty, Spawner};
use crate::config::{PresetConfig, expand_home};
use crate::pty::{self, PtyEvent};
use crate::tmux::{Tmux, TmuxError};

/// Attaches configured tmux sessions to PTYs owned by the channel manager.
pub struct TmuxSpawner {
    tmux: Tmux,
    presets: Vec<PresetConfig>,
    events: UnboundedSender<PtyEvent>,
}

impl TmuxSpawner {
    /// `events` receives output and exit events from every PTY it spawns.
    pub fn new(tmux: Tmux, presets: Vec<PresetConfig>, events: UnboundedSender<PtyEvent>) -> Self {
        Self {
            tmux,
            presets,
            events,
        }
    }
}

impl Spawner for TmuxSpawner {
    fn spawn(
        &mut self,
        tab: Tab,
        ch: Ch,
        epoch: Epoch,
        session: &SessionName,
        cols: u16,
        rows: u16,
    ) -> Result<Box<dyn Pty>, String> {
        let (program, args) = self.tmux.attach_command(session);
        pty::spawn(
            &program,
            &args,
            cols,
            rows,
            (tab, ch, epoch),
            self.events.clone(),
        )
        .map(|pty| Box::new(pty) as Box<dyn Pty>)
        .map_err(|err| err.to_string())
    }

    fn session_exists(&mut self, session: &SessionName) -> bool {
        self.tmux.has_session(session).unwrap_or(false)
    }

    fn create(&mut self, session: &SessionName, preset: &str) -> Result<(), CreateError> {
        let preset = self
            .presets
            .iter()
            .find(|p| p.id == preset)
            .ok_or(CreateError::UnknownPreset)?;
        self.tmux
            .new_session(session, &expand_home(&preset.dir), preset.cmd.as_deref())
            .map_err(|err| match err {
                TmuxError::Exists => CreateError::Exists,
                other => CreateError::Failed(other.to_string()),
            })
    }

    fn list(&mut self) -> Result<Vec<SessionInfo>, String> {
        self.tmux.list_sessions().map_err(|err| err.to_string())
    }
}
