use anila_e2e::bench::{
    CODECS, Chunk, frame, measure, read_workload, record, synthetic, write_workload,
};

#[test]
fn framing_deadlines_and_size_caps() {
    let chunks: Vec<_> = [0, 2, 5, 30, 31]
        .into_iter()
        .enumerate()
        .map(|(i, t_ms)| Chunk {
            t_ms,
            bytes: vec![i as u8; i + 1],
        })
        .collect();
    for (batch, sizes) in [
        (0, vec![1, 2, 3, 4, 5]),
        (4, vec![3, 3, 9]),
        (16, vec![6, 9]),
    ] {
        let frames = frame(&chunks, batch, 32768).unwrap();
        assert_eq!(frames.iter().map(Vec::len).collect::<Vec<_>>(), sizes);
        assert_eq!(
            frames.concat(),
            chunks
                .iter()
                .flat_map(|c| c.bytes.clone())
                .collect::<Vec<_>>()
        );
    }
    let large = [Chunk {
        t_ms: 0,
        bytes: vec![42; 40 * 1024],
    }];
    assert_eq!(
        frame(&large, 16, 32768)
            .unwrap()
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        [32768, 8192]
    );
    let exact = [
        Chunk {
            t_ms: 0,
            bytes: vec![1],
        },
        Chunk {
            t_ms: 4,
            bytes: vec![2],
        },
    ];
    assert_eq!(frame(&exact, 4, 8192).unwrap().len(), 2);
    assert!(frame(&exact, 4, 0).is_err());
    assert!(frame(&[exact[1].clone(), exact[0].clone()], 4, 8192).is_err());
}

#[test]
fn all_codecs_round_trip_synthetic_workloads() {
    for (name, chunks) in synthetic(true) {
        for codec in CODECS {
            for batch in [0, 4, 16, 32] {
                for cap in [8192, 32768] {
                    let result = measure(&name, &chunks, codec, batch, cap).unwrap();
                    assert_eq!(
                        result.raw_bytes,
                        chunks.iter().map(|c| c.bytes.len()).sum::<usize>()
                    );
                    assert_eq!(
                        result.decode_ruzstd_us_per_mb.is_some(),
                        codec.starts_with("zstd3")
                    );
                }
            }
        }
    }
}

#[test]
fn streaming_saves_tui_bytes_and_random_is_incompressible() {
    for (name, chunks) in synthetic(true) {
        for cap in [8192, 32768] {
            for batch in [0, 4, 16, 32] {
                let results: Vec<_> = CODECS
                    .iter()
                    .map(|codec| measure(&name, &chunks, codec, batch, cap).unwrap())
                    .collect();
                if name == "synth-tui" {
                    for streaming in &results[3..] {
                        for per_frame in &results[..3] {
                            assert!(
                                streaming.wire_bytes < per_frame.wire_bytes,
                                "{} versus {}",
                                streaming.codec,
                                per_frame.codec
                            );
                        }
                    }
                }
                if name == "synth-random" {
                    assert!(results.iter().all(|r| r.ratio >= 0.95));
                }
            }
        }
    }
}

#[test]
fn workload_format_is_lossless_and_rejects_bad_input() {
    let path = std::env::temp_dir().join(format!("anila-workload-{}.jsonl", std::process::id()));
    let chunks = synthetic(true).pop().unwrap().1;
    write_workload(&path, &chunks).unwrap();
    assert_eq!(read_workload(&path).unwrap(), chunks);
    std::fs::write(&path, "{\"t_ms\":0,\"b64\":\"!\"}\n").unwrap();
    assert!(read_workload(&path).is_err());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn record_captures_separated_reads() {
    let path = std::env::temp_dir().join(format!("anila-record-{}.jsonl", std::process::id()));
    let result = record(
        &path,
        "/bin/sh",
        &["-c".into(), "printf hello; sleep 0.2; printf world".into()],
        120,
        40,
        false,
    );
    if let Err(error) = &result {
        let message = format!("{error:#}");
        if message.contains("Operation not permitted") || message.contains("Permission denied") {
            eprintln!("SKIP record_captures_separated_reads: PTYs denied: {message}");
            let _ = std::fs::remove_file(path);
            return;
        }
    }
    let (bytes, duration) = result.unwrap();
    let chunks = read_workload(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(bytes, 10);
    assert!(duration.as_millis() >= 150);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].bytes, b"hello");
    assert_eq!(chunks[1].bytes, b"world");
    assert!(chunks[1].t_ms - chunks[0].t_ms >= 150);
}

#[test]
#[ignore = "regenerates the checked-in zstd dictionary template"]
fn generate_zstd_template() {
    let samples: Vec<Vec<u8>> = (0..128)
        .map(|i| {
            format!(
                "\x1b[32m Compiling crate_{i} v0.1.0\x1b[0m\r\n\x1b[H terminal session {i}: build complete\r\n"
            )
            .into_bytes()
        })
        .collect();
    let template = zstd::dict::from_samples(&samples, 4096).unwrap();
    ruzstd::decoding::Dictionary::decode_dict(&template).unwrap();
    std::fs::write(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../proto/src/zstd_template.dict"),
        template,
    )
    .unwrap();
}

#[test]
fn zstd_prefix_template_parses() {
    let dictionary = ruzstd::decoding::Dictionary::decode_dict(include_bytes!(
        "../../proto/src/zstd_template.dict"
    ))
    .unwrap();
    assert_ne!(dictionary.id, 0);
}

#[test]
fn zstd_prefix_round_trips_each_piece_on_arrival() {
    for (name, chunks) in synthetic(false) {
        for cap in [8192, 32768] {
            let result = measure(&name, &chunks, "zstd3_prefix", 16, cap).unwrap();
            assert_eq!(result.frames, frame(&chunks, 16, cap).unwrap().len());
            assert_eq!(
                result.raw_bytes,
                chunks.iter().map(|c| c.bytes.len()).sum::<usize>()
            );
            assert!(result.decode_ruzstd_us_per_mb.is_some());
        }
    }
}
