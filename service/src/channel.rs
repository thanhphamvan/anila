//! The channel manager: one state machine per `(tab, ch)`, with no I/O.
//!
//! The link layer feeds it browser frames, PTY output and timer ticks, and
//! sends whatever `Outbound` frames it returns. PTYs are created through
//! the `Spawner` trait so tests can use fakes.
//!
//! States (see the contracts page, "Channel lifecycle"):
//! - Live: attached to a browser tab, streaming OUT within the flow window.
//! - Orphaned: the tab is gone; the PTY stays attached and output is
//!   buffered until a `resume`, or until the grace period ends.
//! - Gone: the PTY is killed and the channel removed.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anila_proto::{
    Ch, Codec, Epoch, ErrorCode, InputId, Off, Preset, Req, SessionInfo, SessionName, Tab, ToAgent,
    ToBrowser, codec::Encoder, limits,
};

/// A running PTY attached to a tmux session.
pub trait Pty {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()>;
    /// Ends the tmux client. Must not block. The tmux session keeps running.
    fn kill(&mut self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    Exists,
    UnknownPreset,
    Failed(String),
}

/// Everything the manager needs from tmux and the OS.
pub trait Spawner {
    /// Attaches to an existing session in a new PTY. Output and exit of this
    /// PTY are reported back through `Manager::on_pty_output` and
    /// `Manager::on_pty_exit` with the same `(tab, ch, epoch)`.
    fn spawn(
        &mut self,
        tab: Tab,
        ch: Ch,
        epoch: Epoch,
        session: &SessionName,
        cols: u16,
        rows: u16,
    ) -> Result<Box<dyn Pty>, String>;
    fn session_exists(&mut self, session: &SessionName) -> bool;
    /// Creates a detached session from a preset id in the service config.
    fn create(&mut self, session: &SessionName, preset: &str) -> Result<(), CreateError>;
    fn list(&mut self) -> Result<Vec<SessionInfo>, String>;
}

/// A frame for the browser side of `tab`.
#[derive(Debug, Clone, PartialEq)]
pub enum Outbound {
    Text(ToBrowser),
    /// Encoded by the link layer as `anila_proto::Data::Out`.
    Out {
        tab: Tab,
        ch: Ch,
        epoch: Epoch,
        off: Off,
        bytes: Vec<u8>,
        compressed: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Limits {
    /// How long an orphaned channel keeps its PTY.
    pub grace: Duration,
    /// Replay buffer per channel.
    pub buffer_bytes: usize,
    /// Most output bytes sent but not yet acked, per channel.
    pub window_bytes: usize,
    /// Largest OUT payload.
    pub max_out_payload: usize,
    pub max_channels_per_tab: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(limits::GRACE_SECS),
            buffer_bytes: limits::REPLAY_BUFFER_BYTES,
            window_bytes: limits::FLOW_WINDOW_BYTES,
            max_out_payload: limits::MAX_OUT_PAYLOAD,
            max_channels_per_tab: limits::MAX_CHANNELS_PER_TAB,
        }
    }
}

struct Channel {
    session: SessionName,
    epoch: Epoch,
    pty: Box<dyn Pty>,
    buffer: crate::buffer::ReplayBuffer,
    sent: Off,
    acked: Off,
    last_in: InputId,
    last_in_acked: InputId,
    size: (u16, u16),
    state: State,
    encoder: Encoder,
}

#[derive(Clone, Copy)]
enum State {
    Live,
    Orphaned { since: Instant },
}

/// Owns channel lifetimes, replay and flow control across link reconnects.
/// Bytes kept free below `max_out_payload` for deflate's worst-case expansion.
const COMPRESSED_HEADROOM: usize = 256;

pub struct Manager {
    spawner: Box<dyn Spawner>,
    presets: Vec<Preset>,
    limits: Limits,
    epochs: Box<dyn FnMut() -> Epoch>,
    channels: BTreeMap<(Tab, Ch), Channel>,
}

impl Manager {
    /// `epochs` yields epoch candidates. The manager never uses 0 and never
    /// reuses the channel's current epoch; it draws again in both cases.
    pub fn new(
        spawner: Box<dyn Spawner>,
        presets: Vec<Preset>,
        limits: Limits,
        epochs: Box<dyn FnMut() -> Epoch>,
    ) -> Self {
        Self {
            spawner,
            presets,
            limits,
            epochs,
            channels: BTreeMap::new(),
        }
    }

    /// A routed text frame from the browser.
    pub fn handle(&mut self, msg: ToAgent, now: Instant) -> Vec<Outbound> {
        let _ = now;
        match msg {
            ToAgent::List { tab, req } => match self.spawner.list() {
                Ok(items) => vec![Outbound::Text(ToBrowser::Sessions {
                    tab,
                    req,
                    items,
                    presets: self.presets.clone(),
                })],
                Err(msg) => Self::error(tab, None, Some(req), ErrorCode::Internal, msg),
            },
            ToAgent::Open {
                tab,
                ch,
                req,
                session,
                create,
                cols,
                rows,
                codec,
            } => {
                let previous = self.remove((tab, ch)).map_or(0, |channel| channel.epoch);
                let error = |code, msg| Self::error(tab, Some(ch), Some(req), code, msg);
                if self.channels.keys().filter(|(t, _)| *t == tab).count()
                    >= self.limits.max_channels_per_tab
                {
                    return error(ErrorCode::Limit, "channel limit reached".into());
                }
                if let Some(create) = create {
                    if let Err(err) = self.spawner.create(&session, &create.preset) {
                        let (code, msg) = match err {
                            CreateError::Exists => (ErrorCode::Exists, "session exists".into()),
                            CreateError::UnknownPreset => {
                                (ErrorCode::NotFound, "unknown preset".into())
                            }
                            CreateError::Failed(msg) => (ErrorCode::Internal, msg),
                        };
                        return error(code, msg);
                    }
                } else if !self.spawner.session_exists(&session) {
                    return error(ErrorCode::NotFound, "session not found".into());
                }
                match self.attach((tab, ch), session, (cols, rows), previous, (0, 0), codec) {
                    Ok(epoch) => vec![Outbound::Text(ToBrowser::Opened {
                        tab,
                        ch,
                        req,
                        epoch,
                    })],
                    Err(msg) => error(ErrorCode::Internal, msg),
                }
            }
            ToAgent::Resume {
                tab,
                ch,
                session,
                epoch,
                from,
                cols,
                rows,
                codec,
            } => {
                if let Some(channel) = self.channels.get_mut(&(tab, ch)) {
                    if channel.session == session
                        && channel.epoch == epoch
                        && channel.buffer.start() <= from
                        && from <= channel.buffer.end()
                    {
                        channel.state = State::Live;
                        channel.encoder = Encoder::new(codec);
                        // A resize makes tmux redraw the whole screen, so
                        // skip it when the size is unchanged.
                        if channel.size != (cols, rows) {
                            channel.size = (cols, rows);
                            let _ = channel.pty.resize(cols, rows);
                        }
                        channel.acked = from;
                        channel.sent = from;
                        channel.last_in_acked = channel.last_in;
                        let mut out = vec![Outbound::Text(ToBrowser::Resumed {
                            tab,
                            ch,
                            epoch,
                            from,
                            in_acked: channel.last_in,
                        })];
                        out.extend(self.flush((tab, ch)));
                        return out;
                    }
                } else if !self.spawner.session_exists(&session) {
                    return vec![Outbound::Text(ToBrowser::Exited {
                        tab,
                        ch,
                        reason: "session not found".into(),
                    })];
                }
                self.reset((tab, ch), session, (cols, rows), epoch, codec)
            }
            ToAgent::Ack {
                tab,
                ch,
                epoch,
                off,
            } => {
                if let Some(channel) = self.channels.get_mut(&(tab, ch))
                    && channel.epoch == epoch
                    && channel.acked <= off
                    && off <= channel.sent
                {
                    channel.acked = off;
                    return self.flush((tab, ch));
                }
                Vec::new()
            }
            ToAgent::Resize {
                tab,
                ch,
                cols,
                rows,
            } => {
                if let Some(channel) = self.channels.get_mut(&(tab, ch)) {
                    channel.size = (cols, rows);
                    let _ = channel.pty.resize(cols, rows);
                }
                Vec::new()
            }
            ToAgent::Close { tab, ch } => {
                self.remove((tab, ch));
                Vec::new()
            }
        }
    }

    /// A binary IN frame from the browser.
    pub fn input(
        &mut self,
        tab: Tab,
        ch: Ch,
        id: InputId,
        bytes: &[u8],
        now: Instant,
    ) -> Vec<Outbound> {
        let _ = now;
        let Some(channel) = self.channels.get_mut(&(tab, ch)) else {
            return Self::error(
                tab,
                Some(ch),
                None,
                ErrorCode::NotFound,
                "channel not found".into(),
            );
        };
        if id > channel.last_in && channel.pty.write(bytes).is_ok() {
            channel.last_in = id;
        }
        Vec::new()
    }

    /// Buffers output from the current PTY and streams it when live.
    pub fn on_pty_output(&mut self, tab: Tab, ch: Ch, epoch: Epoch, bytes: &[u8]) -> Vec<Outbound> {
        if let Some(channel) = self.channels.get_mut(&(tab, ch))
            && channel.epoch == epoch
        {
            channel.buffer.push(bytes);
            return self.flush((tab, ch));
        }
        Vec::new()
    }

    /// Reattaches a live channel after its PTY exits if the session survives.
    pub fn on_pty_exit(&mut self, tab: Tab, ch: Ch, epoch: Epoch, now: Instant) -> Vec<Outbound> {
        let _ = now;
        let Some(channel) = self.channels.get(&(tab, ch)) else {
            return Vec::new();
        };
        if channel.epoch != epoch {
            return Vec::new();
        }
        if matches!(channel.state, State::Orphaned { .. }) {
            self.channels.remove(&(tab, ch));
            return Vec::new();
        }
        let session = channel.session.clone();
        let size = channel.size;
        let codec = channel.encoder.codec();
        if self.spawner.session_exists(&session) {
            self.reset((tab, ch), session, size, epoch, codec)
        } else {
            self.channels.remove(&(tab, ch));
            vec![Outbound::Text(ToBrowser::Exited {
                tab,
                ch,
                reason: "session ended".into(),
            })]
        }
    }

    /// The room says this tab's socket closed.
    pub fn tab_gone(&mut self, tab: Tab, now: Instant) {
        for ((t, _), channel) in &mut self.channels {
            if *t == tab {
                channel.state = State::Orphaned { since: now };
            }
        }
    }

    /// The service lost its link to the room: every channel is orphaned.
    pub fn link_lost(&mut self, now: Instant) {
        for channel in self.channels.values_mut() {
            channel.state = State::Orphaned { since: now };
        }
    }

    /// Called about every 100 ms: batched `in_ack`s, then grace expiry.
    pub fn tick(&mut self, now: Instant) -> Vec<Outbound> {
        let mut out = Vec::new();
        for (&(tab, ch), channel) in &mut self.channels {
            if matches!(channel.state, State::Live) && channel.last_in > channel.last_in_acked {
                out.push(Outbound::Text(ToBrowser::InAck {
                    tab,
                    ch,
                    id: channel.last_in,
                }));
                channel.last_in_acked = channel.last_in;
            }
        }
        self.channels.retain(|_, channel| {
            if let State::Orphaned { since } = channel.state
                && now
                    .checked_duration_since(since)
                    .is_some_and(|elapsed| elapsed >= self.limits.grace)
            {
                channel.pty.kill();
                return false;
            }
            true
        });
        out
    }

    /// Number of live and orphaned channels.
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    fn error(
        tab: Tab,
        ch: Option<Ch>,
        req: Option<Req>,
        code: ErrorCode,
        msg: String,
    ) -> Vec<Outbound> {
        vec![Outbound::Text(ToBrowser::Error {
            tab,
            ch,
            req,
            code,
            msg,
        })]
    }

    fn remove(&mut self, key: (Tab, Ch)) -> Option<Channel> {
        self.channels.remove(&key).map(|mut channel| {
            channel.pty.kill();
            channel
        })
    }

    fn attach(
        &mut self,
        key: (Tab, Ch),
        session: SessionName,
        size: (u16, u16),
        previous: Epoch,
        input: (InputId, InputId),
        codec: Codec,
    ) -> Result<Epoch, String> {
        let epoch = loop {
            let candidate = (self.epochs)();
            if candidate != 0 && candidate != previous {
                break candidate;
            }
        };
        let pty = self
            .spawner
            .spawn(key.0, key.1, epoch, &session, size.0, size.1)?;
        self.channels.insert(
            key,
            Channel {
                session,
                epoch,
                pty,
                size,
                buffer: crate::buffer::ReplayBuffer::new(self.limits.buffer_bytes),
                sent: 0,
                acked: 0,
                last_in: input.0,
                last_in_acked: input.1,
                state: State::Live,
                encoder: Encoder::new(codec),
            },
        );
        Ok(epoch)
    }

    fn reset(
        &mut self,
        key: (Tab, Ch),
        session: SessionName,
        size: (u16, u16),
        previous: Epoch,
        codec: Codec,
    ) -> Vec<Outbound> {
        let (previous, input) = self.remove(key).map_or((previous, (0, 0)), |channel| {
            (channel.epoch, (channel.last_in, channel.last_in_acked))
        });
        let (tab, ch) = key;
        vec![Outbound::Text(
            match self.attach(key, session, size, previous, input, codec) {
                Ok(epoch) => ToBrowser::Reset { tab, ch, epoch },
                Err(reason) => ToBrowser::Exited { tab, ch, reason },
            },
        )]
    }

    fn flush(&mut self, key: (Tab, Ch)) -> Vec<Outbound> {
        let Some(channel) = self.channels.get_mut(&key) else {
            return Vec::new();
        };
        if !matches!(channel.state, State::Live) {
            return Vec::new();
        }
        if channel.sent < channel.buffer.start() {
            let session = channel.session.clone();
            let size = channel.size;
            let epoch = channel.epoch;
            let codec = channel.encoder.codec();
            return self.reset(key, session, size, epoch, codec);
        }
        let mut out = Vec::new();
        while channel.sent < channel.buffer.end()
            && channel.sent - channel.acked < self.limits.window_bytes as Off
        {
            let available = self.limits.window_bytes as Off - (channel.sent - channel.acked);
            // Deflate expands incompressible input by a few bytes per block
            // plus the sync-flush marker, so this headroom always fits the
            // payload limit without trial encoding.
            let room = if channel.encoder.codec() == Codec::None {
                self.limits.max_out_payload
            } else {
                self.limits
                    .max_out_payload
                    .saturating_sub(COMPRESSED_HEADROOM)
                    .max(1)
            };
            let len = available
                .min(room as Off)
                .min(channel.buffer.end() - channel.sent) as usize;
            if len == 0 {
                break;
            }
            let Ok(raw) = channel.buffer.read_from(channel.sent, len) else {
                break;
            };
            let bytes = channel.encoder.encode(&raw);
            debug_assert!(bytes.len() <= self.limits.max_out_payload);
            out.push(Outbound::Out {
                tab: key.0,
                ch: key.1,
                epoch: channel.epoch,
                off: channel.sent,
                compressed: channel.encoder.codec() != Codec::None,
                bytes,
            });
            channel.sent += len as Off;
        }
        out
    }
}
