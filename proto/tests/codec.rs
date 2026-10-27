use anila_proto::{
    Codec,
    codec::{CodecError, Decoder, Encoder, SUPPORTED, choose},
};

fn random(len: usize) -> Vec<u8> {
    let mut seed = 0x9e3779b97f4a7c15_u64;
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 32) as u8
        })
        .collect()
}

#[test]
fn synthetic_round_trips_piece_by_piece() {
    let build = (0..2000)
        .map(|n| format!("\x1b[32m Compiling\x1b[0m crate-{n} v1.0\r\n"))
        .collect::<String>()
        .into_bytes();
    let screen = format!("\x1b[H{}", "terminal output ".repeat(128));
    for input in [random(100_000), build, screen.repeat(100).into_bytes()] {
        let mut encoder = Encoder::new(Codec::DeflateStream);
        let mut decoder = Decoder::new(Codec::DeflateStream);
        for piece in input.chunks(4096) {
            assert_eq!(decoder.decode(&encoder.encode(piece)).unwrap(), piece);
        }
    }
}

#[test]
fn repeated_screen_benefits_from_stream_history() {
    let screen = random(80 * 24);
    let mut encoder = Encoder::new(Codec::DeflateStream);
    let mut decoder = Decoder::new(Codec::DeflateStream);
    let mut streaming = 0;
    let mut independent = 0;
    for _ in 0..40 {
        let piece = encoder.encode(&screen);
        streaming += piece.len();
        independent += Encoder::new(Codec::DeflateStream).encode(&screen).len();
        assert_eq!(decoder.decode(&piece).unwrap(), screen);
    }
    assert!(streaming * 6 < independent);
}

#[test]
fn none_and_unsupported_pass_through() {
    for codec in [Codec::None, Codec::Unknown, Codec::Zstd] {
        let mut encoder = Encoder::new(codec);
        assert_eq!(encoder.codec(), Codec::None);
        assert_eq!(encoder.encode(b"hello"), b"hello");
        assert_eq!(Decoder::new(codec).decode(b"hello").unwrap(), b"hello");
    }
}

#[test]
fn corrupt_and_oversized_pieces_are_bounded() {
    for len in 0..500 {
        let mut bytes = random(len);
        assert_eq!(
            Decoder::new(Codec::DeflateStream).decode(&bytes),
            Err(CodecError::Corrupt)
        );
        // Exercise inflate itself with an invalid block followed by a flush marker.
        bytes.insert(0, 0xff);
        bytes.extend_from_slice(&[0, 0, 255, 255]);
        assert_eq!(
            Decoder::new(Codec::DeflateStream).decode(&bytes),
            Err(CodecError::Corrupt)
        );
    }
    let mut encoder = Encoder::new(Codec::DeflateStream);
    let piece = encoder.encode(&vec![b'x'; 4 * 1024 * 1024 + 1]);
    assert_eq!(
        Decoder::new(Codec::DeflateStream).decode(&piece),
        Err(CodecError::Corrupt)
    );
    let piece = Encoder::new(Codec::DeflateStream).encode(&vec![b'x'; 4 * 1024 * 1024]);
    assert_eq!(
        Decoder::new(Codec::DeflateStream)
            .decode(&piece)
            .unwrap()
            .len(),
        4 * 1024 * 1024
    );
}

#[test]
fn negotiation_respects_preferences_and_supported_codecs() {
    // Preference order: zstd prefix frames, then deflate (T25 benchmark).
    assert_eq!(choose(SUPPORTED, SUPPORTED), Codec::ZstdStream);
    assert_eq!(
        choose(&[Codec::DeflateStream, Codec::None], SUPPORTED),
        Codec::DeflateStream
    );
    assert_eq!(choose(&[Codec::None], SUPPORTED), Codec::None);
    assert_eq!(choose(SUPPORTED, &[Codec::None]), Codec::None);
    assert_eq!(choose(&[Codec::Unknown], &[Codec::Unknown]), Codec::None);
    assert_eq!(choose(&[], SUPPORTED), Codec::None);
    assert_eq!(
        choose(SUPPORTED, &[Codec::None, Codec::DeflateStream]),
        Codec::None
    );
}

#[test]
fn empty_and_exact_buffer_pieces_round_trip() {
    let mut encoder = Encoder::new(Codec::DeflateStream);
    let mut decoder = Decoder::new(Codec::DeflateStream);
    for size in [0, 16384, 32768, 0, 65536, 1] {
        let bytes = random(size);
        assert_eq!(decoder.decode(&encoder.encode(&bytes)).unwrap(), bytes);
    }
}

#[test]
fn zstd_prefix_template_and_negotiation() {
    ruzstd::decoding::Dictionary::decode_dict(include_bytes!("../src/zstd_template.dict")).unwrap();
    assert_eq!(
        choose(&[Codec::ZstdStream, Codec::DeflateStream], SUPPORTED),
        Codec::ZstdStream
    );
    assert_eq!(
        SUPPORTED,
        &[Codec::ZstdStream, Codec::DeflateStream, Codec::None]
    );
}

#[cfg(not(feature = "zstd-encode"))]
#[test]
fn zstd_encoding_requires_feature() {
    assert!(!anila_proto::codec::ENCODABLE.contains(&Codec::ZstdStream));
    let mut encoder = Encoder::new(Codec::ZstdStream);
    assert_eq!(encoder.codec(), Codec::None);
    assert_eq!(encoder.encode(b"plain"), b"plain");
}

#[cfg(feature = "zstd-encode")]
#[test]
fn zstd_prefix_round_trip_and_fresh_history() {
    assert!(anila_proto::codec::ENCODABLE.contains(&Codec::ZstdStream));
    let build = (0..2000)
        .map(|n| format!("\x1b[32m Compiling\x1b[0m crate-{n} v1.0\r\n"))
        .collect::<String>()
        .into_bytes();
    let screen = format!("\x1b[H{}", format!("{:<80}", "terminal output").repeat(24));
    for input in [random(100_000), build, screen.repeat(100).into_bytes()] {
        let mut encoder = Encoder::new(Codec::ZstdStream);
        let mut decoder = Decoder::new(Codec::ZstdStream);
        assert_eq!(encoder.codec(), Codec::ZstdStream);
        for piece in input.chunks(4096) {
            let checkpoint = encoder.clone();
            let encoded = encoder.encode(piece);
            assert_eq!(checkpoint.clone().encode(piece), encoded);
            assert_eq!(decoder.decode(&encoded).unwrap(), piece);
        }
        // opened/resumed/reset replace both codec instances with fresh ones.
        encoder = Encoder::new(Codec::ZstdStream);
        decoder = Decoder::new(Codec::ZstdStream);
        for size in [0, 1, 16384, 32768, 65536, 0] {
            let piece = random(size);
            assert_eq!(decoder.decode(&encoder.encode(&piece)).unwrap(), piece);
        }
    }
    let screen = random(80 * 24);
    let mut encoder = Encoder::new(Codec::ZstdStream);
    let first = encoder.encode(&screen);
    let mut decoder = Decoder::new(Codec::ZstdStream);
    assert_eq!(decoder.decode(&first).unwrap(), screen);
    for _ in 0..40 {
        let encoded = encoder.encode(&screen);
        assert!(encoded.len() * 6 < first.len());
        assert_eq!(decoder.decode(&encoded).unwrap(), screen);
    }
    assert_eq!(Encoder::new(Codec::ZstdStream).encode(&screen), first);
}

#[cfg(feature = "zstd-encode")]
#[test]
fn zstd_prefix_rejects_corruption_truncation_and_oversize() {
    let encoded = Encoder::new(Codec::ZstdStream).encode(&random(4096));
    for end in 0..encoded.len() {
        assert_eq!(
            Decoder::new(Codec::ZstdStream).decode(&encoded[..end]),
            Err(CodecError::Corrupt)
        );
    }
    let mut corrupt = encoded.clone();
    corrupt[0] ^= 1;
    assert_eq!(
        Decoder::new(Codec::ZstdStream).decode(&corrupt),
        Err(CodecError::Corrupt)
    );
    let mut trailing = encoded;
    trailing.push(0);
    assert_eq!(
        Decoder::new(Codec::ZstdStream).decode(&trailing),
        Err(CodecError::Corrupt)
    );
    for size in [4 << 20, (4 << 20) + 1] {
        let bytes = vec![b'x'; size];
        let encoded = Encoder::new(Codec::ZstdStream).encode(&bytes);
        let decoded = Decoder::new(Codec::ZstdStream).decode(&encoded);
        if size == 4 << 20 {
            assert_eq!(decoded.unwrap(), bytes);
        } else {
            assert_eq!(decoded, Err(CodecError::Corrupt));
        }
    }
}

#[test]
fn zstd_prefix_rejects_invalid_headers() {
    for size in 0..500 {
        assert_eq!(
            Decoder::new(Codec::ZstdStream).decode(&random(size)),
            Err(CodecError::Corrupt)
        );
    }
    let huge_window = [0x28, 0xb5, 0x2f, 0xfd, 0x80, 0xff, 1, 0, 0, 0];
    assert_eq!(
        Decoder::new(Codec::ZstdStream).decode(&huge_window),
        Err(CodecError::Corrupt)
    );
}

#[test]
fn zstd_prefix_decoder_works_without_c_encoder() {
    let frame = b"\x28\xb5\x2f\xfd\x20\x05\x29\x00\x00hello";
    assert_eq!(
        Decoder::new(Codec::ZstdStream).decode(frame).unwrap(),
        b"hello"
    );
    let mut wrong_size = frame.to_vec();
    wrong_size[5] = 4;
    assert_eq!(
        Decoder::new(Codec::ZstdStream).decode(&wrong_size),
        Err(CodecError::Corrupt)
    );
    let mut bad_block = frame.to_vec();
    bad_block[6] |= 6;
    assert_eq!(
        Decoder::new(Codec::ZstdStream).decode(&bad_block),
        Err(CodecError::Corrupt)
    );
}
