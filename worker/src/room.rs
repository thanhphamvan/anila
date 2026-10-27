//! The host room: one Durable Object per host machine. It pairs browser
//! sockets with the service socket and forwards routed frames unchanged,
//! reading only `t` and `tab` (contracts C2, C3, C4).

use std::time::Duration;

use anila_proto::control::{PING_TEXT, PONG_TEXT};
use anila_proto::data::{OUT_HEADER, peek_tab};
use anila_proto::limits::{
    AGENT_SKEW_SECS, DEAD_AFTER_SECS, HUNG_AGENT_CHECK_SECS, MAX_OUT_PAYLOAD, MAX_TABS_PER_ROOM,
    MAX_TEXT_FRAME, WATCH_PROBE_SECS, close,
};
use anila_proto::{
    AgentStatus, Codec, ErrorCode, Hop, PROTO, Route, Tab, ToBrowser, classify,
    service_proto_supported,
};
use serde::{Deserialize, Serialize};
use worker::*;

use crate::{H_ACCOUNT, H_EXPIRES, H_HOST, H_HOST_KEY, H_HOST_NAME, H_ROLE, H_SID_HASH, util};

const TAG_BROWSER: &str = "browser";
const TAG_AGENT: &str = "agent";
const KEY_SINCE: &str = "agent_since";

/// Saved on each socket so it survives hibernation.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "role", rename_all = "snake_case")]
enum Peer {
    Browser {
        host: String,
        tab: Option<Tab>,
        sid_hash: Option<String>,
        expires_at: Option<i64>,
        #[serde(default)]
        account: Option<String>,
        #[serde(default)]
        host_name: Option<String>,
    },
    Agent {
        host: String,
        nonce: String,
        #[serde(default)]
        key: Option<String>,
        authed: bool,
        #[serde(default)]
        last_seen: i64,
        #[serde(default)]
        watch: bool,
        #[serde(default)]
        watch_since: Option<i64>,
        /// A heartbeat arrived since `watch_since`. Kept apart from
        /// `last_seen`, whose whole seconds cannot order a heartbeat sent
        /// just before `watch { on: true }`.
        #[serde(default)]
        beat_since_watch: bool,
        proto: u16,
        codecs: Vec<Codec>,
    },
}

#[derive(Deserialize)]
struct Revoke {
    sid_hash: Option<String>,
    account: Option<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    host_removed: bool,
}

impl Revoke {
    fn has_target(&self) -> bool {
        self.all || self.host_removed || self.sid_hash.is_some() || self.account.is_some()
    }

    fn close_for(&self, peer: &Peer) -> Option<(u16, &'static str)> {
        if self.host_removed {
            return Some((close::FORBIDDEN, "machine removed"));
        }
        if let Peer::Browser {
            sid_hash, account, ..
        } = peer
        {
            if self.account.is_some() && self.account == *account {
                return Some((close::FORBIDDEN, "access removed"));
            }
            if self.all || (self.sid_hash.is_some() && self.sid_hash == *sid_hash) {
                return Some((close::UNAUTHORIZED, "signed out"));
            }
        }
        None
    }
}

#[durable_object]
pub struct HostRoom {
    state: State,
    env: Env,
}

impl DurableObject for HostRoom {
    fn new(state: State, env: Env) -> Self {
        // Pings are answered without waking the room.
        if let Ok(pair) = worker_sys::WebSocketRequestResponsePair::new(PING_TEXT, PONG_TEXT) {
            state.set_websocket_auto_response(&pair);
        }
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        if req.method() == Method::Post
            && req.path() == "/internal/revoke"
            && req.headers().get(H_ROLE)?.is_none()
        {
            let body = match req.json::<Revoke>().await {
                Ok(body) if body.has_target() => body,
                _ => return util::json_error(400, "bad_frame", "expected a revocation target"),
            };
            if body.host_removed {
                let now = util::now_secs();
                self.state.storage().put(KEY_SINCE, now).await?;
                self.broadcast_status(AgentStatus {
                    online: false,
                    since: Some(now),
                    ..AgentStatus::default()
                })?;
            }
            for ws in self.state.get_websockets() {
                if let Some(mut peer) = ws.deserialize_attachment::<Peer>()?
                    && let Some((code, reason)) = body.close_for(&peer)
                {
                    if let Peer::Agent { authed, .. } = &mut peer {
                        *authed = false;
                        ws.serialize_attachment(peer)?;
                    }
                    shut(&ws, code, reason)?;
                }
            }
            return Response::from_json(&serde_json::json!({}));
        }
        if req.method() == Method::Post
            && req.path() == "/internal/status"
            && req.headers().get(H_ROLE)?.is_none()
        {
            return Response::from_json(&self.agent_status().await?);
        }
        let h = req.headers();
        let host = h.get(H_HOST)?.unwrap_or_default();
        let pair = WebSocketPair::new()?;
        let server = pair.server;
        match h.get(H_ROLE)?.as_deref() {
            Some("browser") => {
                self.state
                    .accept_websocket_with_tags(&server, &[TAG_BROWSER]);
                server.serialize_attachment(Peer::Browser {
                    host,
                    tab: None,
                    sid_hash: h.get(H_SID_HASH)?,
                    expires_at: h.get(H_EXPIRES)?.and_then(|v| v.parse().ok()),
                    account: h.get(H_ACCOUNT)?,
                    host_name: h.get(H_HOST_NAME)?,
                })?;
            }
            Some("agent") => {
                let nonce = util::hex(&util::random_bytes::<32>());
                self.state.accept_websocket_with_tags(&server, &[TAG_AGENT]);
                server.serialize_attachment(Peer::Agent {
                    host,
                    nonce: nonce.clone(),
                    key: h.get(H_HOST_KEY)?,
                    authed: false,
                    last_seen: util::now_secs(),
                    watch: false,
                    watch_since: None,
                    beat_since_watch: false,
                    proto: 0,
                    codecs: vec![],
                })?;
                send(&server, &Hop::Challenge { nonce })?;
            }
            _ => return util::json_error(400, "bad_frame", "missing role"),
        }
        Response::from_websocket(pair.client)
    }

    async fn alarm(&self) -> Result<Response> {
        let now = util::now_secs();
        for ws in self.state.get_websockets_with_tag(TAG_BROWSER) {
            if let Some(Peer::Browser {
                expires_at: Some(expires),
                ..
            }) = ws.deserialize_attachment::<Peer>()?
                && now >= expires
            {
                shut(&ws, close::UNAUTHORIZED, "session expired")?;
            }
        }
        if let Some((ws, _)) = self.authed_agent(None)?
            && let Some(mut peer) = ws.deserialize_attachment::<Peer>()?
            && let Peer::Agent {
                authed,
                last_seen,
                watch,
                watch_since,
                beat_since_watch,
                ..
            } = &mut peer
        {
            let now = util::now_secs();
            if agent_hung(now, *last_seen, *watch, *watch_since, *beat_since_watch) {
                *authed = false;
                ws.serialize_attachment(peer)?;
                shut(&ws, close::TIMEOUT, "no heartbeat")?;
                self.state.storage().put(KEY_SINCE, now).await?;
                self.broadcast_status(AgentStatus {
                    online: false,
                    since: Some(now),
                    proto: None,
                    codecs: vec![],
                })?;
            }
        }
        if self.has_watching_browser(None)? {
            self.state
                .storage()
                .set_alarm(Duration::from_secs(HUNG_AGENT_CHECK_SECS))
                .await?;
        }
        Response::ok("")
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        let Some(peer) = ws.deserialize_attachment::<Peer>()? else {
            return shut(&ws, close::BAD_FRAME, "unknown socket");
        };
        match message {
            WebSocketIncomingMessage::String(text) => {
                if text.len() > MAX_TEXT_FRAME {
                    return shut(&ws, close::TOO_BIG, "text frame too large");
                }
                match classify(&text) {
                    Some(Route::Hop) => self.on_hop(&ws, peer, &text).await,
                    Some(Route::Routed(tab)) => {
                        self.route(&ws, &peer, tab, |to| to.send_with_str(&text))
                    }
                    None => shut(&ws, close::BAD_FRAME, "unreadable frame"),
                }
            }
            WebSocketIncomingMessage::Binary(bytes) => {
                if bytes.len() > OUT_HEADER + MAX_OUT_PAYLOAD {
                    return shut(&ws, close::TOO_BIG, "binary frame too large");
                }
                match peek_tab(&bytes) {
                    Some(tab) => self.route(&ws, &peer, tab, |to| to.send_with_bytes(&bytes)),
                    None => shut(&ws, close::BAD_FRAME, "unreadable frame"),
                }
            }
        }
    }

    async fn websocket_close(
        &self,
        ws: WebSocket,
        _code: usize,
        _reason: String,
        _clean: bool,
    ) -> Result<()> {
        self.on_gone(&ws).await
    }

    async fn websocket_error(&self, ws: WebSocket, _error: Error) -> Result<()> {
        self.on_gone(&ws).await
    }
}

impl HostRoom {
    async fn on_hop(&self, ws: &WebSocket, peer: Peer, text: &str) -> Result<()> {
        let Ok(hop) = serde_json::from_str::<Hop>(text) else {
            return shut(ws, close::BAD_FRAME, "malformed hop frame");
        };
        match (peer, hop) {
            (
                Peer::Browser {
                    host,
                    sid_hash,
                    expires_at,
                    account,
                    host_name,
                    ..
                },
                Hop::Hello { proto, tab },
            ) => {
                if proto != PROTO {
                    send(
                        ws,
                        &Hop::Error {
                            code: ErrorCode::UnsupportedProto,
                            msg: format!("room speaks proto {PROTO}"),
                        },
                    )?;
                    return shut(ws, close::BAD_FRAME, "unsupported proto");
                }
                if tab == 0 {
                    return shut(ws, close::BAD_FRAME, "tab must be nonzero");
                }
                // One socket per tab: the newer one wins.
                let mut tabs = 0;
                let mut first_watcher = true;
                for other in self.state.get_websockets_with_tag(TAG_BROWSER) {
                    if let Some(Peer::Browser { tab: Some(t), .. }) =
                        other.deserialize_attachment::<Peer>()?
                    {
                        if t == tab {
                            let _ = other
                                .close(Some(close::REPLACED), Some("replaced by a newer socket"));
                        } else {
                            tabs += 1;
                            let socket: &web_sys::WebSocket = other.as_ref();
                            if !same(&other, ws) && socket.ready_state() == web_sys::WebSocket::OPEN
                            {
                                first_watcher = false;
                            }
                        }
                    }
                }
                if tabs >= MAX_TABS_PER_ROOM {
                    send(
                        ws,
                        &Hop::Error {
                            code: ErrorCode::Limit,
                            msg: "too many tabs".into(),
                        },
                    )?;
                    return shut(ws, close::BAD_FRAME, "too many tabs");
                }
                ws.serialize_attachment(Peer::Browser {
                    host: host.clone(),
                    tab: Some(tab),
                    sid_hash,
                    expires_at,
                    account,
                    host_name: host_name.clone(),
                })?;
                if first_watcher && self.set_agent_watch(true)? {
                    self.state
                        .storage()
                        .set_alarm(Duration::from_secs(WATCH_PROBE_SECS))
                        .await?;
                } else if self.state.storage().get_alarm().await?.is_none() {
                    self.state
                        .storage()
                        .set_alarm(Duration::from_secs(HUNG_AGENT_CHECK_SECS))
                        .await?;
                }
                let agent = self.agent_status().await?;
                send(
                    ws,
                    &Hop::Welcome {
                        proto: PROTO,
                        host: Some(host_name.unwrap_or(host)),
                        agent: Some(agent),
                    },
                )
            }
            (
                Peer::Agent {
                    host,
                    nonce,
                    key,
                    authed: false,
                    ..
                },
                Hop::Auth {
                    proto,
                    host: claimed,
                    ts,
                    sig,
                    codecs,
                    watch,
                },
            ) => {
                if claimed != host || !service_proto_supported(proto) {
                    send(
                        ws,
                        &Hop::Error {
                            code: ErrorCode::UnsupportedProto,
                            msg: format!("room speaks proto {PROTO}"),
                        },
                    )?;
                    return shut(ws, close::BAD_FRAME, "unsupported proto or wrong host");
                }
                if util::now_secs().abs_diff(ts) > AGENT_SKEW_SECS as u64 {
                    return shut(ws, close::UNAUTHORIZED, "clock skew too large");
                }
                if !self.verify_agent(&host, key.as_deref(), ts, &nonce, &sig)? {
                    return shut(ws, close::UNAUTHORIZED, "bad signature");
                }
                for other in self.state.get_websockets_with_tag(TAG_AGENT) {
                    if let Some(Peer::Agent { authed: true, .. }) =
                        other.deserialize_attachment::<Peer>()?
                    {
                        let _ =
                            other.close(Some(close::REPLACED), Some("replaced by a newer service"));
                    }
                }
                let watching = self.has_watching_browser(None)?;
                let now = util::now_secs();
                ws.serialize_attachment(Peer::Agent {
                    host,
                    nonce,
                    key,
                    authed: true,
                    watch,
                    watch_since: (watch && watching).then_some(now),
                    beat_since_watch: false,
                    proto,
                    codecs: codecs.clone(),
                    last_seen: util::now_secs(),
                })?;
                if watching {
                    if watch {
                        self.state
                            .storage()
                            .set_alarm(Duration::from_secs(WATCH_PROBE_SECS))
                            .await?;
                    } else if self.state.storage().get_alarm().await?.is_none() {
                        self.state
                            .storage()
                            .set_alarm(Duration::from_secs(HUNG_AGENT_CHECK_SECS))
                            .await?;
                    }
                }
                send(
                    ws,
                    &Hop::Welcome {
                        proto: PROTO,
                        host: None,
                        agent: None,
                    },
                )?;
                if watch {
                    send(ws, &Hop::Watch { on: watching })?;
                }
                self.state
                    .storage()
                    .put(KEY_SINCE, util::now_secs())
                    .await?;
                self.broadcast_status(AgentStatus {
                    online: true,
                    since: Some(util::now_secs()),
                    proto: Some(proto),
                    codecs,
                })
            }
            (mut peer, Hop::Heartbeat) => {
                if let Peer::Agent {
                    authed: true,
                    last_seen,
                    beat_since_watch,
                    ..
                } = &mut peer
                {
                    *last_seen = util::now_secs();
                    *beat_since_watch = true;
                    ws.serialize_attachment(peer)?;
                }
                ws.send_with_str(PONG_TEXT)
            }
            // The auto-response normally answers pings; this covers the rest.
            (_, Hop::Ping) => ws.send_with_str(PONG_TEXT),
            (_, Hop::Pong) => Ok(()),
            _ => shut(ws, close::BAD_FRAME, "unexpected hop frame"),
        }
    }

    /// Forwards a routed frame to the other side of `tab`.
    fn route(
        &self,
        from: &WebSocket,
        peer: &Peer,
        tab: Tab,
        deliver: impl Fn(&WebSocket) -> Result<()>,
    ) -> Result<()> {
        match peer {
            Peer::Browser { tab: own, .. } => {
                if *own != Some(tab) {
                    return shut(from, close::FORBIDDEN, "frame carries another tab");
                }
                match self.authed_agent(None)? {
                    Some((agent, _)) => deliver(&agent),
                    None => send(
                        from,
                        &ToBrowser::Error {
                            tab,
                            ch: None,
                            req: None,
                            code: ErrorCode::AgentOffline,
                            msg: "host is offline".into(),
                        },
                    ),
                }
            }
            Peer::Agent { authed: true, .. } => {
                // A frame for a tab that already left is dropped.
                for b in self.state.get_websockets_with_tag(TAG_BROWSER) {
                    if let Some(Peer::Browser { tab: Some(t), .. }) =
                        b.deserialize_attachment::<Peer>()?
                        && t == tab
                    {
                        return deliver(&b);
                    }
                }
                Ok(())
            }
            Peer::Agent { authed: false, .. } => {
                shut(from, close::UNAUTHORIZED, "authenticate first")
            }
        }
    }

    async fn on_gone(&self, ws: &WebSocket) -> Result<()> {
        match ws.deserialize_attachment::<Peer>()? {
            Some(Peer::Browser { tab: Some(tab), .. }) => {
                // A newer socket for the same tab may have replaced this one.
                let still_here = self
                    .state
                    .get_websockets_with_tag(TAG_BROWSER)
                    .iter()
                    .any(|b| {
                        !same(b, ws)
                            && matches!(b.deserialize_attachment::<Peer>(),
                            Ok(Some(Peer::Browser { tab: Some(t), .. })) if t == tab)
                    });
                if !still_here && let Some((agent, _)) = self.authed_agent(None)? {
                    send(&agent, &Hop::TabGone { tab })?;
                }
                if !self.has_watching_browser(Some(ws))? {
                    self.set_agent_watch(false)?;
                }
                Ok(())
            }
            Some(Peer::Browser { tab: None, .. }) => {
                if !self.has_watching_browser(Some(ws))? {
                    self.set_agent_watch(false)?;
                }
                Ok(())
            }
            Some(Peer::Agent { authed: true, .. }) => {
                // Another authed service may already have replaced this one.
                // The closing socket is still listed while its close event runs.
                if self.authed_agent(Some(ws))?.is_some() {
                    return Ok(());
                }
                let now = util::now_secs();
                self.state.storage().put(KEY_SINCE, now).await?;
                self.broadcast_status(AgentStatus {
                    online: false,
                    since: Some(now),
                    proto: None,
                    codecs: vec![],
                })
            }
            _ => Ok(()),
        }
    }

    fn has_watching_browser(&self, except: Option<&WebSocket>) -> Result<bool> {
        for ws in self.state.get_websockets_with_tag(TAG_BROWSER) {
            if except.is_some_and(|e| same(e, &ws)) {
                continue;
            }
            let socket: &web_sys::WebSocket = ws.as_ref();
            if socket.ready_state() == web_sys::WebSocket::OPEN
                && matches!(
                    ws.deserialize_attachment::<Peer>()?,
                    Some(Peer::Browser { tab: Some(_), .. })
                )
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn set_agent_watch(&self, on: bool) -> Result<bool> {
        if let Some((ws, _)) = self.authed_agent(None)?
            && let Some(mut peer) = ws.deserialize_attachment::<Peer>()?
            && let Peer::Agent {
                watch: true,
                watch_since,
                beat_since_watch,
                ..
            } = &mut peer
        {
            *watch_since = on.then(util::now_secs);
            *beat_since_watch = false;
            send(&ws, &Hop::Watch { on })?;
            ws.serialize_attachment(peer)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn authed_agent(&self, except: Option<&WebSocket>) -> Result<Option<(WebSocket, AgentStatus)>> {
        for ws in self.state.get_websockets_with_tag(TAG_AGENT) {
            if except.is_some_and(|e| same(e, &ws)) {
                continue;
            }
            if let Some(Peer::Agent {
                authed: true,
                proto,
                codecs,
                ..
            }) = ws.deserialize_attachment::<Peer>()?
            {
                let status = AgentStatus {
                    online: true,
                    since: None,
                    proto: Some(proto),
                    codecs,
                };
                return Ok(Some((ws, status)));
            }
        }
        Ok(None)
    }

    async fn agent_status(&self) -> Result<AgentStatus> {
        let since = self
            .state
            .storage()
            .get::<i64>(KEY_SINCE)
            .await
            .ok()
            .flatten();
        Ok(match self.authed_agent(None)? {
            Some((_, status)) => AgentStatus { since, ..status },
            None => AgentStatus {
                online: false,
                since,
                proto: None,
                codecs: vec![],
            },
        })
    }

    fn broadcast_status(&self, status: AgentStatus) -> Result<()> {
        let frame = serde_json::to_string(&Hop::Status(status))?;
        for b in self.state.get_websockets_with_tag(TAG_BROWSER) {
            if let Some(Peer::Browser { tab: Some(_), .. }) = b.deserialize_attachment::<Peer>()? {
                let _ = b.send_with_str(&frame);
            }
        }
        Ok(())
    }

    /// Checks the service's ed25519 signature over
    /// `anila_proto::sig::ws_sign_input(host, ts, nonce)` against its stored key.
    fn verify_agent(
        &self,
        host: &str,
        key: Option<&str>,
        ts: i64,
        nonce: &str,
        sig: &str,
    ) -> Result<bool> {
        if let Some(key) = key {
            return Ok(crate::agent_sig::verify_with_key(
                key,
                anila_proto::sig::ws_sign_input(host, ts, nonce).as_bytes(),
                sig,
            ));
        }
        let keys = self
            .env
            .var("HOST_KEYS")
            .map_err(|e| e.to_string())
            .and_then(|value| crate::agent_sig::HostKeys::parse(&value.to_string()));
        let verified = match keys {
            Ok(keys) => keys.verify(
                host,
                anila_proto::sig::ws_sign_input(host, ts, nonce).as_bytes(),
                sig,
            ),
            Err(e) => {
                worker::console_error!("failed to load HOST_KEYS: {e}");
                false
            }
        };
        Ok(verified || util::insecure_dev_allowed(&self.env)?)
    }
}

/// Whether the authed service counts as hung (`docs/optimizations.md`, O3).
/// A service without `watch` must heartbeat every `DEAD_AFTER_SECS`; a
/// watch-capable one only while watched, and its first heartbeat after
/// `watch { on: true }` is due within `WATCH_PROBE_SECS`.
fn agent_hung(
    now: i64,
    last_seen: i64,
    watch: bool,
    watch_since: Option<i64>,
    beat_since_watch: bool,
) -> bool {
    if !watch {
        return now - last_seen > DEAD_AFTER_SECS as i64;
    }
    match watch_since {
        None => false,
        Some(_) if beat_since_watch => now - last_seen > DEAD_AFTER_SECS as i64,
        Some(since) => now - since >= WATCH_PROBE_SECS as i64,
    }
}

fn send<T: Serialize>(ws: &WebSocket, frame: &T) -> Result<()> {
    ws.send_with_str(serde_json::to_string(frame)?)
}

/// Whether two handles refer to the same underlying socket.
fn same(a: &WebSocket, b: &WebSocket) -> bool {
    let a: &web_sys::WebSocket = a.as_ref();
    let b: &web_sys::WebSocket = b.as_ref();
    a == b
}

fn shut(ws: &WebSocket, code: u16, reason: &str) -> Result<()> {
    ws.close(Some(code), Some(reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn browser(account: Option<&str>) -> Peer {
        Peer::Browser {
            host: "machine".into(),
            tab: Some(1),
            sid_hash: Some("hash".into()),
            expires_at: Some(100),
            account: account.map(str::to_owned),
            host_name: Some("studio".into()),
        }
    }

    fn legacy_agent() -> Peer {
        serde_json::from_value(json!({
            "role": "agent", "host": "machine", "nonce": "challenge",
            "authed": true, "proto": 1, "codecs": []
        }))
        .unwrap()
    }

    #[test]
    fn attachments_remain_compatible_across_deploys() {
        let old: Peer = serde_json::from_value(json!({
            "role": "browser", "host": "machine", "tab": 1,
            "sid_hash": "hash", "expires_at": 100
        }))
        .unwrap();
        assert!(matches!(
            old,
            Peer::Browser {
                account: None,
                host_name: None,
                ..
            }
        ));
        assert!(matches!(
            legacy_agent(),
            Peer::Agent {
                key: None,
                last_seen: 0,
                ..
            }
        ));
        let saved = serde_json::to_value(browser(Some("alice"))).unwrap();
        let restored: Peer = serde_json::from_value(saved).unwrap();
        assert!(matches!(restored, Peer::Browser {
            account: Some(account), host_name: Some(name), ..
        } if account == "alice" && name == "studio"));
    }

    #[test]
    fn watch_attachments_default_and_round_trip() {
        let mut peer = legacy_agent();
        assert!(matches!(
            peer,
            Peer::Agent {
                watch: false,
                watch_since: None,
                ..
            }
        ));
        if let Peer::Agent {
            watch, watch_since, ..
        } = &mut peer
        {
            *watch = true;
            *watch_since = Some(100);
        }
        let restored: Peer = serde_json::from_str(&serde_json::to_string(&peer).unwrap()).unwrap();
        assert!(matches!(
            restored,
            Peer::Agent {
                watch: true,
                watch_since: Some(100),
                ..
            }
        ));
    }

    #[test]
    fn hung_check_respects_watch_and_deadline_boundaries() {
        let start = 100;
        let dead = DEAD_AFTER_SECS as i64;
        let probe = WATCH_PROBE_SECS as i64;
        // Without watch: today's rule, whatever the watch fields say.
        assert!(!agent_hung(start + dead, start, false, None, false));
        assert!(agent_hung(start + dead + 1, start, false, None, false));
        // Unwatched: never hung, however long it has been quiet.
        assert!(!agent_hung(start + dead + 100, start, true, None, false));
        // Watched, no heartbeat yet: the probe window, even when the last
        // heartbeat (from before) has the same second as `watch_since`.
        for last_seen in [start - 1, start] {
            assert!(!agent_hung(
                start + probe - 1,
                last_seen,
                true,
                Some(start),
                false
            ));
            assert!(agent_hung(
                start + probe,
                last_seen,
                true,
                Some(start),
                false
            ));
        }
        // Watched and heartbeating: the dead-after rule.
        for last_seen in [start, start + 1] {
            assert!(!agent_hung(
                last_seen + dead,
                last_seen,
                true,
                Some(start),
                true
            ));
            assert!(agent_hung(
                last_seen + dead + 1,
                last_seen,
                true,
                Some(start),
                true
            ));
        }
    }

    #[test]
    fn removing_an_account_only_closes_its_browsers() {
        let revoke: Revoke = serde_json::from_value(json!({"account": "alice"})).unwrap();
        assert!(revoke.has_target());
        assert_eq!(
            revoke.close_for(&browser(Some("alice"))),
            Some((close::FORBIDDEN, "access removed"))
        );
        assert_eq!(revoke.close_for(&browser(Some("bob"))), None);
        assert_eq!(revoke.close_for(&browser(None)), None);
        assert_eq!(revoke.close_for(&legacy_agent()), None);
    }

    #[test]
    fn removing_a_machine_closes_all_peers() {
        let revoke: Revoke = serde_json::from_value(json!({"host_removed": true})).unwrap();
        assert!(revoke.has_target());
        for peer in [browser(Some("alice")), browser(None), legacy_agent()] {
            assert_eq!(
                revoke.close_for(&peer),
                Some((close::FORBIDDEN, "machine removed"))
            );
        }
    }

    #[test]
    fn session_revocation_keeps_existing_close_codes() {
        for body in [json!({"sid_hash": "hash"}), json!({"all": true})] {
            let revoke: Revoke = serde_json::from_value(body).unwrap();
            assert!(revoke.has_target());
            assert_eq!(
                revoke.close_for(&browser(Some("alice"))),
                Some((close::UNAUTHORIZED, "signed out"))
            );
            assert_eq!(revoke.close_for(&legacy_agent()), None);
        }
        let other: Revoke = serde_json::from_value(json!({"sid_hash": "other"})).unwrap();
        assert_eq!(other.close_for(&browser(None)), None);
        for body in [
            json!({}),
            json!({"all": false}),
            json!({"host_removed": false}),
        ] {
            assert!(!serde_json::from_value::<Revoke>(body).unwrap().has_target());
        }
    }
}
