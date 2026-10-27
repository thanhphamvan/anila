//! Recorded terminal workloads, codec comparisons, and real-stack network measurements.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use miniz_oxide::{
    DataFormat, MZFlush,
    deflate::core::{CompressorOxide, TDEFLFlush, TDEFLStatus, compress},
    inflate::stream::{InflateState, inflate},
};
use serde::{Deserialize, Serialize};
use zstd::stream::raw::{Decoder, Encoder, Operation, OutBuffer};

/// One PTY read, timestamped in milliseconds since recording started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub t_ms: u64,
    pub bytes: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Line {
    t_ms: u64,
    b64: String,
}

/// Writes a workload as base64 JSON lines.
pub fn write_workload(path: &Path, chunks: &[Chunk]) -> Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    for chunk in chunks {
        write_chunk(&mut file, chunk)?;
    }
    file.flush()?;
    Ok(())
}

fn write_chunk(writer: &mut impl Write, chunk: &Chunk) -> Result<()> {
    serde_json::to_writer(
        &mut *writer,
        &Line {
            t_ms: chunk.t_ms,
            b64: STANDARD.encode(&chunk.bytes),
        },
    )?;
    writeln!(writer)?;
    Ok(())
}

/// Loads a workload, rejecting invalid base64 and decreasing timestamps.
pub fn read_workload(path: &Path) -> Result<Vec<Chunk>> {
    let mut chunks = Vec::new();
    for (index, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line: Line = serde_json::from_str(&line?)
            .with_context(|| format!("{} line {}", path.display(), index + 1))?;
        chunks.push(Chunk {
            t_ms: line.t_ms,
            bytes: STANDARD.decode(line.b64)?,
        });
    }
    validate(&chunks)?;
    Ok(chunks)
}

fn validate(chunks: &[Chunk]) -> Result<()> {
    ensure!(
        chunks.windows(2).all(|w| w[0].t_ms <= w[1].t_ms),
        "timestamps must be ordered"
    );
    Ok(())
}

/// Simulates a deadline from the first byte, with an independent frame size cap.
pub fn frame(chunks: &[Chunk], batch_ms: u64, max_frame: usize) -> Result<Vec<Vec<u8>>> {
    ensure!(
        max_frame > 0 && max_frame <= anila_proto::limits::MAX_OUT_PAYLOAD,
        "invalid frame cap"
    );
    validate(chunks)?;
    let mut frames = Vec::new();
    let mut pending = Vec::new();
    let mut first = 0;
    for chunk in chunks {
        if !pending.is_empty() && chunk.t_ms - first >= batch_ms {
            frames.push(std::mem::take(&mut pending));
        }
        let mut bytes = chunk.bytes.as_slice();
        while !bytes.is_empty() {
            if pending.is_empty() {
                first = chunk.t_ms;
            }
            let count = bytes.len().min(max_frame - pending.len());
            pending.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if pending.len() == max_frame {
                frames.push(std::mem::take(&mut pending));
            }
        }
        if batch_ms == 0 && !pending.is_empty() {
            frames.push(std::mem::take(&mut pending));
        }
    }
    if !pending.is_empty() {
        frames.push(pending);
    }
    Ok(frames)
}

/// Deterministic synthetic workloads; `small` selects shortened test fixtures.
pub fn synthetic(small: bool) -> Vec<(String, Vec<Chunk>)> {
    let ticker = (0..if small { 12 } else { 600 })
        .map(|n| Chunk {
            t_ms: n * 100,
            bytes: format!("tick {n}\r\n").into_bytes(),
        })
        .collect();
    let build = (0..if small { 100 } else { 3000 })
        .map(|n| {
            let line = if n % 31 == 30 {
                format!("\x1b[33mwarning:\x1b[0m unused variable in crate-{n}/src/lib.rs:12\r\n")
            } else {
                format!(
                    "\x1b[1m\x1b[32m   Compiling\x1b[0m crate-{n} v0.{n}.0 (/path/to/crate-{n})\r\n"
                )
            };
            Chunk {
                t_ms: n / 20 * 50,
                bytes: line.into_bytes(),
            }
        })
        .collect();
    let tui = (0..if small { 24 } else { 300 })
        .map(|n| {
            let mut screen = String::from("\x1b[H");
            let status = format!(
                "anila | elapsed {:02}:{:02} | {} working",
                n / 1200,
                n / 20 % 60,
                ['|', '/', '-', '\\'][n as usize % 4]
            );
            screen.push_str(&format!("\x1b[7m{status:<80}\x1b[0m\r\n"));
            for row in 0..22 {
                let text = format!(
                    "  {:02}  {}",
                    row + 1,
                    if row == 21 {
                        format!("Streaming response: {}", ".".repeat((n / 10 % 50) as usize))
                    } else {
                        "Terminal session output remains available while the assistant works."
                            .into()
                    }
                );
                screen.push_str(&format!("{text:<80}\r\n"));
            }
            screen.push_str(&format!("{:<80}", " > Ask a question"));
            Chunk {
                t_ms: n * 50,
                bytes: screen.into_bytes(),
            }
        })
        .collect();
    let mut seed = 0x9e3779b97f4a7c15_u64;
    let random = (0..if small { 8 } else { 128 })
        .map(|n| {
            let bytes = (0..4096)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    (seed >> 32) as u8
                })
                .collect();
            Chunk {
                t_ms: n * 10,
                bytes,
            }
        })
        .collect();
    vec![
        ("synth-ticker".into(), ticker),
        ("synth-build".into(), build),
        ("synth-tui".into(), tui),
        ("synth-random".into(), random),
    ]
}

/// Captures PTY output until exit, streaming JSONL to `path` and echoing to stdout.
/// Input is forwarded from stdin when `interactive` is true (line-buffered).
pub fn record(
    path: &Path,
    program: &str,
    args: &[String],
    cols: u16,
    rows: u16,
    interactive: bool,
) -> Result<(usize, Duration)> {
    use anila_service::{
        channel::Pty,
        pty::{PtyEvent, spawn},
    };
    ensure!(cols > 0 && rows > 0, "terminal dimensions must be positive");
    let mut writer = BufWriter::new(File::create(path)?);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let start = Instant::now();
    let process = std::sync::Arc::new(std::sync::Mutex::new(spawn(
        program,
        args,
        cols,
        rows,
        (1, 1, 1),
        tx,
    )?));
    if interactive {
        let weak = std::sync::Arc::downgrade(&process);
        std::thread::spawn(move || {
            use std::io::Read;
            let mut input = std::io::stdin().lock();
            let mut buffer = [0; 4096];
            while let Ok(n) = input.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                let Some(process) = weak.upgrade() else {
                    break;
                };
                let Ok(mut process) = process.lock() else {
                    break;
                };
                if process.write(&buffer[..n]).is_err() {
                    break;
                }
            }
        });
    }
    let mut count = 0;
    while let PtyEvent::Output { bytes, .. } = rx
        .blocking_recv()
        .context("PTY event channel closed before exit")?
    {
        count += bytes.len();
        let chunk = Chunk {
            t_ms: start.elapsed().as_millis().try_into()?,
            bytes,
        };
        write_chunk(&mut writer, &chunk)?;
        if interactive {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&chunk.bytes)?;
            stdout.flush()?;
        }
    }
    writer.flush()?;
    Ok((count, start.elapsed()))
}

/// The codecs compared by the benchmark.
pub const CODECS: [&str; 6] = [
    "none",
    "lz4",
    "zstd3",
    "zstd3_stream",
    "deflate_stream",
    "zstd3_prefix",
];

const ZSTD_TEMPLATE: &[u8] = include_bytes!("../../proto/src/zstd_template.dict");

fn append_history(history: &mut Vec<u8>, bytes: &[u8]) {
    history.extend_from_slice(bytes);
    let excess = history.len().saturating_sub(32 << 10);
    history.drain(..excess);
}

fn zstd_result(result: zstd::zstd_safe::SafeResult) -> Result<usize> {
    result.map_err(|code| anyhow::anyhow!("zstd: {}", zstd::zstd_safe::get_error_name(code)))
}

fn prefix_encode(input: &[u8], history: &[u8]) -> Result<Vec<u8>> {
    use zstd::zstd_safe::{CCtx, CParameter, compress_bound};
    let mut encoder = CCtx::default();
    zstd_result(encoder.set_parameter(CParameter::CompressionLevel(3)))?;
    zstd_result(encoder.set_parameter(CParameter::ChecksumFlag(false)))?;
    zstd_result(encoder.set_parameter(CParameter::DictIdFlag(false)))?;
    zstd_result(encoder.ref_prefix(history))?;
    let mut output = Vec::with_capacity(compress_bound(input.len()));
    // compress2 finishes a complete frame, equivalent to ZSTD_e_end.
    zstd_result(encoder.compress2(&mut output, input))?;
    Ok(output)
}

fn prefix_decode(payload: &[u8], history: &[u8]) -> Result<Vec<u8>> {
    use ruzstd::decoding::{BlockDecodingStrategy, Dictionary, FrameDecoder};
    let mut dictionary = Dictionary::decode_dict(ZSTD_TEMPLATE)?;
    dictionary.dict_content = history.to_vec();
    dictionary.offset_hist = [1, 4, 8];
    let id = dictionary.id;
    let mut decoder = FrameDecoder::new();
    decoder.add_dict(dictionary)?;
    let mut source = payload;
    decoder.init(&mut source)?;
    decoder.force_dict(id)?;
    decoder.decode_blocks(&mut source, BlockDecodingStrategy::All)?;
    ensure!(
        decoder.is_finished() && source.is_empty(),
        "incomplete prefix frame"
    );
    decoder.collect().context("prefix output held back")
}

/// One measured workload/framing/codec combination. MB means 1,000,000 bytes.
#[derive(Debug, Serialize, Deserialize)]
pub struct Measurement {
    pub workload: String,
    pub codec: String,
    pub batch_ms: u64,
    pub max_frame: usize,
    pub raw_bytes: usize,
    pub wire_bytes: usize,
    pub ratio: f64,
    pub frames: usize,
    pub encode_us_per_mb: f64,
    pub decode_us_per_mb: f64,
    pub decode_ruzstd_us_per_mb: Option<f64>,
    pub git_commit: String,
    /// Unix seconds, UTC.
    pub timestamp: u64,
}

fn zstd_encode(encoder: &mut Encoder<'_>, bytes: &[u8], last: bool) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    let mut consumed = 0;
    let mut buffer = vec![0; 65536];
    while consumed < bytes.len() {
        let status = encoder.run_on_buffers(&bytes[consumed..], &mut buffer)?;
        ensure!(
            status.bytes_read + status.bytes_written > 0,
            "zstd encoder stalled"
        );
        consumed += status.bytes_read;
        payload.extend_from_slice(&buffer[..status.bytes_written]);
    }
    loop {
        let mut out = OutBuffer::around(buffer.as_mut_slice());
        let remaining = encoder.flush(&mut out)?;
        payload.extend_from_slice(&out.as_slice()[..out.pos()]);
        if remaining == 0 {
            break;
        }
    }
    if last {
        loop {
            let mut out = OutBuffer::around(buffer.as_mut_slice());
            let remaining = encoder.finish(&mut out, true)?;
            payload.extend_from_slice(&out.as_slice()[..out.pos()]);
            if remaining == 0 {
                break;
            }
        }
    }
    Ok(payload)
}

/// Measures a combination and fails if any frame does not round-trip on arrival.
/// Only the legacy zstd3_stream allows Ruzstd output to be held until EOF.
pub fn measure(
    name: &str,
    chunks: &[Chunk],
    codec: &str,
    batch_ms: u64,
    max_frame: usize,
) -> Result<Measurement> {
    ensure!(CODECS.contains(&codec), "unknown codec {codec}");
    let frames = frame(chunks, batch_ms, max_frame)?;
    let raw_bytes: usize = frames.iter().map(Vec::len).sum();
    ensure!(raw_bytes > 0, "workload {name} is empty");
    let mut zencoder = Encoder::new(3)?;
    let mut zdecoder = Decoder::new()?;
    let mut deflater = Box::new(CompressorOxide::with_format_and_level(
        DataFormat::Raw,
        miniz_oxide::deflate::CompressionLevel::DefaultLevel,
    ));
    let mut inflater = InflateState::new_boxed(DataFormat::Raw);
    let mut ruz = ruzstd::decoding::FrameDecoder::new();
    let mut ruz_output = Vec::new();
    let expected = frames.concat();
    let mut ruz_buffer = vec![0; raw_bytes + 1];
    let mut encode_time = Duration::ZERO;
    let mut decode_time = Duration::ZERO;
    let mut ruz_time = Duration::ZERO;
    let mut wire_bytes = 0;
    let mut encode_history = Vec::new();
    let mut decode_history = Vec::new();
    let mut ruz_history = Vec::new();
    for (index, input) in frames.iter().enumerate() {
        let start = Instant::now();
        let payload = match codec {
            "none" => input.clone(),
            "lz4" => lz4_flex::block::compress_prepend_size(input),
            "zstd3" => zstd::bulk::compress(input, 3)?,
            "zstd3_prefix" => {
                let output = prefix_encode(input, &encode_history)?;
                append_history(&mut encode_history, input);
                output
            }
            "zstd3_stream" => zstd_encode(&mut zencoder, input, index + 1 == frames.len())?,
            "deflate_stream" => {
                // A full raw frame plus ample room for stored-block and sync markers.
                let mut output = vec![0; input.len() * 2 + 65536];
                let (status, read, written) =
                    compress(&mut deflater, input, &mut output, TDEFLFlush::Sync);
                ensure!(
                    status == TDEFLStatus::Okay && read == input.len() && written < output.len(),
                    "deflate encode failed: {status:?}"
                );
                output.truncate(written);
                output
            }
            _ => unreachable!(),
        };
        encode_time += start.elapsed();
        wire_bytes += payload.len() + anila_proto::data::OUT_HEADER + 4 + 29;
        let start = Instant::now();
        let decoded = match codec {
            "none" => payload.clone(),
            "lz4" => lz4_flex::block::decompress_size_prepended(&payload)?,
            "zstd3" => zstd::bulk::decompress(&payload, input.len())?,
            "zstd3_prefix" => {
                let mut decoder = zstd::zstd_safe::DCtx::default();
                zstd_result(decoder.ref_prefix(&decode_history))?;
                let mut output = Vec::with_capacity(input.len() + 1);
                zstd_result(decoder.decompress(&mut output, &payload))?;
                drop(decoder);
                append_history(&mut decode_history, &output);
                output
            }
            "zstd3_stream" => {
                let mut output = vec![0; input.len() + 1];
                let status = zdecoder.run_on_buffers(&payload, &mut output)?;
                ensure!(
                    status.bytes_read == payload.len(),
                    "zstd did not consume frame"
                );
                output.truncate(status.bytes_written);
                output
            }
            "deflate_stream" => {
                let mut output = vec![0; input.len() + 1];
                let status = inflate(&mut inflater, &payload, &mut output, MZFlush::Sync);
                ensure!(
                    status.status.is_ok() && status.bytes_consumed == payload.len(),
                    "inflate failed: {status:?}"
                );
                output.truncate(status.bytes_written);
                output
            }
            _ => unreachable!(),
        };
        decode_time += start.elapsed();
        ensure!(
            decoded == *input,
            "{codec} mismatch in {name}, frame {index}"
        );
        if codec.starts_with("zstd3") {
            let start = Instant::now();
            if codec == "zstd3" {
                ruz = ruzstd::decoding::FrameDecoder::new();
            }
            let written = if codec == "zstd3_prefix" {
                let output = prefix_decode(&payload, &ruz_history)?;
                let written = output.len();
                ruz_buffer[..written].copy_from_slice(&output);
                append_history(&mut ruz_history, &output);
                written
            } else {
                let (read, written) = ruz.decode_from_to(&payload, &mut ruz_buffer)?;
                ensure!(read == payload.len(), "ruzstd did not consume frame");
                written
            };
            ruz_time += start.elapsed();
            let output = &ruz_buffer[..written];
            if codec == "zstd3" || codec == "zstd3_prefix" {
                ensure!(
                    output == input,
                    "ruzstd {codec} frame {index} mismatch in {name}"
                );
            }
            ruz_output.extend_from_slice(output);
            ensure!(
                expected.starts_with(&ruz_output),
                "ruzstd output mismatch: {name}"
            );
        }
    }
    if codec.starts_with("zstd3") {
        ensure!(ruz_output == expected, "ruzstd workload mismatch: {name}");
    }
    let scale = 1_000_000.0 / raw_bytes as f64;
    Ok(Measurement {
        workload: name.into(),
        codec: codec.into(),
        batch_ms,
        max_frame,
        raw_bytes,
        wire_bytes,
        ratio: wire_bytes as f64 / raw_bytes as f64,
        frames: frames.len(),
        encode_us_per_mb: encode_time.as_secs_f64() * 1e6 * scale,
        decode_us_per_mb: decode_time.as_secs_f64() * 1e6 * scale,
        decode_ruzstd_us_per_mb: codec
            .starts_with("zstd3")
            .then_some(ruz_time.as_secs_f64() * 1e6 * scale),
        git_commit: String::new(),
        timestamp: 0,
    })
}

/// Returns the checkout's short commit, or `unknown` outside a Git checkout.
pub fn git_commit() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Runs the complete matrix, saves JSONL, and returns a sorted Markdown table.
pub fn run(workloads: &Path, out: &Path) -> Result<String> {
    let mut paths = fs::read_dir(workloads)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|path| path.extension().is_some_and(|ext| ext == "jsonl"));
    paths.sort();
    ensure!(
        !paths.is_empty(),
        "no workloads; run anila-bench synth first"
    );
    let commit = git_commit();
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let date = Command::new("date").args(["-u", "+%Y-%m-%d"]).output()?;
    ensure!(date.status.success(), "date command failed");
    let date = String::from_utf8(date.stdout)?;
    let mut results = Vec::new();
    for path in paths {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("invalid workload filename")?;
        let chunks = read_workload(&path)?;
        for batch in [0, 4, 16, 32] {
            for cap in [8 << 10, 32 << 10] {
                for codec in CODECS {
                    let mut result = measure(name, &chunks, codec, batch, cap)?;
                    result.git_commit = commit.clone();
                    result.timestamp = timestamp;
                    results.push(result);
                }
            }
        }
    }
    results.sort_by(|a, b| {
        a.workload
            .cmp(&b.workload)
            .then_with(|| a.ratio.total_cmp(&b.ratio))
    });
    fs::create_dir_all(out)?;
    let path = out.join(format!("{}-{commit}-codecs.jsonl", date.trim()));
    let mut file = BufWriter::new(File::create(&path)?);
    let mut table = String::from(
        "| Workload | Codec | Batch ms | Max KiB | Frames | Raw | Wire | Ratio | Encode µs/MB | Decode µs/MB | Ruzstd µs/MB | Top |\n|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|:---:|\n",
    );
    let mut previous = "";
    let mut rank = 0;
    for result in &results {
        if previous != result.workload {
            previous = &result.workload;
            rank = 0;
        }
        rank += 1;
        serde_json::to_writer(&mut file, result)?;
        writeln!(file)?;
        use std::fmt::Write;
        writeln!(
            table,
            "| {} | {} | {} | {} | {} | {} | {} | {:.4} | {:.0} | {:.0} | {} | {} |",
            result.workload,
            result.codec,
            result.batch_ms,
            result.max_frame / 1024,
            result.frames,
            result.raw_bytes,
            result.wire_bytes,
            result.ratio,
            result.encode_us_per_mb,
            result.decode_us_per_mb,
            result
                .decode_ruzstd_us_per_mb
                .map(|n| format!("{n:.0}"))
                .unwrap_or_else(|| "—".into()),
            if rank <= 3 { "★" } else { "" }
        )?;
    }
    file.flush()?;
    fs::write(
        out.join(format!("{}-{commit}-codecs.md", date.trim())),
        &table,
    )?;
    Ok(table)
}

/// Parses and executes the hand-written command line.
pub fn cli(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("synth") if args.len() == 1 => {
            fs::create_dir_all("bench/workloads")?;
            for (name, chunks) in synthetic(false) {
                write_workload(
                    &Path::new("bench/workloads").join(format!("{name}.jsonl")),
                    &chunks,
                )?;
            }
        }
        Some("phone-cpu") => crate::phone::cli(&args[1..])?,
        Some("net") => net::cli(&args[1..])?,
        Some("codecs") => {
            let mut workloads = "bench/workloads";
            let mut out = "bench/results";
            let (options, remainder) = args[1..].as_chunks::<2>();
            for pair in options {
                match pair[0].as_str() {
                    "--workloads" => workloads = &pair[1],
                    "--out" => out = &pair[1],
                    other => bail!("unknown option {other}"),
                }
            }
            ensure!(remainder.is_empty(), "option requires a value");
            print!("{}", run(Path::new(workloads), Path::new(out))?);
        }
        Some("record") => {
            let name = args.get(1).context("record requires a name")?;
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "name must contain only letters, digits, - or _"
            );
            let mut cols = 120;
            let mut rows = 40;
            let mut i = 2;
            while args.get(i).is_some_and(|s| s != "--") {
                let value: u16 = args.get(i + 1).context("missing size value")?.parse()?;
                match args[i].as_str() {
                    "--cols" => cols = value,
                    "--rows" => rows = value,
                    other => bail!("unknown option {other}"),
                }
                i += 2;
            }
            ensure!(
                args.get(i).is_some_and(|s| s == "--"),
                "record requires -- before program"
            );
            let program = args.get(i + 1).context("missing program")?;
            fs::create_dir_all("bench/workloads")?;
            let (bytes, duration) = record(
                &Path::new("bench/workloads").join(format!("{name}.jsonl")),
                program,
                &args[i + 2..],
                cols,
                rows,
                true,
            )?;
            eprintln!(
                "\nRecorded {bytes} bytes in {:.3} s",
                duration.as_secs_f64()
            );
        }
        _ => bail!(
            "usage: anila-bench net [--profiles LIST] [--codecs LIST] [--out DIR] | synth | codecs [--workloads DIR] [--out DIR] | record NAME [--cols N --rows N] -- PROGRAM [ARGS..]"
        ),
    }
    Ok(())
}

mod net {
    use super::*;
    use crate::{
        probe::{Probe, ProbeOpts},
        proxy::{Mode, Proxy},
        stack::{Service, Worker, WorkerOpts},
        tmux::TmuxServer,
    };
    use anila_proto::{Codec, client::ClientConfig};
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };
    use tokio::{
        task::JoinSet,
        time::{sleep, timeout},
    };

    const PROFILES: [&str; 4] = ["lan", "good4g", "poor4g", "train"];
    const CODECS: [&str; 3] = ["none", "deflate_stream", "zstd_stream"];
    const LIMIT: Duration = Duration::from_secs(30);

    #[derive(Debug, PartialEq)]
    struct Options {
        profiles: Vec<String>,
        codecs: Vec<String>,
        out: PathBuf,
    }

    impl Options {
        fn parse(args: &[String]) -> Result<Self> {
            let mut options = Self {
                profiles: PROFILES.map(str::to_owned).to_vec(),
                codecs: CODECS.map(str::to_owned).to_vec(),
                out: "bench/results".into(),
            };
            let (pairs, rest) = args.as_chunks::<2>();
            ensure!(rest.is_empty(), "option requires a value");
            let mut seen = std::collections::HashSet::new();
            for [key, value] in pairs {
                ensure!(seen.insert(key), "duplicate option {key}");
                match key.as_str() {
                    "--profiles" => options.profiles = selection(value, &PROFILES)?,
                    "--codecs" => options.codecs = selection(value, &CODECS)?,
                    "--out" => {
                        ensure!(
                            !value.is_empty() && !value.starts_with("--"),
                            "invalid output path"
                        );
                        options.out = value.into();
                    }
                    _ => bail!("unknown net option {key}"),
                }
            }
            Ok(options)
        }
    }

    fn selection(value: &str, allowed: &[&str]) -> Result<Vec<String>> {
        let mut result = Vec::new();
        for name in value.split(',') {
            ensure!(allowed.contains(&name), "unknown selection {name:?}");
            ensure!(
                !result.iter().any(|item| item == name),
                "duplicate selection {name}"
            );
            result.push(name.to_owned());
        }
        Ok(result)
    }

    #[derive(Serialize)]
    struct Metrics {
        profile: String,
        codec: String,
        echo_p50_ms: f64,
        echo_p95_ms: f64,
        echo_max_ms: f64,
        throughput_ms: f64,
        throughput_down_bytes: u64,
        resume_ms: f64,
        git_commit: String,
        timestamp: u64,
        ping_ms: u64,
        dead_ms: u64,
    }

    pub(super) fn cli(args: &[String]) -> Result<()> {
        let options = Options::parse(args)?;
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run(options))
    }

    async fn until(mut condition: impl FnMut() -> bool) -> Result<()> {
        timeout(LIMIT, async {
            while !condition() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .context("network benchmark observation timed out")
    }

    fn tmux_checked(tmux: &TmuxServer, args: &[&str]) -> Result<()> {
        let output = Command::new("tmux")
            .args(["-L", tmux.socket()])
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "tmux: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    async fn run(options: Options) -> Result<()> {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let seed = [22; 32];
        let public = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let worker = Worker::start(WorkerOpts {
            dev_insecure: true,
            host_keys: serde_json::json!({"netbench": URL_SAFE_NO_PAD.encode(public.as_bytes())})
                .to_string(),
            default_host: "netbench".into(),
            extra_vars: Vec::new(),
            ..Default::default()
        })?;
        let tmux = TmuxServer::new("anila-netbench");
        tmux_checked(&tmux, &["new-session", "-d", "-s", "keeper", "cat"])?;
        // Keep the final seq screen available even if it exits before attach.
        tmux_checked(&tmux, &["set-window-option", "-g", "remain-on-exit", "on"])?;
        let config = format!(
            "host = \"netbench\"\nworker = {:?}\ntmux_socket = {:?}\n\
             [[preset]]\nid = \"echo\"\nlabel = \"Echo\"\ndir = \"/tmp\"\ncmd = \"cat\"\n\
             [[preset]]\nid = \"throughput\"\nlabel = \"Throughput\"\ndir = \"/tmp\"\ncmd = \"sh -c 'seq 1 20000'\"\n\
             [[preset]]\nid = \"resume\"\nlabel = \"Resume\"\ndir = \"/tmp\"\ncmd = \"sh -c 'while :; do date; sleep 0.05; done'\"\n",
            worker.http_base(),
            tmux.socket()
        );
        let _service = Service::start(&config, seed)?;
        let proxy = Arc::new(Proxy::start(([127, 0, 0, 1], worker.port).into()).await?);
        fs::create_dir_all(&options.out)?;
        let commit = git_commit();
        let date = Command::new("date").args(["-u", "+%Y-%m-%d"]).output()?;
        ensure!(date.status.success(), "date command failed");
        let date = String::from_utf8(date.stdout)?;
        let mut file = BufWriter::new(File::create(
            options
                .out
                .join(format!("{}-{commit}-net.jsonl", date.trim())),
        )?);
        println!(
            "| Profile | Codec | Echo p50 ms | p95 ms | max ms | Throughput ms | Down bytes | Resume ms |\n|---|---|---:|---:|---:|---:|---:|---:|"
        );
        let mut tab = 0;
        for profile in &options.profiles {
            let (latency, bandwidth) = match profile.as_str() {
                "good4g" => (25, Some(2_500_000)),
                "poor4g" => (75, Some(125_000)),
                "train" => (200, Some(32_000)),
                _ => (0, None),
            };
            proxy.set_latency(Duration::from_millis(latency));
            proxy.set_bandwidth(bandwidth);
            let automatic = Arc::new(Mutex::new(true));
            let mut faults = JoinSet::new();
            if profile == "train" {
                let proxy = proxy.clone();
                let automatic = automatic.clone();
                faults.spawn(async move {
                    let start = Instant::now();
                    loop {
                        {
                            let enabled = automatic.lock().unwrap_or_else(|e| e.into_inner());
                            if *enabled {
                                let elapsed = start.elapsed().as_secs_f64();
                                proxy.set_mode(if elapsed >= 30.0 && elapsed % 30.0 < 3.0 {
                                    Mode::Blackhole
                                } else {
                                    Mode::Pass
                                });
                            }
                        }
                        sleep(Duration::from_millis(10)).await;
                    }
                });
            }
            for codec in &options.codecs {
                tab += 1;
                *automatic.lock().unwrap_or_else(|e| e.into_inner()) = true;
                let result =
                    measure(&worker, &proxy, profile, codec, tab, &commit, &automatic).await;
                {
                    let mut enabled = automatic.lock().unwrap_or_else(|e| e.into_inner());
                    *enabled = false;
                    proxy.set_mode(Mode::Pass);
                }
                for kind in ["echo", "throughput", "resume"] {
                    tmux.cmd(&["kill-session", "-t", &format!("=n{tab}-{kind}")]);
                }
                let result = result.with_context(|| format!("{profile}/{codec}"))?;
                serde_json::to_writer(&mut file, &result)?;
                writeln!(file)?;
                file.flush()?;
                println!(
                    "| {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {:.1} |",
                    result.profile,
                    result.codec,
                    result.echo_p50_ms,
                    result.echo_p95_ms,
                    result.echo_max_ms,
                    result.throughput_ms,
                    result.throughput_down_bytes,
                    result.resume_ms
                );
            }
        }
        Ok(())
    }

    async fn measure(
        worker: &Worker,
        proxy: &Proxy,
        profile: &str,
        codec: &str,
        tab: u32,
        commit: &str,
        automatic: &Mutex<bool>,
    ) -> Result<Metrics> {
        let selected = match codec {
            "deflate_stream" => Codec::DeflateStream,
            "zstd_stream" => Codec::ZstdStream,
            _ => Codec::None,
        };
        let probe = Arc::new(
            timeout(
                LIMIT,
                Probe::start(ProbeOpts {
                    url: format!("ws://127.0.0.1:{}/ws/term", proxy.port()),
                    origin: worker.origin(),
                    cookie: None,
                    tab,
                    config: ClientConfig {
                        codecs: vec![selected],
                        ping_ms: 500,
                        dead_ms: 2_000,
                        ..ClientConfig::default()
                    },
                    backoff_scale: 1.0,
                }),
            )
            .await?,
        );
        until(|| probe.is_connected() && probe.host_online() == Some(true)).await?;
        let echo = probe
            .open(&format!("n{tab}-echo"), Some("echo"), 120, 40, LIMIT)
            .await?;
        let mut samples = JoinSet::new();
        let start = tokio::time::Instant::now();
        for i in 0..50 {
            tokio::time::sleep_until(start + Duration::from_millis(i * 300)).await;
            let probe = probe.clone();
            samples.spawn(async move {
                let marker = format!("{tab:04x}{i:04x}");
                let start = Instant::now();
                timeout(LIMIT, async {
                    while !probe.type_str(echo, &format!("{marker}\n")).await {
                        sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .context("echo input stayed paused")?;
                probe.wait_output(echo, &marker, LIMIT).await?;
                Ok::<_, anyhow::Error>(start.elapsed().as_secs_f64() * 1000.0)
            });
        }
        let mut echo_ms = Vec::new();
        while let Some(sample) = samples.join_next().await {
            echo_ms.push(sample??);
        }
        echo_ms.sort_by(f64::total_cmp);
        probe.close_channel(echo).await;

        let down = proxy.bytes().1;
        let start = Instant::now();
        let throughput = probe
            .open(
                &format!("n{tab}-throughput"),
                Some("throughput"),
                120,
                40,
                LIMIT,
            )
            .await?;
        probe.wait_output(throughput, "20000", LIMIT).await?;
        let throughput_ms = start.elapsed().as_secs_f64() * 1000.0;
        let throughput_down_bytes = proxy.bytes().1 - down;
        probe.close_channel(throughput).await;

        let resume = probe
            .open(&format!("n{tab}-resume"), Some("resume"), 120, 40, LIMIT)
            .await?;
        until(|| !probe.screen(resume).is_empty()).await?;
        // Keep the scheduled train outage from extending the explicit resume test.
        let resumes = probe.stats().resumes;
        {
            let mut enabled = automatic.lock().unwrap_or_else(|e| e.into_inner());
            *enabled = false;
            proxy.set_mode(Mode::Blackhole);
        }
        sleep(Duration::from_secs(5)).await;
        let before = probe.screen(resume).len();
        let start = Instant::now();
        proxy.set_mode(Mode::Pass);
        until(|| probe.stats().resumes > resumes && probe.screen(resume).len() > before).await?;
        let resume_ms = start.elapsed().as_secs_f64() * 1000.0;
        probe.close_channel(resume).await;
        Ok(Metrics {
            profile: profile.into(),
            codec: codec.into(),
            echo_p50_ms: echo_ms[24],
            echo_p95_ms: echo_ms[47],
            echo_max_ms: echo_ms[49],
            throughput_ms,
            throughput_down_bytes,
            resume_ms,
            git_commit: commit.into(),
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            ping_ms: 500,
            dead_ms: 2_000,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        fn parse(args: &[&str]) -> Result<Options> {
            Options::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        }
        #[test]
        fn net_arguments() {
            let defaults = parse(&[]).unwrap();
            assert_eq!(defaults.profiles, PROFILES);
            assert_eq!(defaults.codecs, CODECS);
            assert_eq!(defaults.out, Path::new("bench/results"));
            let selected = parse(&[
                "--codecs",
                "zstd_stream,none",
                "--out",
                "/tmp/net",
                "--profiles",
                "train,lan",
            ])
            .unwrap();
            assert_eq!(selected.profiles, ["train", "lan"]);
            assert_eq!(selected.codecs, ["zstd_stream", "none"]);
            assert_eq!(selected.out, Path::new("/tmp/net"));
            for args in [
                vec!["--profiles"],
                vec!["--profiles", ""],
                vec!["--profiles", "lan,"],
                vec!["--profiles", "bad"],
                vec!["--codecs", "lz4"],
                vec!["--codecs", "none,none"],
                vec!["--out", ""],
                vec!["--other", "x"],
                vec!["--out", "a", "--out", "b"],
            ] {
                assert!(parse(&args).is_err(), "{args:?}");
            }
        }
    }
}
