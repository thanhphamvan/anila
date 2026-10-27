//! A loopback TCP proxy with controllable link faults.

use std::io;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{
    TcpListener, TcpStream,
    tcp::{OwnedReadHalf, OwnedWriteHalf},
};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;

/// How the proxy treats traffic and incoming connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Forward bytes both ways.
    Pass,
    /// Hold traffic in order until forwarding resumes, including new connections.
    Blackhole,
    /// Close new connections immediately; existing connections keep working.
    Refuse,
}

#[derive(Default)]
struct Counts {
    sent: AtomicU64,
    received: AtomicU64,
    connections: AtomicUsize,
}

struct Connection(Arc<Counts>);

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Owns a listener and all its forwarding tasks on the current Tokio runtime.
pub struct Proxy {
    port: u16,
    mode: watch::Sender<Mode>,
    latency: watch::Sender<Duration>,
    bandwidth: watch::Sender<Option<u64>>,
    cut: watch::Sender<u64>,
    counts: Arc<Counts>,
    task: JoinHandle<()>,
}

impl Proxy {
    /// Listens on an ephemeral loopback port and forwards to `upstream`.
    pub async fn start(upstream: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let (mode, mode_rx) = watch::channel(Mode::Pass);
        let (latency, latency_rx) = watch::channel(Duration::ZERO);
        let (bandwidth, bandwidth_rx) = watch::channel(None);
        let (cut, mut cut_rx) = watch::channel(0_u64);
        let counts = Arc::new(Counts::default());
        let task_counts = counts.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = cut_rx.changed() => connections.abort_all(),
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((client, _)) = accepted else { break };
                        if *mode_rx.borrow() == Mode::Refuse {
                            continue;
                        }
                        let mode = mode_rx.clone();
                        let latency = latency_rx.clone();
                        let bandwidth = bandwidth_rx.clone();
                        task_counts.connections.fetch_add(1, Ordering::Relaxed);
                        let guard = Connection(task_counts.clone());
                        connections.spawn(async move {
                            let _guard = guard;
                            if let Ok(server) = TcpStream::connect(upstream).await {
                                let (client_read, client_write) = client.into_split();
                                let (server_read, server_write) = server.into_split();
                                let _ = tokio::try_join!(
                                    forward(client_read, server_write, mode.clone(), latency.clone(), bandwidth.clone(), &_guard.0.sent),
                                    forward(server_read, client_write, mode, latency, bandwidth, &_guard.0.received),
                                );
                            }
                        });
                    }
                }
            }
        });
        Ok(Self {
            port,
            mode,
            latency,
            bandwidth,
            cut,
            counts,
            task,
        })
    }

    /// Returns the local listening port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Changes the fault mode and wakes held connections.
    pub fn set_mode(&self, mode: Mode) {
        self.mode.send_replace(mode);
    }

    /// Sets the propagation delay for chunks arriving in either direction.
    /// Chunks already in flight retain their original delay.
    pub fn set_latency(&self, latency: Duration) {
        self.latency.send_replace(latency);
    }

    /// Sets bytes/second independently for each direction of each connection.
    /// `None` is unlimited; zero holds writes until the setting changes.
    /// Buckets start empty and accumulate at most 16 KiB of burst credit.
    pub fn set_bandwidth(&self, bytes_per_sec: Option<u64>) {
        self.bandwidth.send_replace(bytes_per_sec);
    }

    /// Cancels all current connections while leaving the listener available.
    pub fn cut_all(&self) {
        self.cut
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    /// Returns forwarded byte totals, client-to-upstream then upstream-to-client.
    pub fn bytes(&self) -> (u64, u64) {
        (
            self.counts.sent.load(Ordering::Relaxed),
            self.counts.received.load(Ordering::Relaxed),
        )
    }

    /// Returns the number of currently open forwarding connections.
    pub fn connections(&self) -> usize {
        self.counts.connections.load(Ordering::Relaxed)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Chunk {
    bytes: Vec<u8>,
    arrival: Instant,
    latency: Duration,
}

async fn forward(
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    mode: watch::Receiver<Mode>,
    latency: watch::Receiver<Duration>,
    bandwidth: watch::Receiver<Option<u64>>,
    count: &AtomicU64,
) -> io::Result<()> {
    // At most 256 chunks of 16 KiB are queued per direction (4 MiB).
    let (send, receive) = mpsc::channel(256);
    tokio::try_join!(
        read_chunks(reader, latency, send),
        write_chunks(writer, mode, bandwidth, count, receive),
    )?;
    Ok(())
}

async fn read_chunks(
    mut reader: OwnedReadHalf,
    latency: watch::Receiver<Duration>,
    send: mpsc::Sender<Option<Chunk>>,
) -> io::Result<()> {
    let mut buffer = [0; 16 * 1024];
    loop {
        let Ok(permit) = send.reserve().await else {
            return Ok(());
        };
        let size = reader.read(&mut buffer).await?;
        let arrival = Instant::now();
        let latency = *latency.borrow();
        if size == 0 {
            permit.send(None);
            return Ok(());
        }
        permit.send(Some(Chunk {
            bytes: buffer[..size].to_vec(),
            arrival,
            latency,
        }));
    }
}

async fn write_chunks(
    mut writer: OwnedWriteHalf,
    mut mode: watch::Receiver<Mode>,
    mut bandwidth: watch::Receiver<Option<u64>>,
    count: &AtomicU64,
    mut receive: mpsc::Receiver<Option<Chunk>>,
) -> io::Result<()> {
    let mut bucket = Bucket::default();
    while let Some(chunk) = receive.recv().await {
        let buffer = if let Some(chunk) = chunk {
            tokio::time::sleep_until(chunk.arrival + chunk.latency).await;
            chunk.bytes
        } else {
            Vec::new()
        };
        let size = buffer.len();
        let mut offset = 0;
        loop {
            while *mode.borrow_and_update() == Mode::Blackhole {
                if mode.changed().await.is_err() {
                    return Ok(());
                }
            }
            if size == 0 {
                return writer.shutdown().await;
            }
            if offset == size {
                break;
            }
            let rate = *bandwidth.borrow_and_update();
            let wait = bucket.wait(rate, size - offset);
            if let Some(wait) = wait {
                tokio::select! {
                    changed = bandwidth.changed() => { if changed.is_err() { return Ok(()); } },
                    changed = mode.changed() => { if changed.is_err() { return Ok(()); } },
                    _ = tokio::time::sleep(wait) => {},
                }
                continue;
            }
            tokio::select! {
                biased;
                changed = bandwidth.changed() => { if changed.is_err() { return Ok(()); } },
                changed = mode.changed() => { if changed.is_err() { return Ok(()); } },
                written = writer.write(&buffer[offset..size]) => {
                    let written = written?;
                    if written == 0 { return Err(io::ErrorKind::WriteZero.into()); }
                    bucket.tokens -= written as f64;
                    offset += written;
                    count.fetch_add(written as u64, Ordering::Relaxed);
                }
            }
        }
    }
    Ok(())
}

struct Bucket {
    rate: Option<u64>,
    tokens: f64,
    updated: tokio::time::Instant,
}

impl Default for Bucket {
    fn default() -> Self {
        Self {
            rate: None,
            tokens: 0.0,
            updated: tokio::time::Instant::now(),
        }
    }
}

impl Bucket {
    fn wait(&mut self, rate: Option<u64>, bytes: usize) -> Option<Duration> {
        let now = tokio::time::Instant::now();
        if self.rate != rate {
            self.rate = rate;
            self.tokens = 0.0;
            self.updated = now;
        }
        let rate = rate?;
        self.tokens = (self.tokens + now.duration_since(self.updated).as_secs_f64() * rate as f64)
            .min(16_384.0);
        self.updated = now;
        if rate == 0 {
            return Some(Duration::from_secs(3600));
        }
        let missing = bytes as f64 - self.tokens;
        (missing > 0.0).then(|| Duration::from_secs_f64(missing / rate as f64))
    }
}
