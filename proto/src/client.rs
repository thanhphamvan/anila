//! The browser side of the protocol as a pure state machine, with no I/O.
//!
//! The web app and the end-to-end probe both drive it: they feed it socket
//! events and the clock, send the frames it returns, and render the events
//! it emits. Keeping it here means reconnect and resume behave identically
//! in both, and are tested natively.

use std::collections::BTreeMap;

use crate::codec::{Decoder, SUPPORTED, choose};

use crate::{
    AgentStatus, Ch, Codec, Create, Data, Epoch, ErrorCode, Hop, InputId, PROTO, Preset, Req,
    SessionInfo, SessionName, Tab, ToAgent, ToBrowser, control::PING_TEXT, limits,
};

/// A frame to send on the websocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
}

/// Something for the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The room welcomed this tab.
    Connected {
        host: String,
        agent: AgentStatus,
    },
    /// The host machine went online or offline.
    Status(AgentStatus),
    Sessions {
        req: Req,
        items: Vec<SessionInfo>,
        presets: Vec<Preset>,
    },
    Opened {
        ch: Ch,
        req: Req,
    },
    /// Bytes to write to this channel's terminal, in order.
    Output {
        ch: Ch,
        bytes: Vec<u8>,
    },
    /// Clear this channel's terminal: a fresh attach starts from offset 0.
    Reset {
        ch: Ch,
    },
    /// The channel is gone; its tmux session ended or vanished.
    Exited {
        ch: Ch,
        reason: String,
    },
    Error {
        ch: Option<Ch>,
        req: Option<Req>,
        code: ErrorCode,
        msg: String,
    },
    /// Nothing heard from the room for `dead_ms`: close the socket and
    /// reconnect. Emitted once per connection.
    LinkDead,
}

/// What to do after the socket closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseAction {
    /// Reconnect after `backoff_ms(attempt, jitter)`.
    Reconnect,
    /// Do not reconnect; show `reason`. 4401 means the user must sign in.
    Stop { code: u16, reason: String },
}

/// Timing and output acknowledgement thresholds.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Browser codec preference order.
    pub codecs: Vec<Codec>,
    pub ping_ms: u64,
    pub dead_ms: u64,
    pub ack_ms: u64,
    pub ack_bytes: u64,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            codecs: SUPPORTED.to_vec(),
            ping_ms: crate::limits::PING_INTERVAL_SECS * 1000,
            dead_ms: crate::limits::DEAD_AFTER_SECS * 1000,
            ack_ms: crate::limits::ACK_INTERVAL_MS,
            ack_bytes: crate::limits::ACK_EVERY_BYTES as u64,
        }
    }
}

/// Browser protocol state, driven entirely by caller-supplied events and time.
pub struct Client {
    tab: Tab,
    config: ClientConfig,
    socket_open: bool,
    welcomed: bool,
    online: bool,
    advertised: Vec<Codec>,
    last_rx_ms: u64,
    last_ping_ms: u64,
    dead_reported: bool,
    next_req: Req,
    next_ch: Ch,
    channels: BTreeMap<Ch, Channel>,
}

#[derive(PartialEq, Eq)]
enum Phase {
    Opening,
    Live,
    Resuming,
    NeedsResume,
}

struct Channel {
    codec: Codec,
    decoder: Decoder,
    session: SessionName,
    cols: u16,
    rows: u16,
    create: Option<Create>,
    req: Req,
    epoch: Option<Epoch>,
    phase: Phase,
    rendered: u64,
    acked: u64,
    last_ack_ms: u64,
    next_in_id: InputId,
    pending: Vec<(InputId, Vec<u8>)>,
}

fn text_frame(message: &impl serde::Serialize) -> Frame {
    // Protocol messages contain only JSON-compatible fields.
    Frame::Text(serde_json::to_string(message).expect("protocol message serializes"))
}

fn input_frame(tab: Tab, ch: Ch, id: InputId, bytes: &[u8]) -> Frame {
    Frame::Binary(
        Data::In { tab, ch, id, bytes }
            .to_vec()
            .expect("input is chunked to the protocol limit"),
    )
}

impl Channel {
    fn open_frame(&self, tab: Tab, ch: Ch) -> Frame {
        text_frame(&ToAgent::Open {
            tab,
            ch,
            req: self.req,
            session: self.session.clone(),
            create: self.create.clone(),
            cols: self.cols,
            rows: self.rows,
            codec: self.codec,
        })
    }

    fn resume_frame(&mut self, tab: Tab, ch: Ch, codec: Codec) -> Option<Frame> {
        let epoch = self.epoch?;
        self.codec = codec;
        self.decoder = Decoder::new(codec);
        self.phase = Phase::Resuming;
        Some(text_frame(&ToAgent::Resume {
            tab,
            ch,
            session: self.session.clone(),
            epoch,
            from: self.rendered,
            cols: self.cols,
            rows: self.rows,
            codec,
        }))
    }

    fn ack(&mut self, tab: Tab, ch: Ch, now_ms: u64) -> Option<Frame> {
        let epoch = self.epoch?;
        self.acked = self.rendered;
        self.last_ack_ms = now_ms;
        Some(text_frame(&ToAgent::Ack {
            tab,
            ch,
            epoch,
            off: self.rendered,
        }))
    }
}

impl Client {
    /// Creates a disconnected client with fresh channel and request counters.
    pub fn new(tab: Tab, config: ClientConfig) -> Self {
        Self {
            tab,
            config,
            socket_open: false,
            welcomed: false,
            online: false,
            advertised: Vec::new(),
            last_rx_ms: 0,
            last_ping_ms: 0,
            dead_reported: false,
            next_req: 1,
            next_ch: 1,
            channels: BTreeMap::new(),
        }
    }

    /// The tab carried by this client's routed frames.
    pub fn tab(&self) -> Tab {
        self.tab
    }

    /// True between `welcome` and the socket closing.
    pub fn is_connected(&self) -> bool {
        self.welcomed
    }

    /// The socket opened. Returns the `hello` frame.
    pub fn on_open(&mut self, now_ms: u64) -> Vec<Frame> {
        self.socket_open = true;
        self.welcomed = false;
        self.last_rx_ms = now_ms;
        self.last_ping_ms = now_ms;
        self.dead_reported = false;
        vec![text_frame(&Hop::Hello {
            proto: PROTO,
            tab: self.tab,
        })]
    }

    /// The socket closed with `code`. Channels keep their state so they can
    /// resume after reconnecting.
    pub fn on_close(&mut self, code: u16) -> CloseAction {
        self.socket_open = false;
        self.welcomed = false;
        for channel in self.channels.values_mut() {
            if channel.phase == Phase::Live {
                channel.phase = Phase::Resuming;
            }
        }
        let reason = match code {
            4400 => "Invalid protocol frame",
            4401 => "Sign in required",
            4403 => "Access forbidden",
            4409 => "Connection replaced",
            _ => return CloseAction::Reconnect,
        };
        CloseAction::Stop {
            code,
            reason: reason.into(),
        }
    }

    fn catch_up(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        for (&ch, channel) in &mut self.channels {
            let codec = choose(&self.advertised, &self.config.codecs);
            if channel.epoch.is_some() {
                frames.extend(channel.resume_frame(self.tab, ch, codec));
            } else if channel.phase == Phase::Opening {
                channel.codec = codec;
                frames.push(channel.open_frame(self.tab, ch));
            }
        }
        frames
    }

    /// Consumes a text frame, ignoring malformed or unrelated messages.
    pub fn on_text(&mut self, text: &str, now_ms: u64) -> (Vec<Frame>, Vec<Event>) {
        self.last_rx_ms = now_ms;
        let mut frames = Vec::new();
        let mut events = Vec::new();
        match crate::classify(text) {
            Some(crate::Route::Hop) => match serde_json::from_str::<Hop>(text) {
                Ok(Hop::Welcome { host, agent, .. }) => {
                    let agent = agent.unwrap_or_default();
                    self.welcomed = true;
                    self.online = agent.online;
                    self.advertised = agent.codecs.clone();
                    events.push(Event::Connected {
                        host: host.unwrap_or_default(),
                        agent,
                    });
                    if self.online {
                        frames = self.catch_up();
                    }
                }
                Ok(Hop::Status(agent)) => {
                    let came_online = !self.online && agent.online;
                    self.online = agent.online;
                    self.advertised = agent.codecs.clone();
                    events.push(Event::Status(agent));
                    if came_online && self.welcomed {
                        frames = self.catch_up();
                    }
                }
                Ok(Hop::Error { code, msg }) => events.push(Event::Error {
                    ch: None,
                    req: None,
                    code,
                    msg,
                }),
                _ => {}
            },
            Some(crate::Route::Routed(tab)) if tab == self.tab => {
                if let Ok(message) = serde_json::from_str::<ToBrowser>(text) {
                    self.on_routed(message, now_ms, &mut frames, &mut events);
                }
            }
            _ => {}
        }
        (frames, events)
    }

    fn on_routed(
        &mut self,
        message: ToBrowser,
        now_ms: u64,
        frames: &mut Vec<Frame>,
        events: &mut Vec<Event>,
    ) {
        match message {
            ToBrowser::Sessions {
                req,
                items,
                presets,
                ..
            } => events.push(Event::Sessions {
                req,
                items,
                presets,
            }),
            ToBrowser::Opened { ch, req, epoch, .. } => {
                let Some(channel) = self.channels.get_mut(&ch) else {
                    return;
                };
                channel.epoch = Some(epoch);
                channel.phase = Phase::Live;
                channel.decoder = Decoder::new(channel.codec);
                channel.rendered = 0;
                channel.acked = 0;
                channel.last_ack_ms = now_ms;
                events.push(Event::Opened { ch, req });
            }
            ToBrowser::Resumed { ch, in_acked, .. } => {
                let Some(channel) = self.channels.get_mut(&ch) else {
                    return;
                };
                channel.phase = Phase::Live;
                channel.decoder = Decoder::new(channel.codec);
                channel.last_ack_ms = now_ms;
                channel.pending.retain(|(id, _)| *id > in_acked);
                frames.extend(
                    channel
                        .pending
                        .iter()
                        .map(|(id, bytes)| input_frame(self.tab, ch, *id, bytes)),
                );
            }
            ToBrowser::Reset { ch, epoch, .. } => {
                let Some(channel) = self.channels.get_mut(&ch) else {
                    return;
                };
                channel.epoch = Some(epoch);
                channel.rendered = 0;
                channel.acked = 0;
                channel.pending.clear();
                channel.phase = Phase::Live;
                channel.decoder = Decoder::new(channel.codec);
                channel.last_ack_ms = now_ms;
                events.push(Event::Reset { ch });
            }
            ToBrowser::InAck { ch, id, .. } => {
                if let Some(channel) = self.channels.get_mut(&ch) {
                    channel.pending.retain(|(pending_id, _)| *pending_id > id);
                }
            }
            ToBrowser::Exited { ch, reason, .. } => {
                if self.channels.remove(&ch).is_some() {
                    events.push(Event::Exited { ch, reason });
                }
            }
            ToBrowser::Error {
                ch, req, code, msg, ..
            } => {
                if ch.is_some_and(|ch| !self.channels.contains_key(&ch)) {
                    return;
                }
                if let Some(req) = req {
                    self.channels
                        .retain(|_, channel| channel.phase != Phase::Opening || channel.req != req);
                }
                events.push(Event::Error { ch, req, code, msg });
            }
        }
    }

    /// Decodes output, trims plain replayed bytes and acknowledges progress.
    pub fn on_binary(&mut self, bytes: &[u8], now_ms: u64) -> (Vec<Frame>, Vec<Event>) {
        self.last_rx_ms = now_ms;
        let mut frames = Vec::new();
        let mut events = Vec::new();
        if let Ok(Data::Out {
            tab,
            ch,
            epoch,
            off,
            compressed,
            bytes,
        }) = Data::decode(bytes)
        {
            if tab != self.tab {
                return (frames, events);
            }
            let Some(channel) = self.channels.get_mut(&ch) else {
                return (frames, events);
            };
            let decoded;
            let bytes = if compressed {
                if channel.epoch != Some(epoch) || channel.phase != Phase::Live {
                    return (frames, events);
                }
                if off != channel.rendered || channel.codec == Codec::None {
                    channel.phase = Phase::NeedsResume;
                    return (frames, events);
                }
                match channel.decoder.decode(bytes) {
                    Ok(output) => decoded = output,
                    Err(_) => {
                        channel.phase = Phase::NeedsResume;
                        return (frames, events);
                    }
                }
                decoded.as_slice()
            } else {
                bytes
            };
            let Some(end) = off.checked_add(bytes.len() as u64) else {
                return (frames, events);
            };
            if channel.epoch != Some(epoch) || off > channel.rendered || end <= channel.rendered {
                return (frames, events);
            }
            let skip = (channel.rendered - off) as usize;
            events.push(Event::Output {
                ch,
                bytes: bytes[skip..].to_vec(),
            });
            channel.rendered = end;
            if channel.rendered - channel.acked >= self.config.ack_bytes {
                frames.extend(channel.ack(self.tab, ch, now_ms));
            }
        }
        (frames, events)
    }

    fn request(&mut self) -> Req {
        let req = self.next_req;
        self.next_req = self.next_req.wrapping_add(1);
        req
    }

    /// Asks for the session list. The frame is `None` while not connected.
    pub fn list(&mut self) -> (Req, Option<Frame>) {
        let req = self.request();
        (
            req,
            self.welcomed
                .then(|| text_frame(&ToAgent::List { tab: self.tab, req })),
        )
    }

    /// Opens a terminal on `session`, creating it from a preset if `create`
    /// is set. Allocates the channel now; the frame is `None` while not
    /// connected, and the open is sent after the next `welcome`.
    pub fn open(
        &mut self,
        session: SessionName,
        create: Option<Create>,
        cols: u16,
        rows: u16,
    ) -> (Ch, Req, Option<Frame>) {
        let ch = self.next_ch;
        self.next_ch = self.next_ch.wrapping_add(1);
        let req = self.request();
        let codec = choose(&self.advertised, &self.config.codecs);
        let channel = Channel {
            codec,
            decoder: Decoder::new(codec),
            session,
            create,
            cols,
            rows,
            req,
            epoch: None,
            phase: Phase::Opening,
            rendered: 0,
            acked: 0,
            last_ack_ms: 0,
            next_in_id: 1,
            pending: Vec::new(),
        };
        let frame = self.welcomed.then(|| channel.open_frame(self.tab, ch));
        self.channels.insert(ch, channel);
        (ch, req, frame)
    }

    /// Keystrokes for `ch`. Empty while disconnected or while the channel
    /// is not live: input is paused, never queued for later.
    pub fn input(&mut self, ch: Ch, bytes: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        if !self.welcomed || !self.online {
            return frames;
        }
        let Some(channel) = self.channels.get_mut(&ch) else {
            return frames;
        };
        if channel.phase != Phase::Live {
            return frames;
        }
        for bytes in bytes.chunks(limits::MAX_IN_PAYLOAD) {
            let id = channel.next_in_id;
            let Some(next) = id.checked_add(1) else {
                break;
            };
            channel.next_in_id = next;
            frames.push(input_frame(self.tab, ch, id, bytes));
            channel.pending.push((id, bytes.to_vec()));
        }
        frames
    }

    /// Stores a channel's terminal size and sends it when connected.
    pub fn resize(&mut self, ch: Ch, cols: u16, rows: u16) -> Option<Frame> {
        let channel = self.channels.get_mut(&ch)?;
        channel.cols = cols;
        channel.rows = rows;
        self.welcomed.then(|| {
            text_frame(&ToAgent::Resize {
                tab: self.tab,
                ch,
                cols,
                rows,
            })
        })
    }

    /// Closes the channel locally and tells the service to detach.
    pub fn close_channel(&mut self, ch: Ch) -> Option<Frame> {
        self.channels.remove(&ch)?;
        self.welcomed
            .then(|| text_frame(&ToAgent::Close { tab: self.tab, ch }))
    }

    /// Call every ~100 ms: acks, pings, dead-link detection.
    pub fn tick(&mut self, now_ms: u64) -> (Vec<Frame>, Vec<Event>) {
        let mut frames = Vec::new();
        let mut events = Vec::new();
        if !self.socket_open {
            return (frames, events);
        }
        for (&ch, channel) in &mut self.channels {
            if channel.phase == Phase::NeedsResume && self.welcomed && self.online {
                frames.extend(channel.resume_frame(
                    self.tab,
                    ch,
                    choose(&self.advertised, &self.config.codecs),
                ));
            }
            if channel.phase == Phase::Live
                && channel.rendered > channel.acked
                && now_ms.saturating_sub(channel.last_ack_ms) >= self.config.ack_ms
            {
                frames.extend(channel.ack(self.tab, ch, now_ms));
            }
        }
        if now_ms.saturating_sub(self.last_ping_ms) >= self.config.ping_ms {
            frames.push(Frame::Text(PING_TEXT.into()));
            self.last_ping_ms = now_ms;
        }
        if !self.dead_reported && now_ms.saturating_sub(self.last_rx_ms) >= self.config.dead_ms {
            events.push(Event::LinkDead);
            self.dead_reported = true;
        }
        (frames, events)
    }

    /// The channel's current epoch, once opened.
    pub fn epoch(&self, ch: Ch) -> Option<Epoch> {
        self.channels.get(&ch)?.epoch
    }

    /// Bytes of this channel's current epoch already rendered.
    pub fn rendered(&self, ch: Ch) -> Option<u64> {
        self.channels.get(&ch).map(|channel| channel.rendered)
    }
}

/// Reconnect delay for attempt 0, 1, 2, ...: 500 ms doubling to a 5 s cap,
/// then scaled by `0.8 + 0.4 * jitter` where `jitter` is in `[0, 1]`.
pub fn backoff_ms(attempt: u32, jitter: f64) -> u64 {
    let base = (500_u64 << attempt.min(4)).min(5000);
    (base as f64 * (0.8 + 0.4 * jitter.clamp(0.0, 1.0))) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_ack_across_legal_output_frames() {
        let mut client = Client::new(7, ClientConfig::default());
        client.on_open(0);
        client.on_text(
            &serde_json::to_string(&Hop::Welcome {
                proto: PROTO,
                host: Some("host".into()),
                agent: Some(AgentStatus {
                    online: true,
                    ..Default::default()
                }),
            })
            .unwrap(),
            0,
        );
        let (ch, req, _) = client.open(SessionName::new("shell").unwrap(), None, 80, 24);
        client.on_text(
            &serde_json::to_string(&ToBrowser::Opened {
                tab: 7,
                ch,
                req,
                epoch: 50,
            })
            .unwrap(),
            0,
        );
        let payload = vec![b'x'; limits::MAX_OUT_PAYLOAD];
        for index in 0..2 {
            let bytes = Data::Out {
                tab: 7,
                ch,
                epoch: 50,
                off: index * payload.len() as u64,
                compressed: false,
                bytes: &payload,
            }
            .to_vec()
            .unwrap();
            let (frames, events) = client.on_binary(&bytes, 10);
            assert_eq!(
                events,
                vec![Event::Output {
                    ch,
                    bytes: payload.clone()
                }]
            );
            if index == 0 {
                assert!(frames.is_empty());
            } else {
                assert_eq!(
                    frames,
                    vec![text_frame(&ToAgent::Ack {
                        tab: 7,
                        ch,
                        epoch: 50,
                        off: limits::ACK_EVERY_BYTES as u64,
                    })]
                );
            }
        }
        assert!(client.tick(250).0.is_empty());
    }
}
