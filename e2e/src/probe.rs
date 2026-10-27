//! A browser tab driven by the shared protocol state machine.

use anila_proto::{
    Create, Preset, SessionInfo, SessionName, ToBrowser,
    client::{Client, ClientConfig, CloseAction, Event, Frame, backoff_ms},
};
use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

/// Connection settings for a browser stand-in.
pub struct ProbeOpts {
    /// Websocket endpoint.
    pub url: String,
    /// Browser Origin header.
    pub origin: String,
    /// Value of the `__Host-term` cookie.
    pub cookie: Option<String>,
    /// Protocol tab id.
    pub tab: u32,
    /// Protocol timers and acknowledgement thresholds.
    pub config: ClientConfig,
    /// Multiplier for reconnect delays (1.0 uses real timings).
    pub backoff_scale: f64,
}

/// Observations accumulated across all connections.
#[derive(Debug, Clone, Default)]
pub struct ProbeStats {
    /// Sockets successfully opened.
    pub connects: u32,
    /// Welcome events received.
    pub welcomes: u32,
    /// Resumed frames received for this tab.
    pub resumes: u32,
    /// Reset events received.
    pub resets: u32,
    /// Dead links detected by the Client.
    pub link_dead: u32,
    /// Close codes causing a permanent stop.
    pub stops: Vec<u16>,
    /// All observed close codes, including synthetic 1006 on transport failure.
    pub closes: Vec<u16>,
}

#[derive(Default)]
struct State {
    screens: BTreeMap<u16, Vec<u8>>,
    stats: ProbeStats,
    connected: bool,
    host_online: Option<bool>,
}

type Shared = Arc<Mutex<State>>;
type Operation = Box<dyn FnOnce(&mut Client) -> Vec<Frame> + Send>;
enum Command {
    Run(Operation),
    Drop(oneshot::Sender<()>),
}

fn lock(state: &Shared) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

/// A background websocket task with observable screens and protocol events.
pub struct Probe {
    state: Shared,
    events: broadcast::Sender<Event>,
    commands: mpsc::Sender<Command>,
    task: JoinHandle<()>,
}

impl Probe {
    /// Start a tab and wait until its first connection attempt finishes.
    pub async fn start(opts: ProbeOpts) -> Self {
        let state = Shared::default();
        let (events, _) = broadcast::channel(1024);
        let (commands, rx) = mpsc::channel(64);
        let (ready, started) = oneshot::channel();
        let task = tokio::spawn(run(opts, state.clone(), events.clone(), rx, ready));
        let _ = started.await;
        Self {
            state,
            events,
            commands,
            task,
        }
    }

    /// True after a welcome and until the socket closes.
    pub fn is_connected(&self) -> bool {
        lock(&self.state).connected
    }
    /// Host availability from the latest welcome or status.
    pub fn host_online(&self) -> Option<bool> {
        lock(&self.state).host_online
    }
    /// A copy of all observations so far.
    pub fn stats(&self) -> ProbeStats {
        lock(&self.state).stats.clone()
    }
    /// Bytes rendered since the channel's last open or reset.
    pub fn screen(&self, ch: u16) -> Vec<u8> {
        lock(&self.state)
            .screens
            .get(&ch)
            .cloned()
            .unwrap_or_default()
    }

    async fn command<T: Send + 'static>(
        &self,
        op: impl FnOnce(&mut Client) -> (T, Vec<Frame>) + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Run(Box::new(move |client| {
                let (result, frames) = op(client);
                let _ = tx.send(result);
                frames
            })))
            .await
            .map_err(|_| anyhow!("probe stopped"))?;
        rx.await.map_err(|_| anyhow!("probe stopped"))
    }

    /// Request the host's sessions and presets.
    pub async fn list(&self, timeout: Duration) -> Result<(Vec<SessionInfo>, Vec<Preset>)> {
        let mut events = self.events.subscribe();
        tokio::time::timeout(timeout, async {
            let req = self
                .command(|client| {
                    let (req, frame) = client.list();
                    (frame.as_ref().map(|_| req), frame.into_iter().collect())
                })
                .await?
                .ok_or_else(|| anyhow!("probe disconnected"))?;
            loop {
                match events.recv().await? {
                    Event::Sessions {
                        req: id,
                        items,
                        presets,
                    } if id == req => return Ok((items, presets)),
                    Event::Error {
                        req: Some(id), msg, ..
                    } if id == req => return Err(anyhow!(msg)),
                    _ => {}
                }
            }
        })
        .await?
    }

    /// Open a terminal and wait for Opened or a matching Error.
    pub async fn open(
        &self,
        session: &str,
        create: Option<&str>,
        cols: u16,
        rows: u16,
        timeout: Duration,
    ) -> Result<u16> {
        let session = SessionName::new(session)?;
        let create = create.map(|preset| Create {
            preset: preset.into(),
        });
        let mut events = self.events.subscribe();
        tokio::time::timeout(timeout, async {
            let (ch, req) = self
                .command(move |client| {
                    let (ch, req, frame) = client.open(session, create, cols, rows);
                    ((ch, req), frame.into_iter().collect())
                })
                .await?;
            loop {
                match events.recv().await? {
                    Event::Opened {
                        ch: id,
                        req: request,
                    } if id == ch && request == req => return Ok(ch),
                    Event::Error {
                        ch: id,
                        req: request,
                        msg,
                        ..
                    } if request == Some(req) || (request.is_none() && id == Some(ch)) => {
                        return Err(anyhow!(msg));
                    }
                    _ => {}
                }
            }
        })
        .await?
    }

    /// Send keystrokes; false means the Client paused input or the task stopped.
    pub async fn type_str(&self, ch: u16, s: &str) -> bool {
        let bytes = s.as_bytes().to_vec();
        self.command(move |client| {
            let frames = client.input(ch, &bytes);
            (!frames.is_empty(), frames)
        })
        .await
        .unwrap_or(false)
    }
    /// Update the terminal dimensions.
    pub async fn resize(&self, ch: u16, cols: u16, rows: u16) {
        let _ = self
            .command(move |client| ((), client.resize(ch, cols, rows).into_iter().collect()))
            .await;
    }
    /// Detach a terminal channel.
    pub async fn close_channel(&self, ch: u16) {
        let _ = self
            .command(move |client| ((), client.close_channel(ch).into_iter().collect()))
            .await;
    }
    /// Wait for the rendered screen to contain `needle`.
    pub async fn wait_output(&self, ch: u16, needle: &str, timeout: Duration) -> Result<()> {
        let mut events = self.events.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                if needle.is_empty()
                    || self
                        .screen(ch)
                        .windows(needle.len())
                        .any(|bytes| bytes == needle.as_bytes())
                {
                    return Ok(());
                }
                match events.recv().await {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        })
        .await?
    }
    /// Wait for a matching event received from now on.
    pub async fn wait_event(
        &self,
        timeout: Duration,
        pred: impl Fn(&Event) -> bool,
    ) -> Result<Event> {
        let mut events = self.events.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                let event = events.recv().await?;
                if pred(&event) {
                    return Ok(event);
                }
            }
        })
        .await?
    }
    /// Drop the active socket and allow the normal reconnect policy to run.
    pub async fn drop_link(&self) {
        let (tx, rx) = oneshot::channel();
        if self.commands.send(Command::Drop(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
    /// Stop the task, dropping its socket even during a pending connection attempt.
    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
        lock(&self.state).connected = false;
    }
}

fn publish(state: &Shared, sender: &broadcast::Sender<Event>, events: Vec<Event>) -> bool {
    let mut dead = false;
    for event in events {
        {
            let mut state = lock(state);
            match &event {
                Event::Connected { agent, .. } => {
                    state.connected = true;
                    state.host_online = Some(agent.online);
                    state.stats.welcomes += 1;
                }
                Event::Status(agent) => state.host_online = Some(agent.online),
                Event::Opened { ch, .. } => {
                    state.screens.insert(*ch, Vec::new());
                }
                Event::Reset { ch } => {
                    state.screens.insert(*ch, Vec::new());
                    state.stats.resets += 1;
                }
                Event::Output { ch, bytes } => state
                    .screens
                    .entry(*ch)
                    .or_default()
                    .extend_from_slice(bytes),
                Event::LinkDead => {
                    state.stats.link_dead += 1;
                    dead = true;
                }
                _ => {}
            }
        }
        let _ = sender.send(event);
    }
    dead
}

async fn run(
    opts: ProbeOpts,
    state: Shared,
    events: broadcast::Sender<Event>,
    mut commands: mpsc::Receiver<Command>,
    ready: oneshot::Sender<()>,
) {
    let mut client = Client::new(opts.tab, opts.config);
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let mut ready = Some(ready);
    let mut attempt = 0;
    let scale = if opts.backoff_scale.is_finite() && opts.backoff_scale >= 0.0 {
        opts.backoff_scale
    } else {
        1.0
    };
    loop {
        let connection = async {
            let mut request = opts.url.as_str().into_client_request()?;
            request.headers_mut().insert("Origin", opts.origin.parse()?);
            if let Some(cookie) = &opts.cookie {
                request
                    .headers_mut()
                    .insert("Cookie", format!("__Host-term={cookie}").parse()?);
            }
            Ok::<_, anyhow::Error>(connect_async(request).await?.0)
        };
        tokio::pin!(connection);
        let connection = loop {
            tokio::select! {
                result = &mut connection => break result,
                command = commands.recv() => match command {
                    Some(Command::Run(op)) => { op(&mut client); }
                    Some(Command::Drop(reply)) => { let _ = reply.send(()); }
                    None => return,
                }
            }
        };
        if connection.is_ok() {
            lock(&state).stats.connects += 1;
        }
        if let Some(ready) = ready.take() {
            let _ = ready.send(());
        }
        let mut drop_reply = None;
        let code = if let Ok(mut socket) = connection {
            let mut pending = client.on_open(now());
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            'socket: loop {
                for frame in pending.drain(..) {
                    let message = match frame {
                        Frame::Text(text) => Message::Text(text.into()),
                        Frame::Binary(bytes) => Message::Binary(bytes.into()),
                    };
                    if socket.send(message).await.is_err() {
                        break 'socket 1006;
                    }
                }
                let (frames, emitted) = tokio::select! {
                    command = commands.recv() => match command {
                        Some(Command::Run(op)) => (op(&mut client), Vec::new()),
                        Some(Command::Drop(reply)) => { drop_reply = Some(reply); break 1006; }
                        None => return,
                    },
                    _ = tick.tick() => client.tick(now()),
                    message = socket.next() => match message {
                        Some(Ok(Message::Text(text))) => {
                            if matches!(serde_json::from_str::<ToBrowser>(&text), Ok(ToBrowser::Resumed { tab, .. }) if tab == opts.tab) { lock(&state).stats.resumes += 1; }
                            client.on_text(&text, now())
                        }
                        Some(Ok(Message::Binary(bytes))) => client.on_binary(&bytes, now()),
                        Some(Ok(Message::Close(frame))) => break frame.map_or(1006, |frame| u16::from(frame.code)),
                        Some(Ok(Message::Ping(_))) => {
                            if socket.flush().await.is_err() { break 1006; }
                            (Vec::new(), Vec::new())
                        }
                        Some(Ok(_)) => (Vec::new(), Vec::new()),
                        Some(Err(_)) | None => break 1006,
                    }
                };
                if emitted
                    .iter()
                    .any(|event| matches!(event, Event::Connected { .. }))
                {
                    attempt = 0;
                }
                if publish(&state, &events, emitted) {
                    break 1006;
                }
                pending = frames;
            }
        } else {
            1006
        };
        let action = client.on_close(code);
        {
            let mut state = lock(&state);
            state.connected = false;
            state.stats.closes.push(code);
            if let CloseAction::Stop { code, .. } = action {
                state.stats.stops.push(code);
            }
        }
        if let Some(reply) = drop_reply {
            let _ = reply.send(());
        }
        if matches!(action, CloseAction::Stop { .. }) {
            return;
        }
        let delay = Duration::from_millis((backoff_ms(attempt, 0.5) as f64 * scale) as u64);
        attempt = attempt.saturating_add(1);
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                command = commands.recv() => match command {
                    Some(Command::Run(op)) => { op(&mut client); }
                    Some(Command::Drop(reply)) => { let _ = reply.send(()); }
                    None => return,
                }
            }
        }
    }
}
