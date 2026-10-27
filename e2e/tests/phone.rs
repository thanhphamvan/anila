//! Pure phone CPU benchmark checks: no browser, trunk, sockets or PTYs.

use anila_e2e::{
    bench::{self, Chunk},
    phone::{self, Measurement},
};
use anila_proto::{Codec, codec::Decoder};
use base64::{Engine, engine::general_purpose::STANDARD};

#[test]
fn pieces_round_trip_in_stream_order() {
    let mut workloads = bench::synthetic(true);
    workloads.push((
        "boundaries".into(),
        vec![
            Chunk {
                t_ms: 0,
                bytes: vec![1; 40_000],
            },
            Chunk {
                t_ms: 1,
                bytes: vec![2; 20],
            },
            Chunk {
                t_ms: 8,
                bytes: vec![3; 63],
            },
            Chunk {
                t_ms: 16,
                bytes: vec![4; 64],
            },
        ],
    ));
    for (_, chunks) in workloads {
        let frames = bench::frame(&chunks, 8, 32 << 10).unwrap();
        for codec in [Codec::None, Codec::DeflateStream, Codec::ZstdStream] {
            let encoded = phone::encode_pieces(&chunks, codec).unwrap();
            assert_eq!(
                encoded.raw_lengths,
                frames.iter().map(Vec::len).collect::<Vec<_>>()
            );
            let mut decoder = Decoder::new(codec);
            for (piece, expected) in encoded.pieces.iter().zip(&frames) {
                assert_eq!(
                    decoder.decode(&STANDARD.decode(piece).unwrap()).unwrap(),
                    *expected
                );
            }
            assert_eq!(encoded.pieces.len(), frames.len());
        }
    }
}

#[test]
fn percentiles_use_nearest_rank() {
    assert_eq!(phone::percentile(&[4., 1., 3., 2.], 50.), Some(2.));
    assert_eq!(phone::percentile(&[4., 1., 3., 2.], 95.), Some(4.));
    let samples: Vec<_> = (1..=100).map(f64::from).collect();
    assert_eq!(phone::percentile(&samples, 95.), Some(95.));
    assert_eq!(phone::percentile(&[], 50.), None);
}

#[test]
fn statistics_split_by_raw_size_and_use_decimal_mb() {
    let stats = phone::statistics(&[1., 3., 5.], &[63, 64, 999_873]).unwrap();
    assert_eq!(stats.pieces, 3);
    assert_eq!(stats.raw_bytes, 1_000_000);
    assert_eq!(stats.small_p95_ms, Some(1.));
    assert_eq!(stats.large_p50_ms, Some(3.));
    assert_eq!(stats.large_p95_ms, Some(5.));
    assert_eq!(stats.decode_ms_per_mb, Some(9.));
    assert!(phone::statistics(&[1.], &[]).is_err());
    assert!(phone::statistics(&[f64::NAN], &[1]).is_err());
    assert!(phone::statistics(&[-1.], &[1]).is_err());
}

#[test]
fn empty_groups_and_markdown_are_explicit() {
    let empty = phone::statistics(&[], &[]).unwrap();
    assert_eq!(empty.small_p50_ms, None);
    assert_eq!(empty.large_p95_ms, None);
    assert_eq!(empty.decode_ms_per_mb, None);
    assert!(
        phone::encode_pieces(&[], Codec::ZstdStream)
            .unwrap()
            .pieces
            .is_empty()
    );
    let row = Measurement {
        rate: 4.,
        workload: "test|name".into(),
        codec: Codec::ZstdStream,
        stats: phone::statistics(&[2.], &[1000]).unwrap(),
        git_commit: "abc123".into(),
        timestamp: 1,
    };
    assert_eq!(row.stats.small_p95_ms, None);
    let json = serde_json::to_value(&row).unwrap();
    assert!(json["small_p95_ms"].is_null());
    assert_eq!(json["git_commit"], "abc123");
    let table = phone::markdown(&[row]);
    assert!(table.contains(
        "| 4 | test&#124;name | zstd_stream | 1 | 1000 | — | — | 2.0000 | 2.0000 | 2000.0000 |"
    ));
}

#[test]
fn invalid_options_fail_before_starting_external_tools() {
    for args in [
        vec!["--rates", "0"],
        vec!["--rates", "NaN"],
        vec!["--rates", ""],
        vec!["--rates"],
        vec!["--unknown", "4"],
    ] {
        assert!(phone::cli(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
    }
}

#[test]
fn bulk_timing_replaces_the_rounded_sum() {
    // Per-piece times rounded to zero would claim free decoding.
    let mut stats = phone::statistics(&[0.0, 0.0, 0.0, 0.0], &[10, 20, 30, 940]).unwrap();
    assert_eq!(stats.decode_ms_per_mb, Some(0.0));
    // 4 runs over 1000 raw bytes in 2 ms: 0.5 ms per run.
    stats.bulk(2.0, 4);
    assert_eq!(stats.decode_ms_per_mb, Some(500.0));
    assert_eq!(stats.mean_us_per_piece, Some(125.0));
    let mut empty = phone::statistics(&[], &[]).unwrap();
    empty.bulk(1.0, 0);
    assert_eq!(empty.decode_ms_per_mb, None);
    assert_eq!(empty.mean_us_per_piece, None);
}
