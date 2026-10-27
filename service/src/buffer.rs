//! Per-channel replay buffer, addressed by byte offset within an epoch.

use std::collections::VecDeque;

use anila_proto::Off;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayError {
    /// The requested offset has already been dropped. Answer with `reset`.
    TooOld { from: Off, start: Off },
    /// The requested offset was never produced. The browser is confused.
    Ahead { from: Off, end: Off },
}

/// Keeps the newest `cap` bytes of a PTY's output.
pub struct ReplayBuffer {
    bytes: VecDeque<u8>,
    /// Offset of `bytes[0]`.
    start: Off,
    cap: usize,
}

impl ReplayBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            start: 0,
            cap,
        }
    }

    /// Offset of the oldest byte still held.
    pub fn start(&self) -> Off {
        self.start
    }

    /// Offset one past the newest byte.
    pub fn end(&self) -> Off {
        self.start + self.bytes.len() as Off
    }

    pub fn push(&mut self, data: &[u8]) {
        let data = if data.len() > self.cap {
            self.start += (self.bytes.len() + data.len() - self.cap) as Off;
            self.bytes.clear();
            &data[data.len() - self.cap..]
        } else {
            let overflow = (self.bytes.len() + data.len()).saturating_sub(self.cap);
            self.bytes.drain(..overflow);
            self.start += overflow as Off;
            data
        };
        self.bytes.extend(data);
    }

    /// Up to `max` bytes starting at `from`.
    pub fn read_from(&self, from: Off, max: usize) -> Result<Vec<u8>, ReplayError> {
        if from < self.start {
            return Err(ReplayError::TooOld {
                from,
                start: self.start,
            });
        }
        if from > self.end() {
            return Err(ReplayError::Ahead {
                from,
                end: self.end(),
            });
        }
        let skip = (from - self.start) as usize;
        Ok(self.bytes.range(skip..).take(max).copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn keeps_newest_bytes() {
        let mut b = ReplayBuffer::new(4);
        b.push(b"abc");
        assert_eq!((b.start(), b.end()), (0, 3));
        b.push(b"de");
        assert_eq!((b.start(), b.end()), (1, 5));
        assert_eq!(b.read_from(1, 10).unwrap(), b"bcde");
        assert_eq!(b.read_from(3, 1).unwrap(), b"d");
        assert_eq!(b.read_from(5, 10).unwrap(), b"");
        assert_eq!(
            b.read_from(0, 1),
            Err(ReplayError::TooOld { from: 0, start: 1 })
        );
        assert_eq!(
            b.read_from(6, 1),
            Err(ReplayError::Ahead { from: 6, end: 5 })
        );
        b.push(b"0123456789");
        assert_eq!((b.start(), b.end()), (11, 15));
        assert_eq!(b.read_from(11, 10).unwrap(), b"6789");
    }

    proptest! {
        #[test]
        fn matches_tail_of_stream(cap in 1usize..64,
                                  chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..100), 0..20)) {
            let mut b = ReplayBuffer::new(cap);
            let mut all = Vec::new();
            for c in &chunks {
                b.push(c);
                all.extend_from_slice(c);
            }
            prop_assert_eq!(b.end(), all.len() as Off);
            prop_assert!(b.end() - b.start() <= cap as Off);
            let tail = b.read_from(b.start(), usize::MAX).unwrap();
            prop_assert_eq!(&tail[..], &all[b.start() as usize..]);
        }
    }
}
