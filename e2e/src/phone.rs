//! Throttled-browser measurements of the production wasm decoder.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anila_proto::{Codec, codec::Encoder};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    bench::{self, Chunk},
    browser::Browser,
    util,
};

/// Ordered base64 codec pieces and their uncompressed lengths.
#[derive(Debug, Serialize, Deserialize)]
pub struct Pieces {
    pub pieces: Vec<String>,
    pub raw_lengths: Vec<usize>,
}

/// Frames at 8 ms / 32 KiB and encodes with one stream for the entire workload.
pub fn encode_pieces(chunks: &[Chunk], codec: Codec) -> Result<Pieces> {
    ensure!(
        matches!(
            codec,
            Codec::None | Codec::DeflateStream | Codec::ZstdStream
        ),
        "unsupported codec"
    );
    let frames = bench::frame(chunks, 8, 32 << 10)?;
    let mut encoder = Encoder::new(codec);
    ensure!(encoder.codec() == codec, "encoder unavailable");
    Ok(Pieces {
        raw_lengths: frames.iter().map(Vec::len).collect(),
        pieces: frames
            .iter()
            .map(|bytes| STANDARD.encode(encoder.encode(bytes)))
            .collect(),
    })
}

/// Nearest-rank percentile, absent for empty samples or invalid inputs.
pub fn percentile(samples: &[f64], percent: f64) -> Option<f64> {
    if samples.is_empty()
        || !percent.is_finite()
        || !(0.0..=100.0).contains(&percent)
        || samples.iter().any(|n| !n.is_finite() || *n < 0.0)
    {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (percent * sorted.len() as f64 / 100.0).ceil() as usize;
    sorted.get(rank.saturating_sub(1)).copied()
}

/// Decode timing summary; empty groups have no percentiles.
#[derive(Debug, Serialize)]
pub struct Statistics {
    pub pieces: usize,
    pub raw_bytes: usize,
    pub small_p50_ms: Option<f64>,
    pub small_p95_ms: Option<f64>,
    pub large_p50_ms: Option<f64>,
    pub large_p95_ms: Option<f64>,
    /// Per-piece timer sums until `bulk` replaces it: the browser's clock
    /// has 0.1 ms steps, so sums of tiny pieces are mostly rounding.
    pub decode_ms_per_mb: Option<f64>,
    /// Mean decode time per piece from whole-run timings.
    pub mean_us_per_piece: Option<f64>,
}

impl Statistics {
    /// Replaces the per-MB figure with one from timing whole runs at once
    /// (`total_ms` for `runs` passes over every piece), which the clock's
    /// 0.1 ms steps cannot distort, and adds the mean time per piece.
    pub fn bulk(&mut self, total_ms: f64, runs: u32) {
        let runs = f64::from(runs.max(1));
        if self.raw_bytes > 0 {
            self.decode_ms_per_mb = Some(total_ms * 1_000_000.0 / (self.raw_bytes as f64 * runs));
        }
        if self.pieces > 0 {
            self.mean_us_per_piece = Some(total_ms * 1000.0 / (self.pieces as f64 * runs));
        }
    }
}

/// Summarizes timings, splitting on raw lengths strictly below 64 bytes.
pub fn statistics(times: &[f64], raw_lengths: &[usize]) -> Result<Statistics> {
    ensure!(
        times.len() == raw_lengths.len(),
        "timing/length count mismatch"
    );
    ensure!(
        times.iter().all(|n| n.is_finite() && *n >= 0.0),
        "invalid decode timing"
    );
    let mut small = Vec::new();
    let mut large = Vec::new();
    for (&time, &length) in times.iter().zip(raw_lengths) {
        if length < 64 {
            small.push(time);
        } else {
            large.push(time);
        }
    }
    let raw_bytes = raw_lengths
        .iter()
        .try_fold(0usize, |sum, n| sum.checked_add(*n))
        .context("raw byte count overflow")?;
    Ok(Statistics {
        pieces: times.len(),
        raw_bytes,
        small_p50_ms: percentile(&small, 50.0),
        small_p95_ms: percentile(&small, 95.0),
        large_p50_ms: percentile(&large, 50.0),
        large_p95_ms: percentile(&large, 95.0),
        decode_ms_per_mb: (raw_bytes > 0)
            .then(|| times.iter().sum::<f64>() * 1_000_000.0 / raw_bytes as f64),
        mean_us_per_piece: None,
    })
}

/// One workload/codec/rate measurement with provenance.
#[derive(Debug, Serialize)]
pub struct Measurement {
    pub rate: f64,
    pub workload: String,
    pub codec: Codec,
    #[serde(flatten)]
    pub stats: Statistics,
    pub git_commit: String,
    pub timestamp: u64,
}

fn cell(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('|', "&#124;")
        .replace(['\r', '\n'], " ")
}

/// Formats all timing units in milliseconds and uses a dash for absent groups.
pub fn markdown(rows: &[Measurement]) -> String {
    let mut table = String::from(
        "| CPU rate | Workload | Codec | Pieces | Raw bytes | <64 B p50 ms | <64 B p95 ms | ≥64 B p50 ms | ≥64 B p95 ms | Decode ms/MB | µs/piece |\n|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for row in rows {
        let s = &row.stats;
        let number = |n: Option<f64>| n.map(|n| format!("{n:.4}")).unwrap_or_else(|| "—".into());
        let codec = serde_json::to_value(row.codec)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        table.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            row.rate,
            cell(&row.workload),
            codec,
            s.pieces,
            s.raw_bytes,
            number(s.small_p50_ms),
            number(s.small_p95_ms),
            number(s.large_p50_ms),
            number(s.large_p95_ms),
            number(s.decode_ms_per_mb),
            number(s.mean_us_per_piece)
        ));
    }
    table
}

struct Server {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(root: PathBuf) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    break;
                };
                let _ = serve(&mut stream, &root);
            }
        });
        Ok(Self {
            address,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(stream: &mut TcpStream, root: &Path) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut line = String::new();
    let mut reader = BufReader::new((&mut *stream).take(8192));
    reader.read_line(&mut line)?;
    loop {
        let mut header = String::new();
        ensure!(
            reader.read_line(&mut header)? > 0,
            "incomplete HTTP headers"
        );
        if header == "\r\n" || header == "\n" {
            break;
        }
    }
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or("");
    let path = words.next().unwrap_or("").split('?').next().unwrap_or("");
    let relative = path.strip_prefix('/').unwrap_or("");
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    let safe = Path::new(relative)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    let (status, bytes) = if method != "GET" {
        ("405 Method Not Allowed", Vec::new())
    } else if !safe {
        ("403 Forbidden", Vec::new())
    } else {
        match fs::read(root.join(relative)) {
            Ok(bytes) => ("200 OK", bytes),
            Err(_) => ("404 Not Found", Vec::new()),
        }
    };
    let mime = match Path::new(relative).extension().and_then(|s| s.to_str()) {
        Some("html") => "text/html",
        Some("js") => "application/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("json" | "webmanifest") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        bytes.len()
    )?;
    stream.write_all(&bytes)?;
    Ok(())
}

fn workloads(directory: &Path) -> Result<Vec<(String, Vec<Chunk>)>> {
    let mut paths = match fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    paths.retain(|p| p.is_file() && p.extension().is_some_and(|e| e == "jsonl"));
    paths.sort();
    if paths.is_empty() {
        return Ok(bench::synthetic(false));
    }
    paths
        .iter()
        .map(|p| {
            Ok((
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .context("invalid workload filename")?
                    .into(),
                bench::read_workload(p)?,
            ))
        })
        .collect()
}

// Escape filename bytes without collisions, retaining ordinary workload names.
fn filename(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                (b as char).to_string()
            } else {
                format!("~{b:02x}")
            }
        })
        .collect()
}

/// Parses phone-cpu options and runs the browser benchmark.
pub fn cli(args: &[String]) -> Result<()> {
    let mut workloads = PathBuf::from("bench/workloads");
    let mut out = PathBuf::from("bench/results");
    let mut rates = vec![4.0, 6.0];
    let (pairs, remainder) = args.as_chunks::<2>();
    ensure!(remainder.is_empty(), "option requires a value");
    for pair in pairs {
        match pair[0].as_str() {
            "--workloads" => workloads = (&pair[1]).into(),
            "--out" => out = (&pair[1]).into(),
            "--rates" => {
                rates = pair[1]
                    .split(',')
                    .map(str::parse)
                    .collect::<std::result::Result<Vec<f64>, _>>()?
            }
            other => bail!("unknown option {other}"),
        }
    }
    ensure!(
        !rates.is_empty() && rates.iter().all(|r| r.is_finite() && *r >= 1.0),
        "rates must be finite and at least 1"
    );
    let runtime = tokio::runtime::Runtime::new()?;
    print!("{}", runtime.block_on(run(&workloads, &out, &rates))?);
    Ok(())
}

async fn run(directory: &Path, out: &Path, rates: &[f64]) -> Result<String> {
    let workloads = workloads(directory)?;
    let dist = util::run_dir("phone-cpu").join("dist");
    let status = Command::new("trunk")
        .current_dir(util::workspace_root().join("frontend"))
        .env("CARGO_NET_OFFLINE", "true")
        .args(["build", "--release", "--features", "bench", "--dist"])
        .arg(&dist)
        .status()
        .context("start trunk")?;
    ensure!(status.success(), "bench frontend build failed");
    let mut inputs = Vec::new();
    for (name, chunks) in workloads {
        for codec in [Codec::None, Codec::DeflateStream, Codec::ZstdStream] {
            let pieces = encode_pieces(&chunks, codec)?;
            let codec_name = serde_json::to_value(codec)?;
            let file = format!(
                "pieces-{}-{}.json",
                filename(&name),
                codec_name.as_str().context("codec name")?
            );
            fs::write(dist.join(&file), serde_json::to_vec(&pieces)?)?;
            inputs.push((name.clone(), codec, file, pieces.raw_lengths));
        }
    }
    let server = Server::start(dist)?;
    let browser = Browser::launch().await?;
    let page = browser.new_page().await?;
    page.goto(&format!("http://{}/", server.address)).await?;
    page.wait_for(
        "typeof window.anilaBenchDecode === 'function'",
        Duration::from_secs(30),
    )
    .await?;
    let commit = bench::git_commit();
    let date = Command::new("date").args(["-u", "+%Y-%m-%d"]).output()?;
    ensure!(date.status.success(), "date command failed");
    let stem = format!(
        "{}-{commit}-phone-cpu",
        String::from_utf8(date.stdout)?.trim()
    );
    fs::create_dir_all(out)?;
    let mut jsonl = File::create(out.join(format!("{stem}.jsonl")))?;
    let mut rows = Vec::new();
    for &rate in rates {
        page.set_cpu_throttling(rate).await?;
        for (name, codec, file, expected) in &inputs {
            let value = page.eval(&format!(r#"(async () => {{
                const response = await fetch({file});
                if (!response.ok) throw new Error('pieces fetch failed');
                const data = await response.json();
                const pieces = data.pieces.map(s => Uint8Array.from(atob(s), c => c.charCodeAt(0)));
                window.anilaBenchReset({codec});
                const times = [];
                for (let i = 0; i < pieces.length; i++) {{
                    const start = performance.now();
                    const length = window.anilaBenchDecode(pieces[i]);
                    const elapsed = performance.now() - start;
                    if (length !== data.raw_lengths[i]) throw new Error('decoded length mismatch at piece ' + i);
                    times.push(elapsed);
                }}
                // Whole runs under one timer, repeated for at least 300 ms,
                // for a per-MB figure the clock's 0.1 ms steps cannot distort.
                let runs = 0, total = 0;
                while (total < 300 || runs < 3) {{
                    window.anilaBenchReset({codec});
                    const start = performance.now();
                    for (const piece of pieces) window.anilaBenchDecode(piece);
                    total += performance.now() - start;
                    runs += 1;
                }}
                return {{times, raw_lengths: data.raw_lengths, total, runs}};
            }})()"#, file = json!(file), codec = serde_json::to_string(codec)?)).await
                .with_context(|| format!("rate {rate}, workload {name}, codec {codec:?}"))?;
            #[derive(Deserialize)]
            struct Timings {
                times: Vec<f64>,
                raw_lengths: Vec<usize>,
                total: f64,
                runs: u32,
            }
            let timings: Timings = serde_json::from_value(value)?;
            ensure!(
                &timings.raw_lengths == expected,
                "browser raw lengths mismatch"
            );
            let row = Measurement {
                rate,
                workload: name.clone(),
                codec: *codec,
                stats: {
                    let mut stats = statistics(&timings.times, expected)?;
                    stats.bulk(timings.total, timings.runs);
                    stats
                },
                git_commit: commit.clone(),
                timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            };
            serde_json::to_writer(&mut jsonl, &row)?;
            writeln!(jsonl)?;
            jsonl.flush()?;
            rows.push(row);
            fs::write(out.join(format!("{stem}.md")), markdown(&rows))?;
        }
    }
    Ok(markdown(&rows))
}
