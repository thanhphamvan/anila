//! Pure, per-channel batching of PTY output with immediate sends after quiet.
//! A key with nothing pending sends immediately after at least one window
//! without a send; subsequent chunks batch from their first byte.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use anila_proto::{Ch, Epoch, Tab};

type Key = (Tab, Ch, Epoch);

struct Pending {
    key: Key,
    first: Instant,
    bytes: Vec<u8>,
}

/// Sends PTY output immediately when a (tab, ch, epoch) has nothing pending
/// and has not sent for at least `window` (or has never sent). Subsequent
/// reads batch for up to `window` from their first byte, or until `max_bytes`.
/// Only bytes returned by `push` and `due` record a send; `take` and `drain`
/// leave send history unchanged.
pub struct Coalescer {
    window: Duration,
    max_bytes: usize,
    pending: Vec<Pending>,
    last_sent: BTreeMap<Key, Instant>,
    sent_by_time: BTreeSet<(Instant, Key)>,
}

impl Coalescer {
    /// `window` of zero disables batching (every push is due at once).
    /// `max_bytes` flushes a key as soon as it holds that much.
    pub fn new(window: Duration, max_bytes: usize) -> Self {
        Self {
            window,
            max_bytes,
            pending: Vec::new(),
            last_sent: BTreeMap::new(),
            sent_by_time: BTreeSet::new(),
        }
    }

    /// Adds bytes for `key` at time `now`, returning them immediately when
    /// key is quiet, the size threshold is reached, or the window is zero.
    pub fn push(&mut self, key: Key, bytes: &[u8], now: Instant) -> Option<Vec<u8>> {
        if self.window.is_zero() {
            return Some(bytes.to_vec());
        }
        self.prune_sent(now);
        if bytes.is_empty() {
            return None;
        }
        let index = match self.pending.iter().position(|entry| entry.key == key) {
            Some(index) => index,
            None => {
                if !self.last_sent.contains_key(&key) {
                    self.record_send(key, now);
                    return Some(bytes.to_vec());
                }
                self.pending.push(Pending {
                    key,
                    first: now,
                    bytes: Vec::new(),
                });
                self.pending.len() - 1
            }
        };
        self.pending[index].bytes.extend_from_slice(bytes);
        if self.pending[index].bytes.len() >= self.max_bytes {
            self.record_send(key, now);
            Some(self.pending.remove(index).bytes)
        } else {
            None
        }
    }

    /// Removes every key whose first byte is at least `window` old,
    /// in arrival order of its first byte.
    pub fn due(&mut self, now: Instant) -> Vec<(Key, Vec<u8>)> {
        self.prune_sent(now);
        let mut due = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            if now.saturating_duration_since(self.pending[index].first) >= self.window {
                let entry = self.pending.remove(index);
                self.record_send(entry.key, now);
                due.push((entry.key, entry.bytes));
            } else {
                index += 1;
            }
        }
        due
    }

    fn record_send(&mut self, key: Key, now: Instant) {
        if let Some(previous) = self.last_sent.insert(key, now) {
            self.sent_by_time.remove(&(previous, key));
        }
        self.sent_by_time.insert((now, key));
    }

    fn prune_sent(&mut self, now: Instant) {
        // Expire only old entries, without scanning every recently active key.
        while let Some(&(sent, key)) = self.sent_by_time.first() {
            if now.saturating_duration_since(sent) < self.window {
                break;
            }
            self.sent_by_time.pop_first();
            self.last_sent.remove(&key);
        }
    }

    /// When the oldest pending key becomes due, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .iter()
            .map(|entry| entry.first + self.window)
            .min()
    }

    /// Removes everything for one key, for flushing before its exit.
    pub fn take(&mut self, key: Key) -> Option<Vec<u8>> {
        let index = self.pending.iter().position(|entry| entry.key == key)?;
        Some(self.pending.remove(index).bytes)
    }

    /// Removes everything in first-byte arrival order, for shutdown or link loss.
    pub fn drain(&mut self) -> Vec<(Key, Vec<u8>)> {
        self.pending
            .drain(..)
            .map(|entry| (entry.key, entry.bytes))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_send_indexes_expire_and_replace_entries() {
        let start = Instant::now();
        let window = Duration::from_millis(8);
        let mut batch = Coalescer::new(window, 1);
        let key = (1, 1, 1);
        for i in 0..100 {
            batch.push(key, b"x", start + Duration::from_micros(i));
            assert_eq!(batch.last_sent.len(), 1);
            assert_eq!(batch.sent_by_time.len(), 1);
        }
        batch.due(start + window + Duration::from_micros(99));
        assert!(batch.last_sent.is_empty());
        assert!(batch.sent_by_time.is_empty());

        for i in 0..100_000 {
            batch.push((i, 1, 1), b"x", start + Duration::from_millis(i.into()));
            assert!(batch.last_sent.len() <= 8);
            assert!(batch.sent_by_time.len() <= 8);
        }
    }

    #[test]
    fn take_and_drain_do_not_record_sends() {
        let start = Instant::now();
        let window = Duration::from_millis(8);
        for drain in [false, true] {
            let mut batch = Coalescer::new(window, 1024);
            let key = (1, 1, 1);
            assert_eq!(batch.push(key, b"a", start), Some(b"a".to_vec()));
            assert_eq!(batch.push(key, b"b", start + window / 2), None);
            if drain {
                assert_eq!(batch.drain(), vec![(key, b"b".to_vec())]);
            } else {
                assert_eq!(batch.take(key), Some(b"b".to_vec()));
            }
            assert_eq!(batch.last_sent.get(&key), Some(&start));
            assert_eq!(batch.push(key, b"c", start + window), Some(b"c".to_vec()));
        }
    }
}
