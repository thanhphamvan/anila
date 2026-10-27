//! Per-channel codecs for terminal output streams.
//!
//! `Codec::ZstdStream` uses complete level-3 zstd frames, each referencing the
//! previous 32 KiB of uncompressed output as a raw-content prefix. Frames have
//! no checksum or dictionary ID and decode completely on arrival. A new encoder
//! or decoder resets history at opened, resumed, and reset boundaries.

use miniz_oxide::{
    DataFormat, MZError, MZFlush, MZStatus,
    deflate::{
        CompressionLevel,
        core::{CompressorOxide, TDEFLFlush, compress},
    },
    inflate::stream::{InflateState, inflate},
};

use crate::Codec;

/// Codecs this build implements, in order of preference.
pub const SUPPORTED: &[Codec] = &[Codec::ZstdStream, Codec::DeflateStream, Codec::None];

/// Codecs this build can encode, in preference order.
pub const ENCODABLE: &[Codec] = &[
    #[cfg(feature = "zstd-encode")]
    Codec::ZstdStream,
    Codec::DeflateStream,
    Codec::None,
];

const MAX_OUTPUT: usize = 4 << 20;
const ZSTD_TEMPLATE: &[u8] = include_bytes!("zstd_template.dict");

fn append_history(history: &mut Vec<u8>, bytes: &[u8]) {
    let keep = bytes.len().min(32 << 10);
    if keep == 32 << 10 {
        history.clear();
    } else {
        let excess = (history.len() + keep).saturating_sub(32 << 10);
        history.drain(..excess);
    }
    history.extend_from_slice(&bytes[bytes.len() - keep..]);
}

/// One encoder per channel stream. Cloning provides a checkpoint for sizing retries.
#[derive(Clone)]
pub struct Encoder {
    state: Option<Box<CompressorOxide>>,
    #[cfg(feature = "zstd-encode")]
    prefix: Option<Vec<u8>>,
}

impl Encoder {
    /// Creates a fresh stream; unsupported codecs fall back to plain bytes.
    pub fn new(codec: Codec) -> Self {
        Self {
            #[cfg(feature = "zstd-encode")]
            prefix: (codec == Codec::ZstdStream).then(Vec::new),
            state: (codec == Codec::DeflateStream).then(|| {
                Box::new(CompressorOxide::with_format_and_level(
                    DataFormat::Raw,
                    CompressionLevel::DefaultLevel,
                ))
            }),
        }
    }

    /// The codec actually used by this encoder.
    pub fn codec(&self) -> Codec {
        #[cfg(feature = "zstd-encode")]
        if self.prefix.is_some() {
            return Codec::ZstdStream;
        }
        if self.state.is_some() {
            Codec::DeflateStream
        } else {
            Codec::None
        }
    }

    /// Compresses `bytes`, returning one completely decodable piece.
    pub fn encode(&mut self, bytes: &[u8]) -> Vec<u8> {
        #[cfg(feature = "zstd-encode")]
        if let Some(history) = &mut self.prefix {
            let output = encode_prefix(bytes, history);
            append_history(history, bytes);
            return output;
        }
        let Some(state) = &mut self.state else {
            return bytes.to_vec();
        };
        let mut output = vec![0; bytes.len() * 2 + 65536];
        let (_, _, written) = compress(state, bytes, &mut output, TDEFLFlush::Sync);
        output.truncate(written);
        output
    }
}

/// One decoder per channel stream.
pub struct Decoder {
    state: Option<Box<InflateState>>,
    prefix: Option<Vec<u8>>,
}

/// An invalid or excessively large compressed piece.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("corrupt compressed output")]
    Corrupt,
}

impl Decoder {
    /// Creates a fresh stream; unsupported codecs fall back to plain bytes.
    pub fn new(codec: Codec) -> Self {
        Self {
            prefix: (codec == Codec::ZstdStream).then(Vec::new),
            state: (codec == Codec::DeflateStream)
                .then(|| InflateState::new_boxed(DataFormat::Raw)),
        }
    }

    /// Decodes one piece produced by `Encoder::encode`, in order.
    pub fn decode(&mut self, piece: &[u8]) -> Result<Vec<u8>, CodecError> {
        if let Some(history) = &mut self.prefix {
            let output = decode_prefix(piece, history)?;
            append_history(history, &output);
            return Ok(output);
        }
        let Some(state) = &mut self.state else {
            return Ok(piece.to_vec());
        };
        // Every piece must include its complete sync-flush marker.
        if !piece.ends_with(&[0, 0, 255, 255]) {
            return Err(CodecError::Corrupt);
        }
        const MAX: usize = 4 * 1024 * 1024;
        let mut output = Vec::new();
        let mut consumed = 0;
        loop {
            let mut buffer = [0; 16384];
            let result = inflate(state, &piece[consumed..], &mut buffer, MZFlush::Sync);
            consumed += result.bytes_consumed;
            if output.len() + result.bytes_written > MAX {
                return Err(CodecError::Corrupt);
            }
            output.extend_from_slice(&buffer[..result.bytes_written]);
            match result.status {
                Ok(MZStatus::Ok) => {}
                Err(MZError::Buf) if consumed == piece.len() && result.bytes_written == 0 => {
                    return Ok(output);
                }
                _ => return Err(CodecError::Corrupt),
            }
            if consumed == piece.len() && result.bytes_written < buffer.len() {
                return Ok(output);
            }
            if result.bytes_consumed + result.bytes_written == 0 {
                return Err(CodecError::Corrupt);
            }
        }
    }
}

/// The best codec both sides support, from the host's advertised list.
pub fn choose(advertised: &[Codec], preferred: &[Codec]) -> Codec {
    preferred
        .iter()
        .copied()
        .find(|codec| SUPPORTED.contains(codec) && advertised.contains(codec))
        .unwrap_or(Codec::None)
}

#[cfg(feature = "zstd-encode")]
fn encode_prefix(bytes: &[u8], history: &[u8]) -> Vec<u8> {
    use zstd::zstd_safe::{CCtx, CParameter, compress_bound};
    let compressed = (|| {
        let mut encoder = CCtx::try_create().ok_or(())?;
        for parameter in [
            CParameter::CompressionLevel(3),
            CParameter::ChecksumFlag(false),
            CParameter::DictIdFlag(false),
        ] {
            encoder.set_parameter(parameter).map_err(|_| ())?;
        }
        encoder.ref_prefix(history).map_err(|_| ())?;
        let mut output = Vec::with_capacity(compress_bound(bytes.len()));
        encoder.compress2(&mut output, bytes).map_err(|_| ())?;
        Ok::<_, ()>(output)
    })();
    compressed.unwrap_or_else(|_| {
        // Preserve the codec and history if C zstd cannot compress: raw zstd blocks.
        let mut output = vec![0x28, 0xb5, 0x2f, 0xfd, 0xe0];
        output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        if bytes.is_empty() {
            output.extend_from_slice(&[1, 0, 0]);
        }
        for (index, block) in bytes.chunks(128 << 10).enumerate() {
            let last = (index + 1) * (128 << 10) >= bytes.len();
            let header = ((block.len() as u32) << 3) | u32::from(last);
            output.extend_from_slice(&header.to_le_bytes()[..3]);
            output.extend_from_slice(block);
        }
        output
    })
}

// Validate allocation-driving fields before ruzstd initializes its window.
fn prefix_size(piece: &[u8]) -> Result<usize, CodecError> {
    let corrupt = CodecError::Corrupt;
    if piece.get(..4) != Some(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Err(corrupt);
    }
    let descriptor = *piece.get(4).ok_or(CodecError::Corrupt)?;
    if descriptor & 0x1f != 0 {
        return Err(corrupt);
    }
    let single = descriptor & 0x20 != 0;
    let mut offset = 5;
    if !single {
        let window = *piece.get(offset).ok_or(CodecError::Corrupt)?;
        let base = 1_u64 << (10 + (window >> 3));
        if base + (base / 8) * u64::from(window & 7) > MAX_OUTPUT as u64 {
            return Err(corrupt);
        }
        offset += 1;
    }
    let length = match descriptor >> 6 {
        0 if single => 1,
        0 => return Err(corrupt),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut size = [0; 8];
    size[..length].copy_from_slice(
        piece
            .get(offset..offset + length)
            .ok_or(CodecError::Corrupt)?,
    );
    let size = u64::from_le_bytes(size) + if length == 2 { 256 } else { 0 };
    if size > MAX_OUTPUT as u64 {
        return Err(corrupt);
    }
    Ok(size as usize)
}

fn decode_prefix(piece: &[u8], history: &[u8]) -> Result<Vec<u8>, CodecError> {
    use ruzstd::decoding::{BlockDecodingStrategy, Dictionary, FrameDecoder};
    let size = prefix_size(piece)?;
    // Ruzstd contains internal assertions; malformed input must not unwind callers.
    std::panic::catch_unwind(|| {
        let mut dictionary =
            Dictionary::decode_dict(ZSTD_TEMPLATE).map_err(|_| CodecError::Corrupt)?;
        dictionary.dict_content = history.to_vec();
        dictionary.offset_hist = [1, 4, 8];
        let id = dictionary.id;
        let mut decoder = FrameDecoder::new();
        decoder
            .add_dict(dictionary)
            .map_err(|_| CodecError::Corrupt)?;
        let mut source = piece;
        decoder.init(&mut source).map_err(|_| CodecError::Corrupt)?;
        decoder.force_dict(id).map_err(|_| CodecError::Corrupt)?;
        let mut output = Vec::new();
        while !decoder.is_finished() {
            decoder
                .decode_blocks(&mut source, BlockDecodingStrategy::UptoBlocks(1))
                .map_err(|_| CodecError::Corrupt)?;
            if let Some(bytes) = decoder.collect() {
                if output.len() + bytes.len() > size {
                    return Err(CodecError::Corrupt);
                }
                output.extend_from_slice(&bytes);
            }
        }
        if !source.is_empty() || output.len() != size {
            return Err(CodecError::Corrupt);
        }
        Ok(output)
    })
    .map_err(|_| CodecError::Corrupt)?
}
