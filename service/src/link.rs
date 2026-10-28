//! The service's websocket link to the host room (contract C3).
//!
//! One long-lived task: connect, answer the challenge, then pump frames
//! between the room and the channel manager until the link dies, then
//! reconnect. The manager survives reconnects, so channels are orphaned on
//! a drop and resumed by the browser afterwards.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anila_proto::control::PING_TEXT;
use anila_proto::{Data, Hop, HostId, PROTO, Route, classify};
use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use rand_core::{OsRng, RngCore};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::channel::{Limits, Manager, Outbound};
use crate::coalesce::Coalescer;
use crate::config::Config;
use crate::keys::ServiceKey;
use crate::pty::PtyEvent;
use crate::spawner::SessionSpawner;
use crate::tmux::Tmux;

/// Connection liveness and retry intervals.
#[derive(Debug, Clone)]
pub struct Timings {
    /// How often the service sends a heartbeat or an unwatched ping.
    pub heartbeat: Duration,
    /// Silence from the room after which the link counts as dead.
    pub dead_after: Duration,
    /// How often the manager's `tick` runs.
    pub tick: Duration,
    /// Maximum time to batch PTY output before framing.
    pub batch: Duration,
    /// Cap for the reconnect backoff (0.5 s, 1 s, 2 s, ... up to this).
    pub max_backoff: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(anila_proto::limits::PING_INTERVAL_SECS),
            dead_after: Duration::from_secs(anila_proto::limits::DEAD_AFTER_SECS),
            tick: Duration::from_millis(100),
            batch: Duration::from_millis(8),
            max_backoff: Duration::from_secs(5),
        }
    }
}

/// The room URL for this host: `https://x` becomes
/// `wss://x/ws/agent?host=<host>` and `http://x` becomes `ws://x/...`.
/// A trailing slash on `worker` is ignored.
pub fn agent_url(worker: &str, host: &HostId) -> anyhow::Result<String> {
    let (scheme, base) = if let Some(base) = worker.strip_prefix("https://") {
        ("wss", base)
    } else if let Some(base) = worker.strip_prefix("http://") {
        ("ws", base)
    } else {
        bail!("worker URL must use http or https");
    };
    Ok(format!(
        "{scheme}://{}/ws/agent?host={host}",
        base.trim_end_matches('/')
    ))
}

/// The websocket upgrade request for `url`, with the Cloudflare Access
/// service token headers (if any).
pub fn upgrade_request(
    url: &str,
    access: &[(&str, String)],
) -> anyhow::Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
    let mut request = url.into_client_request()?;
    for (name, value) in access {
        request.headers_mut().insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(value)?,
        );
    }
    Ok(request)
}

/// Runs until `shutdown` turns true, then detaches every PTY and returns.
///
/// The channel manager is not `Send`, so this future must run on a
/// current-thread runtime or inside a `tokio::task::LocalSet`.
pub async fn run(
    config: Config,
    key: ServiceKey,
    timings: Timings,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let url = agent_url(&config.worker, &config.host)?;
    let access = config.access_headers()?;
    // Says whether a token is set, never the token itself.
    tracing::info!(
        "Cloudflare Access service token: {}",
        if access.is_empty() { "not set" } else { "set" }
    );
    let (events, mut incoming) = tokio::sync::mpsc::unbounded_channel();
    let mut manager = Manager::new(
        Box::new(
            SessionSpawner::new(
                Tmux::new(config.tmux_socket.clone()),
                config.presets.clone(),
                events,
            )
            .with_herdr(config.herdr_binary.clone()),
        ),
        config.public_presets(),
        Limits {
            grace: Duration::from_secs(config.grace_secs),
            buffer_bytes: config.buffer_bytes,
            ..Limits::default()
        },
        Box::new(|| OsRng.next_u32()),
    );
    let mut coalescer = Coalescer::new(timings.batch, anila_proto::limits::MAX_OUT_PAYLOAD);
    let connections = async {
        let initial_backoff = Duration::from_millis(500).min(timings.max_backoff);
        let mut backoff = initial_backoff;
        loop {
            tracing::info!("connecting to host room");
            let attempt = async {
                let (mut socket, _) =
                    tokio_tungstenite::connect_async(upgrade_request(&url, &access)?).await?;
                handshake(&mut socket, &config.host, &key).await?;
                Ok::<_, anyhow::Error>(socket)
            };
            let mut socket = match tokio::time::timeout(timings.dead_after, attempt).await {
                Ok(Ok(socket)) => socket,
                result => {
                    // Avoid logging peer-supplied handshake data or credentials.
                    tracing::warn!(
                        timed_out = result.is_err(),
                        "connection or handshake failed"
                    );
                    if let Ok(Err(err)) = &result {
                        if let Some(tokio_tungstenite::tungstenite::Error::Http(response)) =
                            err.downcast_ref::<tokio_tungstenite::tungstenite::Error>()
                        {
                            tracing::warn!(status = %response.status(), "websocket upgrade failed");
                            if response.status().is_redirection()
                                && response
                                    .headers()
                                    .get("location")
                                    .and_then(|l| l.to_str().ok())
                                    .is_some_and(crate::api::is_access_redirect)
                            {
                                tracing::error!(
                                    "{}",
                                    crate::api::access_blocked(!access.is_empty())
                                );
                                backoff = timings.max_backoff;
                            }
                            if response.status().as_u16() == 404 {
                                tracing::error!(
                                    "this machine is not paired (run anila-service pair)"
                                );
                                backoff = timings.max_backoff;
                            }
                        }
                        if forbidden_close(err) {
                            backoff = timings.max_backoff;
                        }
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2).min(timings.max_backoff);
                    continue;
                }
            };
            backoff = initial_backoff;
            tracing::info!("connected to host room");
            let result = pump(
                &mut socket,
                &mut manager,
                &mut incoming,
                &timings,
                &mut coalescer,
            )
            .await;
            for ((tab, ch, epoch), bytes) in coalescer.drain() {
                manager.on_pty_output(tab, ch, epoch, &bytes);
            }
            manager.link_lost(Instant::now());
            drop(socket);
            tracing::info!(failed = result.is_err(), "disconnected from host room");
            if result.as_ref().is_err_and(forbidden_close) {
                tokio::time::sleep(timings.max_backoff).await;
            }
        }
    };
    tokio::select! {
        biased;
        _ = async {
            while !*shutdown.borrow_and_update() {
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
        } => {},
        _ = connections => {},
    }
    for ((tab, ch, epoch), bytes) in coalescer.drain() {
        manager.on_pty_output(tab, ch, epoch, &bytes);
    }
    drop(manager);
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("host room closed the connection")]
struct RoomClosed {
    code: u16,
}

fn room_closed(frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> RoomClosed {
    let (code, reason) = frame.map_or((1005, String::new()), |frame| {
        (
            u16::from(frame.code),
            frame.reason.chars().take(64).collect::<String>(),
        )
    });
    tracing::warn!(code, reason = %reason, "host room closed the connection");
    RoomClosed { code }
}

fn forbidden_close(err: &anyhow::Error) -> bool {
    err.downcast_ref::<RoomClosed>()
        .is_some_and(|closed| closed.code == anila_proto::limits::close::FORBIDDEN)
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn read_hop(socket: &mut Socket) -> anyhow::Result<Hop> {
    match socket
        .next()
        .await
        .context("socket ended during handshake")??
    {
        Message::Text(text) => Ok(serde_json::from_str(&text)?),
        Message::Close(frame) => Err(room_closed(frame).into()),
        _ => bail!("expected handshake text frame"),
    }
}

async fn handshake(socket: &mut Socket, host: &HostId, key: &ServiceKey) -> anyhow::Result<()> {
    let Hop::Challenge { nonce } = read_hop(socket).await? else {
        bail!("expected challenge");
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()?;
    let auth = Hop::Auth {
        proto: PROTO,
        host: host.to_string(),
        ts,
        sig: key.sign_ws(host.as_str(), ts, &nonce),
        codecs: anila_proto::codec::ENCODABLE.to_vec(),
        watch: true,
    };
    socket
        .send(Message::text(serde_json::to_string(&auth)?))
        .await?;
    if !matches!(read_hop(socket).await?, Hop::Welcome { .. }) {
        bail!("expected welcome");
    }
    Ok(())
}

async fn send_outbound(socket: &mut Socket, frames: Vec<Outbound>) -> anyhow::Result<()> {
    for frame in frames {
        let message = match frame {
            Outbound::Text(text) => Message::text(serde_json::to_string(&text)?),
            Outbound::Out {
                tab,
                ch,
                epoch,
                off,
                compressed,
                bytes,
            } => Message::binary(
                Data::Out {
                    tab,
                    ch,
                    epoch,
                    off,
                    bytes: &bytes,
                    compressed,
                }
                .to_vec()?,
            ),
        };
        socket.send(message).await?;
    }
    Ok(())
}

async fn pump(
    socket: &mut Socket,
    manager: &mut Manager,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<PtyEvent>,
    timings: &Timings,
    coalescer: &mut Coalescer,
) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(timings.tick);
    let mut heartbeat = tokio::time::interval(timings.heartbeat);
    let mut last_heard = Instant::now();
    let mut watched = true;
    loop {
        let deadline = coalescer.next_deadline();
        let frames = tokio::select! {
            frame = socket.next() => {
                last_heard = Instant::now();
                match frame {
                    Some(Ok(Message::Text(text))) => match classify(&text) {
                        Some(Route::Hop) => {
                            match serde_json::from_str::<Hop>(&text) {
                                Ok(Hop::Watch { on }) => {
                                    watched = on;
                                    if on {
                                        tokio::time::timeout(
                                            timings.dead_after,
                                            socket.send(Message::text(keepalive(true))),
                                        )
                                        .await
                                        .context("send stalled")??;
                                    }
                                }
                                Ok(Hop::TabGone { tab }) => manager.tab_gone(tab, last_heard),
                                Ok(Hop::Error { code, msg }) => tracing::warn!(?code, %msg, "room error"),
                                Ok(_) => {},
                                Err(err) => tracing::warn!(%err, "invalid hop frame"),
                            }
                            Vec::new()
                        }
                        Some(Route::Routed(_)) => match serde_json::from_str(&text) {
                            Ok(msg) => manager.handle(msg, last_heard),
                            Err(err) => {
                                tracing::warn!(%err, "invalid routed frame");
                                Vec::new()
                            }
                        },
                        None => Vec::new(),
                    },
                    Some(Ok(Message::Binary(bytes))) => match Data::decode(&bytes) {
                        Ok(Data::In { tab, ch, id, bytes }) => manager.input(tab, ch, id, bytes, last_heard),
                        _ => Vec::new(),
                    },
                    None => return Ok(()),
                    Some(Ok(Message::Close(frame))) => {
                        let closed = room_closed(frame);
                        return if closed.code == anila_proto::limits::close::FORBIDDEN {
                            Err(closed.into())
                        } else {
                            Ok(())
                        };
                    }
                    Some(Err(err)) => return Err(err.into()),
                    Some(Ok(_)) => Vec::new(),
                }
            }
            Some(event) = events.recv() => match event {
                PtyEvent::Output { tab, ch, epoch, bytes } => {
                    coalescer.push((tab, ch, epoch), &bytes, Instant::now())
                        .map(|bytes| manager.on_pty_output(tab, ch, epoch, &bytes))
                        .unwrap_or_default()
                },
                PtyEvent::Exit { tab, ch, epoch } => {
                    let mut frames = coalescer.take((tab, ch, epoch))
                        .map(|bytes| manager.on_pty_output(tab, ch, epoch, &bytes))
                        .unwrap_or_default();
                    frames.extend(manager.on_pty_exit(tab, ch, epoch, Instant::now()));
                    frames
                },
            },
            _ = async {
                if let Some(deadline) = deadline {
                    tokio::time::sleep_until(deadline.into()).await;
                }
            }, if deadline.is_some() => {
                coalescer.due(Instant::now()).into_iter()
                    .flat_map(|((tab, ch, epoch), bytes)| manager.on_pty_output(tab, ch, epoch, &bytes))
                    .collect()
            }
            _ = tick.tick() => manager.tick(Instant::now()),
            _ = heartbeat.tick() => {
                if last_heard.elapsed() >= timings.dead_after {
                    return Ok(());
                }
                let beat = Message::text(keepalive(watched));
                tokio::time::timeout(timings.dead_after, socket.send(beat))
                    .await
                    .context("send stalled")??;
                Vec::new()
            }
        };
        // A room that stops reading can stall a send forever; bound it so
        // the dead-link check still runs.
        tokio::time::timeout(timings.dead_after, send_outbound(socket, frames))
            .await
            .context("send stalled")??;
    }
}

fn keepalive(watched: bool) -> String {
    if watched {
        serde_json::to_string(&Hop::Heartbeat).expect("heartbeat is serializable")
    } else {
        PING_TEXT.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keepalive_matches_watch_state() {
        assert_eq!(keepalive(false), PING_TEXT);
        assert_eq!(
            serde_json::from_str::<Hop>(&keepalive(true)).unwrap(),
            Hop::Heartbeat
        );
    }
}
