use std::{net::SocketAddr, time::Duration};

use anila_e2e::proxy::Proxy;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::{Instant, timeout},
};

struct Echo {
    address: SocketAddr,
    tasks: JoinSet<()>,
}

impl Echo {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind echo listener");
        let address = listener.local_addr().unwrap();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        connections.spawn(async move {
                            let mut buffer = [0; 16 * 1024];
                            loop {
                                let n = match stream.read(&mut buffer).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => n,
                                };
                                if stream.write_all(&buffer[..n]).await.is_err() { break; }
                            }
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                }
            }
        });
        Self { address, tasks }
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.tasks.abort_all();
    }
}

async fn echo(stream: &mut TcpStream) -> Duration {
    let payload = vec![42; 200_000];
    let mut received = vec![0; payload.len()];
    let start = Instant::now();
    timeout(Duration::from_secs(4), async {
        let (mut read, mut write) = stream.split();
        let (sent, got) = tokio::join!(write.write_all(&payload), read.read_exact(&mut received));
        sent.unwrap();
        got.unwrap();
    })
    .await
    .expect("200 KB echo exceeded four seconds");
    assert_eq!(received, payload);
    start.elapsed()
}

#[tokio::test]
async fn bandwidth_is_per_connection_and_proxy_and_can_be_disabled() {
    let server = Echo::start().await;
    let shaped = Proxy::start(server.address).await.unwrap();
    let unlimited = Proxy::start(server.address).await.unwrap();
    let mut first = TcpStream::connect(("127.0.0.1", shaped.port()))
        .await
        .unwrap();
    let mut second = TcpStream::connect(("127.0.0.1", shaped.port()))
        .await
        .unwrap();
    let mut other = TcpStream::connect(("127.0.0.1", unlimited.port()))
        .await
        .unwrap();
    // Also covers changing the rate on existing connections.
    shaped.set_bandwidth(Some(100_000));
    let (first_time, second_time, other_time) =
        tokio::join!(echo(&mut first), echo(&mut second), echo(&mut other));
    for elapsed in [first_time, second_time] {
        assert!(
            elapsed >= Duration::from_millis(1800),
            "too fast: {elapsed:?}"
        );
        assert!(elapsed <= Duration::from_secs(4), "too slow: {elapsed:?}");
    }
    assert!(
        other_time < Duration::from_secs(1),
        "other proxy was shaped: {other_time:?}"
    );
    shaped.set_bandwidth(None);
    let elapsed = echo(&mut first).await;
    assert!(
        elapsed < Duration::from_secs(1),
        "unlimited remained slow: {elapsed:?}"
    );
}

#[tokio::test]
async fn disabling_bandwidth_wakes_a_blocked_write() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    proxy.set_bandwidth(Some(0));
    let mut stream = TcpStream::connect(("127.0.0.1", proxy.port()))
        .await
        .unwrap();
    stream.write_all(b"held").await.unwrap();
    let mut received = [0; 4];
    assert!(
        timeout(Duration::from_millis(100), stream.read_exact(&mut received))
            .await
            .is_err()
    );
    assert_eq!(proxy.bytes(), (0, 0));
    proxy.set_bandwidth(None);
    timeout(Duration::from_secs(1), stream.read_exact(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&received, b"held");
}
