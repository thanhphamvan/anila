use std::net::SocketAddr;
use std::process::Command;
use std::time::Duration;

use anila_e2e::{
    proxy::{Mode, Proxy},
    tmux::TmuxServer,
    util,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, timeout};

struct Echo {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl Echo {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind echo listener");
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        tasks.spawn(async move {
                            let mut buffer = [0; 16 * 1024];
                            loop {
                                match socket.read(&mut buffer).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => if socket.write_all(&buffer[..n]).await.is_err() { break; },
                                }
                            }
                        });
                    }
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
                }
            }
        });
        Self { address, task }
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn connect(proxy: &Proxy) -> TcpStream {
    timeout(
        Duration::from_secs(1),
        TcpStream::connect(("127.0.0.1", proxy.port())),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn echo(client: &mut TcpStream, bytes: &[u8]) {
    timeout(Duration::from_secs(2), async {
        client.write_all(bytes).await.unwrap();
        let mut received = vec![0; bytes.len()];
        client.read_exact(&mut received).await.unwrap();
        assert_eq!(received, bytes);
    })
    .await
    .expect("echo timed out");
}

async fn closed(client: &mut TcpStream) {
    let mut byte = [0];
    let result = timeout(Duration::from_secs(1), client.read(&mut byte))
        .await
        .expect("connection stayed open");
    assert!(
        matches!(result, Ok(0) | Err(_)),
        "unexpected data: {result:?}"
    );
}

#[tokio::test]
async fn pass_echoes_and_counts_bytes() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    let mut client = connect(&proxy).await;
    echo(&mut client, b"hello proxy").await;
    assert_eq!(proxy.bytes(), (11, 11));
    assert_eq!(proxy.connections(), 1);
}

#[tokio::test]
async fn blackhole_holds_existing_and_new_connections_in_order() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    let mut existing = connect(&proxy).await;
    echo(&mut existing, b"ready").await;
    proxy.set_mode(Mode::Blackhole);
    let mut new = connect(&proxy).await;
    let payload: Vec<u8> = (0..48 * 1024).map(|n| (n % 251) as u8).collect();
    existing.write_all(&payload[..20_000]).await.unwrap();
    existing.write_all(&payload[20_000..]).await.unwrap();
    new.write_all(b"held new connection").await.unwrap();
    let mut byte = [0];
    assert!(
        timeout(Duration::from_millis(300), existing.read(&mut byte))
            .await
            .is_err()
    );
    assert!(
        timeout(Duration::from_millis(300), new.read(&mut byte))
            .await
            .is_err()
    );
    assert_eq!(proxy.connections(), 2);
    assert_eq!(proxy.bytes(), (5, 5));
    proxy.set_mode(Mode::Pass);
    let mut received = vec![0; payload.len()];
    timeout(Duration::from_secs(2), existing.read_exact(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received, payload);
    let mut received = [0; 19];
    timeout(Duration::from_secs(1), new.read_exact(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&received, b"held new connection");
}

#[tokio::test]
async fn refuse_preserves_existing_connections() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    let mut existing = connect(&proxy).await;
    echo(&mut existing, b"before").await;
    proxy.set_mode(Mode::Refuse);
    if let Ok(mut new) = timeout(
        Duration::from_secs(1),
        TcpStream::connect(("127.0.0.1", proxy.port())),
    )
    .await
    .unwrap()
    {
        closed(&mut new).await;
    }
    echo(&mut existing, b"after").await;
}

#[tokio::test]
async fn cut_all_closes_connections_and_allows_reconnect() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    let mut first = connect(&proxy).await;
    let mut second = connect(&proxy).await;
    echo(&mut first, b"first").await;
    echo(&mut second, b"second").await;
    proxy.set_mode(Mode::Blackhole);
    proxy.cut_all();
    closed(&mut first).await;
    closed(&mut second).await;
    assert_eq!(proxy.connections(), 0);
    proxy.set_mode(Mode::Pass);
    echo(&mut connect(&proxy).await, b"new").await;
}

#[tokio::test]
async fn latency_applies_in_both_directions() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    proxy.set_latency(Duration::from_millis(200));
    let mut client = connect(&proxy).await;
    let start = Instant::now();
    echo(&mut client, b"delayed").await;
    assert!(start.elapsed() >= Duration::from_millis(400));
}

#[test]
fn tmux_server_is_private_and_stops_on_drop() {
    let server = TmuxServer::new("anila-harness");
    let socket = server.socket().to_owned();
    server.new_session("echo", "cat");
    assert!(
        server.cmd(&["ls"]).contains("echo:"),
        "tmux session was not created"
    );
    drop(server);
    assert!(
        util::wait_until(Duration::from_secs(1), || {
            let output = Command::new("tmux")
                .args(["-L", &socket, "ls"])
                .output()
                .expect("run tmux after drop");
            !output.status.success() && output.stdout.is_empty()
        }),
        "tmux server survived drop"
    );
}

#[test]
fn run_directories_are_unique_and_exist() {
    let first = util::run_dir("harness-test");
    let second = util::run_dir("harness-test");
    assert_ne!(first, second);
    assert!(first.is_dir());
    assert!(second.is_dir());
    std::fs::remove_dir(first).unwrap();
    std::fs::remove_dir(second).unwrap();
}
