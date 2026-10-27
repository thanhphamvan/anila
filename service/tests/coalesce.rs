use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anila_proto::{Ch, Epoch, Tab};
use anila_service::coalesce::Coalescer;
use proptest::prelude::*;

const KEY: (Tab, Ch, Epoch) = (7, 1, 42);
const OTHER: (Tab, Ch, Epoch) = (7, 1, 43);
const WINDOW: Duration = Duration::from_millis(8);

fn ms(start: Instant, ms: u64) -> Instant {
    start + Duration::from_millis(ms)
}

/// A coalescer that has just sent a frame for `keys` at `start`, so their
/// next chunks are batched.
fn busy(max_bytes: usize, start: Instant, keys: &[(Tab, Ch, Epoch)]) -> Coalescer {
    let mut batch = Coalescer::new(WINDOW, max_bytes);
    for key in keys {
        assert_eq!(batch.push(*key, b"!", start), Some(b"!".to_vec()));
    }
    batch
}

#[test]
fn a_chunk_on_a_quiet_channel_goes_out_at_once() {
    let start = Instant::now();
    let mut batch = Coalescer::new(WINDOW, 32 << 10);
    assert_eq!(batch.push(KEY, b"k", start), Some(b"k".to_vec()));
    assert_eq!(batch.next_deadline(), None);
    assert!(batch.drain().is_empty());
}

#[test]
fn chunks_after_a_send_are_batched_from_their_first_byte() {
    let start = Instant::now();
    let mut batch = busy(32 << 10, start, &[KEY]);
    for (t, bytes) in [(2, b"one"), (4, b"two"), (7, b"end")] {
        assert_eq!(batch.push(KEY, bytes, ms(start, t)), None);
    }
    assert_eq!(batch.next_deadline(), Some(ms(start, 10)));
    assert!(batch.due(ms(start, 9)).is_empty());
    assert_eq!(batch.due(ms(start, 10)), vec![(KEY, b"onetwoend".to_vec())]);
    assert_eq!(batch.next_deadline(), None);
}

#[test]
fn a_channel_is_quiet_again_one_window_after_its_last_send() {
    let start = Instant::now();
    let mut batch = busy(32 << 10, start, &[KEY]);
    // Exactly one window after the last send: quiet.
    assert_eq!(batch.push(KEY, b"a", ms(start, 8)), Some(b"a".to_vec()));
    // A batched frame counts as a send when `due` returns it.
    assert_eq!(batch.push(KEY, b"b", ms(start, 9)), None);
    assert_eq!(batch.due(ms(start, 17)), vec![(KEY, b"b".to_vec())]);
    assert_eq!(
        batch.push(KEY, b"c", ms(start, 20)),
        None,
        "3 ms after a send"
    );
    assert_eq!(batch.due(ms(start, 28)), vec![(KEY, b"c".to_vec())]);
    assert_eq!(batch.push(KEY, b"d", ms(start, 36)), Some(b"d".to_vec()));
}

#[test]
fn size_threshold_flushes_entire_buffer() {
    let start = Instant::now();
    let mut batch = Coalescer::new(WINDOW, 32 << 10);
    let bytes = vec![42; 40 << 10];
    assert_eq!(batch.push(KEY, &bytes, start), Some(bytes));
    assert_eq!(batch.next_deadline(), None);
    assert!(batch.drain().is_empty());

    assert_eq!(batch.push(KEY, &vec![1; 16 << 10], start), None);
    let mut expected = vec![1; 16 << 10];
    expected.extend(vec![2; 16 << 10]);
    assert_eq!(batch.push(KEY, &vec![2; 16 << 10], start), Some(expected));
    assert!(batch.drain().is_empty());
    // A size flush is a send too.
    assert_eq!(batch.push(KEY, b"x", ms(start, 1)), None);
}

#[test]
fn keys_are_independent_and_due_in_arrival_order() {
    let start = Instant::now();
    let mut batch = Coalescer::new(WINDOW, 1024);
    assert_eq!(batch.push(KEY, b"k", start), Some(b"k".to_vec()));
    assert_eq!(
        batch.push(OTHER, b"o", ms(start, 1)),
        Some(b"o".to_vec()),
        "another key is still quiet"
    );
    // Reverse key order, with equal timestamps, must still preserve arrival order.
    batch.push(OTHER, b"first", ms(start, 2));
    batch.push(KEY, b"second", ms(start, 2));
    batch.push(OTHER, b"!", ms(start, 4));
    assert_eq!(batch.next_deadline(), Some(ms(start, 10)));
    assert_eq!(
        batch.due(ms(start, 10)),
        vec![(OTHER, b"first!".to_vec()), (KEY, b"second".to_vec())]
    );

    batch.push(KEY, b"a", ms(start, 11));
    batch.push(OTHER, b"b", ms(start, 13));
    assert_eq!(batch.due(ms(start, 19)), vec![(KEY, b"a".to_vec())]);
    assert_eq!(batch.next_deadline(), Some(ms(start, 21)));
}

#[test]
fn take_leaves_other_keys_and_drain_empties_all() {
    let start = Instant::now();
    let mut batch = busy(1024, start, &[KEY, OTHER]);
    batch.push(KEY, b"a", start);
    batch.push(OTHER, b"b", ms(start, 2));
    batch.push(KEY, b"c", ms(start, 3));
    assert_eq!(batch.take(KEY), Some(b"ac".to_vec()));
    assert_eq!(batch.take(KEY), None);
    assert_eq!(batch.next_deadline(), Some(ms(start, 10)));
    batch.push(KEY, b"d", ms(start, 4));
    assert_eq!(
        batch.drain(),
        vec![(OTHER, b"b".to_vec()), (KEY, b"d".to_vec())]
    );
    assert_eq!(batch.next_deadline(), None);
    assert!(batch.drain().is_empty());
}

#[test]
fn zero_window_flushes_every_push() {
    let mut batch = Coalescer::new(Duration::ZERO, 1024);
    for bytes in [b"a".as_slice(), b"bc", b""] {
        assert_eq!(batch.push(KEY, bytes, Instant::now()), Some(bytes.to_vec()));
        assert_eq!(batch.next_deadline(), None);
    }
    assert!(batch.drain().is_empty());
}

#[test]
fn empty_reads_send_nothing() {
    let start = Instant::now();
    let mut batch = Coalescer::new(WINDOW, 1024);
    assert_eq!(batch.push(KEY, b"", start), None);
    assert_eq!(batch.next_deadline(), None);
    assert_eq!(
        batch.push(KEY, b"k", ms(start, 1)),
        Some(b"k".to_vec()),
        "an empty read is not a send"
    );
}

#[test]
fn many_short_lived_channels_do_not_accumulate_state() {
    // Every key is sent once and never again; the coalescer must still answer
    // promptly (its bookkeeping of recent sends is pruned).
    let start = Instant::now();
    let mut batch = Coalescer::new(WINDOW, 1024);
    for i in 0..100_000u32 {
        let now = start + Duration::from_millis(u64::from(i));
        assert_eq!(batch.push((i, 1, 1), b"x", now), Some(b"x".to_vec()));
    }
    assert_eq!(batch.next_deadline(), None);
}

proptest! {
    #[test]
    fn arbitrary_pushes_and_due_preserve_every_keys_bytes(
        window_ms in 0u64..20,
        max_bytes in 0usize..128,
        operations in prop::collection::vec(
            (any::<bool>(), 0u32..3, 0u16..3, 0u32..3,
             prop::collection::vec(any::<u8>(), 0..100), 0u64..12),
            0..200,
        ),
    ) {
        let mut batch = Coalescer::new(Duration::from_millis(window_ms), max_bytes);
        let mut now = Instant::now();
        let mut expected = BTreeMap::<_, Vec<u8>>::new();
        let mut actual = BTreeMap::<_, Vec<u8>>::new();
        for (push, tab, ch, epoch, bytes, elapsed) in operations {
            now += Duration::from_millis(elapsed);
            if push {
                let key = (tab, ch, epoch);
                expected.entry(key).or_default().extend_from_slice(&bytes);
                actual.entry(key).or_default();
                if let Some(flushed) = batch.push(key, &bytes, now) {
                    actual.entry(key).or_default().extend(flushed);
                }
            } else {
                for (key, bytes) in batch.due(now) {
                    actual.entry(key).or_default().extend(bytes);
                }
            }
        }
        for (key, bytes) in batch.drain() {
            actual.entry(key).or_default().extend(bytes);
        }
        prop_assert_eq!(actual, expected);
        prop_assert_eq!(batch.next_deadline(), None);
    }

    /// While a key is busy, frames go out at most once per window plus the
    /// size flushes: a burst cannot turn into a frame per read.
    #[test]
    fn a_burst_sends_at_most_one_frame_per_window(
        gaps in prop::collection::vec(0u64..3, 1..400),
    ) {
        let start = Instant::now();
        let mut batch = Coalescer::new(WINDOW, 1 << 20);
        let mut now = start;
        let mut frames = 0u64;
        for gap in gaps {
            now += Duration::from_millis(gap);
            frames += u64::from(batch.push(KEY, b"x", now).is_some());
            frames += batch.due(now).len() as u64;
        }
        frames += batch.drain().len() as u64;
        let span = now.duration_since(start).as_millis() as u64;
        prop_assert!(frames <= span / 8 + 2, "{frames} frames in {span} ms");
    }
}
