use std::{net::SocketAddr, time::Duration};

use anila_e2e::proxy::{Mode, Proxy};
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

async fn connect(proxy: &Proxy) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", proxy.port()))
        .await
        .unwrap();
    stream.set_nodelay(true).unwrap();
    stream
}

async fn receive(stream: &mut TcpStream, expected: &[u8], limit: Duration) {
    let mut received = vec![0; expected.len()];
    timeout(limit, stream.read_exact(&mut received))
        .await
        .expect("echo timed out")
        .unwrap();
    assert_eq!(received, expected);
}

#[tokio::test]
async fn back_to_back_messages_share_propagation_delay() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    proxy.set_latency(Duration::from_millis(200));
    let mut stream = connect(&proxy).await;
    let start = Instant::now();
    timeout(Duration::from_millis(600), async {
        for byte in 0..10 {
            stream.write_all(&[byte]).await.unwrap();
            stream.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
        receive(
            &mut stream,
            &(0..10).collect::<Vec<_>>(),
            Duration::from_secs(1),
        )
        .await;
    })
    .await
    .expect("ten echoes exceeded 400 ms round trip plus 200 ms slack");
    assert!(start.elapsed() >= Duration::from_millis(400));
}

#[tokio::test]
async fn burst_spanning_many_read_buffers_has_one_propagation_delay() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    proxy.set_latency(Duration::from_millis(200));
    let mut stream = connect(&proxy).await;
    let payload = vec![42; 256 * 1024];
    timeout(Duration::from_millis(600), async {
        stream.write_all(&payload).await.unwrap();
        receive(&mut stream, &payload, Duration::from_secs(1)).await;
    })
    .await
    .expect("burst accumulated per-chunk latency");
}

#[tokio::test]
async fn interleaved_writes_preserve_order_and_eof() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    proxy.set_latency(Duration::from_millis(50));
    let mut stream = connect(&proxy).await;
    let payload: Vec<u8> = (0..200_u32).flat_map(u32::to_be_bytes).collect();
    timeout(Duration::from_secs(3), async {
        let (mut read, mut write) = stream.split();
        let sending = async {
            for message in payload.chunks(4) {
                write.write_all(message).await.unwrap();
                write.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            write.shutdown().await.unwrap();
        };
        let receiving = async {
            let mut received = Vec::new();
            read.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, payload);
        };
        tokio::join!(sending, receiving);
    })
    .await
    .expect("interleaved echo timed out");
}

#[tokio::test]
async fn single_round_trip_has_two_propagation_delays() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    proxy.set_latency(Duration::from_millis(200));
    let mut stream = connect(&proxy).await;
    let start = Instant::now();
    stream.write_all(b"delayed").await.unwrap();
    receive(&mut stream, b"delayed", Duration::from_secs(2)).await;
    assert!(start.elapsed() >= Duration::from_millis(400));
}

#[tokio::test]
async fn latency_and_bandwidth_are_pipelined() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    proxy.set_latency(Duration::from_millis(100));
    proxy.set_bandwidth(Some(100_000));
    let mut stream = connect(&proxy).await;
    let payload = vec![42; 200_000];
    let start = Instant::now();
    timeout(Duration::from_secs(4), async {
        let (mut read, mut write) = stream.split();
        let mut received = vec![0; payload.len()];
        let (sent, got) = tokio::join!(write.write_all(&payload), read.read_exact(&mut received));
        sent.unwrap();
        got.unwrap();
        assert_eq!(received, payload);
    })
    .await
    .expect("shaped echo exceeded four seconds");
    assert!(start.elapsed() >= Duration::from_millis(1900));
}

#[tokio::test]
async fn blackhole_holds_in_flight_chunks_and_releases_in_order() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    proxy.set_latency(Duration::from_millis(200));
    let mut stream = connect(&proxy).await;
    let payload: Vec<u8> = (0..64 * 1024).map(|n| (n % 251) as u8).collect();
    stream.write_all(&payload[..32 * 1024]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    proxy.set_mode(Mode::Blackhole);
    stream.write_all(&payload[32 * 1024..]).await.unwrap();
    let mut byte = [0];
    assert!(
        timeout(Duration::from_millis(300), stream.read(&mut byte))
            .await
            .is_err()
    );
    assert_eq!(proxy.bytes(), (0, 0));
    proxy.set_mode(Mode::Pass);
    receive(&mut stream, &payload, Duration::from_secs(1)).await;
}

#[tokio::test]
async fn latency_changes_do_not_retime_queued_chunks() {
    let server = Echo::start().await;
    let proxy = Proxy::start(server.address).await.unwrap();
    proxy.set_mode(Mode::Blackhole);
    proxy.set_latency(Duration::from_millis(200));
    let mut stream = connect(&proxy).await;
    stream.write_all(b"first").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    proxy.set_latency(Duration::from_millis(800));
    stream.write_all(b"second").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    proxy.set_latency(Duration::ZERO);
    proxy.set_mode(Mode::Pass);
    receive(&mut stream, b"first", Duration::from_millis(300)).await;
    let start = Instant::now();
    receive(&mut stream, b"second", Duration::from_secs(2)).await;
    assert!(
        start.elapsed() >= Duration::from_millis(400),
        "queued chunk lost its arrival latency"
    );
}
