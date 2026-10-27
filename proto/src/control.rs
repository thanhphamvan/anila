//! JSON text frames. Every frame is an object with a `t` field.

use serde::{Deserialize, Serialize};

use crate::{Ch, Epoch, InputId, Off, Req, SessionName, Tab};

/// Between one peer and the host room. The room handles these itself.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Hop {
    /// Browser → room, first frame on every browser socket.
    Hello { proto: u16, tab: Tab },
    /// Room → browser (with `host` and `agent`) or room → service (bare).
    Welcome {
        proto: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<AgentStatus>,
    },
    /// Room → browser whenever the service connects or drops.
    Status(AgentStatus),
    /// Room → service, first frame on every service socket.
    Challenge { nonce: String },
    /// Service → room, answering `challenge`.
    Auth {
        proto: u16,
        host: String,
        ts: i64,
        sig: String,
        #[serde(default)]
        codecs: Vec<Codec>,
        /// The service understands `watch` frames (`docs/optimizations.md`,
        /// O3). Absent from older services, which always heartbeat.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        watch: bool,
    },
    /// Room → service, only to a service that sent `watch: true`: whether
    /// any browser is connected. While unwatched the service sends
    /// `PING_TEXT` instead of `heartbeat`, so the room can hibernate; on
    /// `on: true` it sends a heartbeat at once and keeps heartbeating.
    Watch { on: bool },
    /// Room → service: a browser tab's socket closed; start its grace timer.
    TabGone { tab: Tab },
    /// Browser → room. Answered by the room's auto-response without waking it.
    Ping,
    /// Room → browser or service.
    Pong,
    /// Service → room every 10 s while watched (always, for a service or
    /// room without `watch`). Unlike `ping` it reaches the room, which
    /// records it and replies `pong`; a room alarm closes a service that has
    /// been silent for 25 s with code 4408.
    Heartbeat,
    /// Room → any peer, followed by a close.
    Error { code: ErrorCode, msg: String },
}

/// Exact ping text, so the room can answer it with a websocket
/// auto-response without waking up.
pub const PING_TEXT: &str = r#"{"t":"ping"}"#;
pub const PONG_TEXT: &str = r#"{"t":"pong"}"#;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct AgentStatus {
    pub online: bool,
    /// Unix seconds of the last change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<u16>,
    #[serde(default)]
    pub codecs: Vec<Codec>,
}

/// Routed browser → service. Every variant carries `tab`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToAgent {
    List {
        tab: Tab,
        req: Req,
    },
    Open {
        tab: Tab,
        ch: Ch,
        req: Req,
        session: SessionName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        create: Option<Create>,
        cols: u16,
        rows: u16,
        #[serde(default)]
        codec: Codec,
    },
    Resume {
        tab: Tab,
        ch: Ch,
        session: SessionName,
        epoch: Epoch,
        from: Off,
        cols: u16,
        rows: u16,
        #[serde(default)]
        codec: Codec,
    },
    Ack {
        tab: Tab,
        ch: Ch,
        epoch: Epoch,
        off: Off,
    },
    Resize {
        tab: Tab,
        ch: Ch,
        cols: u16,
        rows: u16,
    },
    Close {
        tab: Tab,
        ch: Ch,
    },
}

/// Routed service → browser. Every variant carries `tab`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToBrowser {
    Sessions {
        tab: Tab,
        req: Req,
        items: Vec<SessionInfo>,
        presets: Vec<Preset>,
    },
    Opened {
        tab: Tab,
        ch: Ch,
        req: Req,
        epoch: Epoch,
    },
    Resumed {
        tab: Tab,
        ch: Ch,
        epoch: Epoch,
        from: Off,
        in_acked: InputId,
    },
    Reset {
        tab: Tab,
        ch: Ch,
        epoch: Epoch,
    },
    InAck {
        tab: Tab,
        ch: Ch,
        id: InputId,
    },
    Exited {
        tab: Tab,
        ch: Ch,
        reason: String,
    },
    Error {
        tab: Tab,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ch: Option<Ch>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        req: Option<Req>,
        code: ErrorCode,
        msg: String,
    },
}

impl ToAgent {
    pub fn tab(&self) -> Tab {
        match self {
            ToAgent::List { tab, .. }
            | ToAgent::Open { tab, .. }
            | ToAgent::Resume { tab, .. }
            | ToAgent::Ack { tab, .. }
            | ToAgent::Resize { tab, .. }
            | ToAgent::Close { tab, .. } => *tab,
        }
    }
}

impl ToBrowser {
    pub fn tab(&self) -> Tab {
        match self {
            ToBrowser::Sessions { tab, .. }
            | ToBrowser::Opened { tab, .. }
            | ToBrowser::Resumed { tab, .. }
            | ToBrowser::Reset { tab, .. }
            | ToBrowser::InAck { tab, .. }
            | ToBrowser::Exited { tab, .. }
            | ToBrowser::Error { tab, .. } => *tab,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Create {
    pub preset: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    pub windows: u16,
    pub attached: u16,
    /// Unix seconds of the last activity.
    pub activity: i64,
    /// Command in the active pane of the active window, e.g. `claude`.
    pub cmd: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Preset {
    pub id: String,
    pub label: String,
}

/// Output payload codec, chosen per channel by the browser from the list
/// the service advertises. The codec stream restarts at every `opened`,
/// `resumed` and `reset`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    #[default]
    None,
    Lz4,
    Zstd,
    ZstdStream,
    DeflateStream,
    /// A codec this build does not know. Never chosen.
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadFrame,
    UnsupportedProto,
    Unauthorized,
    StepUp,
    Forbidden,
    AgentOffline,
    NotFound,
    Exists,
    InvalidName,
    Limit,
    Internal,
    /// A code this build does not know.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_and_pong_match_auto_response_strings() {
        assert_eq!(serde_json::to_string(&Hop::Ping).unwrap(), PING_TEXT);
        assert_eq!(serde_json::to_string(&Hop::Pong).unwrap(), PONG_TEXT);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let f: ToAgent =
            serde_json::from_str(r#"{"t":"close","tab":1,"ch":2,"later":true}"#).unwrap();
        assert_eq!(f, ToAgent::Close { tab: 1, ch: 2 });
    }

    #[test]
    fn unknown_codec_and_error_code_do_not_fail() {
        let s: AgentStatus =
            serde_json::from_str(r#"{"online":true,"codecs":["zstd","brotli"]}"#).unwrap();
        assert_eq!(s.codecs, vec![Codec::Zstd, Codec::Unknown]);
        let e: Hop = serde_json::from_str(r#"{"t":"error","code":"teapot","msg":""}"#).unwrap();
        assert_eq!(
            e,
            Hop::Error {
                code: ErrorCode::Unknown,
                msg: String::new()
            }
        );
    }
}
