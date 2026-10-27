//! Binary frames carrying terminal bytes. All integers are big-endian.
//!
//! ```text
//! OUT / OUT_Z  service → browser        IN  browser → service
//! byte size field                       byte size field
//! 0    1    kind 0x01 or 0x03           0    1    kind 0x02
//! 1    4    tab                         1    4    tab
//! 5    2    ch                          5    2    ch
//! 7    4    epoch                       7    4    id
//! 11   8    off                         11   ..   bytes, ≤ 4 KiB
//! 19   ..   bytes, ≤ 32 KiB
//! ```

use crate::limits::{MAX_IN_PAYLOAD, MAX_OUT_PAYLOAD};
use crate::{Ch, Epoch, InputId, Off, Tab};

pub const KIND_OUT: u8 = 0x01;
pub const KIND_IN: u8 = 0x02;
/// OUT with a payload compressed by the channel's codec.
pub const KIND_OUT_Z: u8 = 0x03;

pub const OUT_HEADER: usize = 19;
pub const IN_HEADER: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Data<'a> {
    Out {
        tab: Tab,
        ch: Ch,
        epoch: Epoch,
        /// Offset of the first *uncompressed* byte this frame carries.
        off: Off,
        compressed: bool,
        bytes: &'a [u8],
    },
    In {
        tab: Tab,
        ch: Ch,
        id: InputId,
        bytes: &'a [u8],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("empty frame")]
    Empty,
    #[error("unknown frame kind {0:#04x}")]
    UnknownKind(u8),
    #[error("frame of kind {kind:#04x} is {len} bytes, shorter than its header")]
    Short { kind: u8, len: usize },
    #[error("payload of {len} bytes exceeds the {max} byte limit")]
    TooLarge { len: usize, max: usize },
}

impl<'a> Data<'a> {
    pub fn tab(&self) -> Tab {
        match self {
            Data::Out { tab, .. } | Data::In { tab, .. } => *tab,
        }
    }

    /// Appends the encoded frame to `buf`.
    pub fn encode(&self, buf: &mut Vec<u8>) -> Result<(), FrameError> {
        match *self {
            Data::Out {
                tab,
                ch,
                epoch,
                off,
                compressed,
                bytes,
            } => {
                if bytes.len() > MAX_OUT_PAYLOAD {
                    return Err(FrameError::TooLarge {
                        len: bytes.len(),
                        max: MAX_OUT_PAYLOAD,
                    });
                }
                buf.reserve(OUT_HEADER + bytes.len());
                buf.push(if compressed { KIND_OUT_Z } else { KIND_OUT });
                buf.extend_from_slice(&tab.to_be_bytes());
                buf.extend_from_slice(&ch.to_be_bytes());
                buf.extend_from_slice(&epoch.to_be_bytes());
                buf.extend_from_slice(&off.to_be_bytes());
                buf.extend_from_slice(bytes);
            }
            Data::In { tab, ch, id, bytes } => {
                if bytes.len() > MAX_IN_PAYLOAD {
                    return Err(FrameError::TooLarge {
                        len: bytes.len(),
                        max: MAX_IN_PAYLOAD,
                    });
                }
                buf.reserve(IN_HEADER + bytes.len());
                buf.push(KIND_IN);
                buf.extend_from_slice(&tab.to_be_bytes());
                buf.extend_from_slice(&ch.to_be_bytes());
                buf.extend_from_slice(&id.to_be_bytes());
                buf.extend_from_slice(bytes);
            }
        }
        Ok(())
    }

    pub fn to_vec(&self) -> Result<Vec<u8>, FrameError> {
        let mut buf = Vec::new();
        self.encode(&mut buf)?;
        Ok(buf)
    }

    pub fn decode(frame: &'a [u8]) -> Result<Self, FrameError> {
        let kind = *frame.first().ok_or(FrameError::Empty)?;
        match kind {
            KIND_OUT | KIND_OUT_Z => {
                if frame.len() < OUT_HEADER {
                    return Err(FrameError::Short {
                        kind,
                        len: frame.len(),
                    });
                }
                let bytes = &frame[OUT_HEADER..];
                if bytes.len() > MAX_OUT_PAYLOAD {
                    return Err(FrameError::TooLarge {
                        len: bytes.len(),
                        max: MAX_OUT_PAYLOAD,
                    });
                }
                Ok(Data::Out {
                    tab: u32::from_be_bytes(frame[1..5].try_into().unwrap()),
                    ch: u16::from_be_bytes(frame[5..7].try_into().unwrap()),
                    epoch: u32::from_be_bytes(frame[7..11].try_into().unwrap()),
                    off: u64::from_be_bytes(frame[11..19].try_into().unwrap()),
                    compressed: kind == KIND_OUT_Z,
                    bytes,
                })
            }
            KIND_IN => {
                if frame.len() < IN_HEADER {
                    return Err(FrameError::Short {
                        kind,
                        len: frame.len(),
                    });
                }
                let bytes = &frame[IN_HEADER..];
                if bytes.len() > MAX_IN_PAYLOAD {
                    return Err(FrameError::TooLarge {
                        len: bytes.len(),
                        max: MAX_IN_PAYLOAD,
                    });
                }
                Ok(Data::In {
                    tab: u32::from_be_bytes(frame[1..5].try_into().unwrap()),
                    ch: u16::from_be_bytes(frame[5..7].try_into().unwrap()),
                    id: u32::from_be_bytes(frame[7..11].try_into().unwrap()),
                    bytes,
                })
            }
            other => Err(FrameError::UnknownKind(other)),
        }
    }
}

/// The room's routing for binary frames: reads bytes 1..5 without decoding
/// the rest.
pub fn peek_tab(frame: &[u8]) -> Option<Tab> {
    match *frame.first()? {
        KIND_OUT | KIND_OUT_Z | KIND_IN if frame.len() >= 5 => {
            Some(u32::from_be_bytes(frame[1..5].try_into().unwrap()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn out_layout() {
        let f = Data::Out {
            tab: 7,
            ch: 2,
            epoch: 0xAABBCCDD,
            off: 1042,
            compressed: false,
            bytes: b"hi",
        };
        let v = f.to_vec().unwrap();
        assert_eq!(
            v,
            [
                0x01, 0, 0, 0, 7, 0, 2, 0xAA, 0xBB, 0xCC, 0xDD, 0, 0, 0, 0, 0, 0, 0x04, 0x12, b'h',
                b'i'
            ]
        );
        assert_eq!(peek_tab(&v), Some(7));
    }

    #[test]
    fn rejects_bad_frames() {
        assert_eq!(Data::decode(&[]), Err(FrameError::Empty));
        assert_eq!(
            Data::decode(&[0x09, 0, 0]),
            Err(FrameError::UnknownKind(0x09))
        );
        assert_eq!(
            Data::decode(&[KIND_IN, 0, 0, 0]),
            Err(FrameError::Short {
                kind: KIND_IN,
                len: 4
            })
        );
        let big = vec![0u8; MAX_IN_PAYLOAD + 1];
        let f = Data::In {
            tab: 1,
            ch: 1,
            id: 1,
            bytes: &big,
        };
        assert!(matches!(f.to_vec(), Err(FrameError::TooLarge { .. })));
        assert_eq!(peek_tab(&[0x09, 0, 0, 0, 1]), None);
    }

    proptest! {
        #[test]
        fn out_round_trips(tab: u32, ch: u16, epoch: u32, off: u64, compressed: bool,
                           bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let f = Data::Out { tab, ch, epoch, off, compressed, bytes: &bytes };
            let v = f.to_vec().unwrap();
            prop_assert_eq!(Data::decode(&v).unwrap(), f);
            prop_assert_eq!(peek_tab(&v), Some(tab));
        }

        #[test]
        fn in_round_trips(tab: u32, ch: u16, id: u32,
                          bytes in proptest::collection::vec(any::<u8>(), 0..MAX_IN_PAYLOAD)) {
            let f = Data::In { tab, ch, id, bytes: &bytes };
            let v = f.to_vec().unwrap();
            prop_assert_eq!(Data::decode(&v).unwrap(), f);
        }

        #[test]
        fn decode_never_panics(frame in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = Data::decode(&frame);
            let _ = peek_tab(&frame);
        }
    }
}
