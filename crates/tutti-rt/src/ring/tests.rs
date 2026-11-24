use super::*;

use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};

/// **A `DropNewest` ring holds exactly `capacity` values**: the next push is
/// handed back, counted, and nothing queued is overwritten.
#[test]
fn drop_newest_refuses_the_pushed_value_and_counts_it() {
    let (mut writer, mut reader) = ring::<u32>(2, Overflow::DropNewest);
    writer.push(1).unwrap();
    writer.push(2).unwrap();
    let back = writer.push(3).unwrap_err();
    assert_eq!((back.error, back.value), (PushError::Full, 3));
    assert_eq!(writer.lost(), 1);
    assert_eq!(reader.len(), 2);
    assert_eq!(reader.pop(), Some(1));
    assert_eq!(reader.pop(), Some(2));
    assert_eq!(reader.pop(), None);
}

/// **A `Refuse` ring hands the value back and counts nothing**: the loss is
/// the caller's to decide.
#[test]
fn refuse_hands_the_value_back_without_counting() {
    let (mut writer, reader) = ring::<u32>(1, Overflow::Refuse);
    writer.push(1).unwrap();
    assert_eq!(writer.push(2).unwrap_err().value, 2);
    assert_eq!(writer.lost(), 0);
    assert_eq!(reader.lost(), 0);
}

/// **Values come out in order across many laps**, for a ring of three.
#[test]
fn values_wrap_around_in_order() {
    let (mut writer, mut reader) = ring::<u64>(3, Overflow::Refuse);
    let (mut next, mut expect) = (0, 0);
    for lap in 0..50 {
        for _ in 0..(lap % 3) + 1 {
            writer.push(next).unwrap();
            next += 1;
        }
        while let Some(v) = reader.pop() {
            assert_eq!(v, expect);
            expect += 1;
        }
    }
    assert_eq!(expect, next);
}

/// **`lost` is cumulative**, the same at each end, and a view outlives both
/// ends.
#[test]
fn lost_accumulates_at_both_ends() {
    let (mut writer, reader) = ring::<u8>(1, Overflow::DropNewest);
    let view = writer.losses();
    writer.push(0).unwrap();
    let _ = writer.push(1);
    let _ = writer.push(2);
    assert_eq!((writer.lost(), reader.lost()), (2, 2));
    let _ = writer.push(3);
    assert_eq!((writer.lost(), reader.lost()), (3, 3));
    drop((writer, reader));
    assert_eq!(view.get(), 3, "cumulative, after both ends are gone");
}

/// **A dropped reader disconnects pushes, and the value comes back**; a
/// dropped writer is seen by the reader after what it pushed.
#[test]
fn a_dropped_end_is_seen_by_the_other() {
    let (mut writer, reader) = ring::<u8>(2, Overflow::DropNewest);
    drop(reader);
    assert!(writer.is_disconnected());
    let back = writer.push(7).unwrap_err();
    assert_eq!((back.error, back.value), (PushError::Disconnected, 7));
    assert_eq!(writer.lost(), 0, "a gone reader is not a loss");

    let (mut writer, mut reader) = ring::<u8>(2, Overflow::DropNewest);
    writer.push(1).unwrap();
    drop(writer);
    assert!(reader.is_disconnected());
    assert_eq!(reader.pop(), Some(1), "what was pushed is still there");
    assert_eq!(reader.pop(), None);
}

/// **A zero-capacity ring refuses everything and holds nothing.**
#[test]
fn a_zero_capacity_ring_is_always_full() {
    let (mut writer, mut reader) = ring::<u8>(0, Overflow::Refuse);
    assert!(writer.is_full());
    assert_eq!(writer.push(1).unwrap_err().error, PushError::Full);
    assert_eq!(reader.pop(), None);
    assert!(reader.iter().next().is_none());
}

/// **The slice helpers move whole runs in order**, report the count, and
/// count the values that did not fit on a `DropNewest` ring.
#[test]
fn slices_move_in_order() {
    let (mut writer, mut reader) = ring::<f32>(4, Overflow::DropNewest);
    assert_eq!(writer.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 4);
    assert_eq!(writer.lost(), 2);
    assert_eq!(writer.vacant(), 0);
    let mut out = [0.0; 3];
    assert_eq!(reader.pop_slice(&mut out), 3);
    assert_eq!(out, [1.0, 2.0, 3.0]);
    assert_eq!(
        writer.push_slice(&[7.0, 8.0, 9.0]),
        3,
        "room freed by the pop"
    );
    let mut rest = [0.0; 8];
    assert_eq!(reader.pop_slice(&mut rest), 4);
    assert_eq!(rest[..4], [4.0, 7.0, 8.0, 9.0], "across the wrap");
    assert!(reader.is_empty());
}

/// **`iter` and `peek` read without taking**, oldest first, and `drain` takes
/// only what was queued when it began.
#[test]
fn iter_and_peek_leave_values_queued_and_drain_is_bounded() {
    let (mut writer, mut reader) = ring::<u32>(4, Overflow::Refuse);
    for n in 1..=3 {
        writer.push(n).unwrap();
    }
    assert_eq!(reader.peek(), Some(&1));
    assert_eq!(reader.iter().copied().collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(reader.len(), 3);
    assert_eq!(reader.drain().next(), Some(1));
    assert_eq!(reader.len(), 2, "an unread value stays");

    let mut drain = reader.drain();
    writer.push(4).unwrap();
    assert_eq!(
        drain.by_ref().count(),
        2,
        "the push after the drain began is not taken"
    );
    assert_eq!(reader.pop(), Some(4));
}

/// **Queued values drop with the ring**, none leaked and none twice.
#[test]
fn queued_values_drop_with_the_ring() {
    let live = std::sync::Arc::new(());
    let (mut writer, reader) = ring::<std::sync::Arc<()>>(4, Overflow::Refuse);
    for _ in 0..3 {
        writer.push(live.clone()).unwrap();
    }
    assert_eq!(std::sync::Arc::strong_count(&live), 4);
    drop((writer, reader));
    assert_eq!(std::sync::Arc::strong_count(&live), 1);
}

/// **The writer's drop frees nothing**: queued values wait for the reader,
/// which is where they are freed.
#[test]
fn the_writer_drop_leaves_queued_values_to_the_reader() {
    let live = std::sync::Arc::new(());
    let (mut writer, mut reader) = ring::<std::sync::Arc<()>>(4, Overflow::Refuse);
    writer.push(live.clone()).unwrap();
    drop(writer);
    assert_eq!(std::sync::Arc::strong_count(&live), 2, "still queued");
    assert!(reader.pop().is_some());
}

/// **Freeing queued values inside an audio block is a debug panic**, so an
/// audio-thread end is caught being dropped where it must not be.
#[test]
#[cfg(debug_assertions)]
fn dropping_a_reader_with_queued_values_inside_a_block_panics() {
    let (mut writer, reader) = ring::<Box<u8>>(2, Overflow::Refuse);
    writer.push(Box::new(1)).unwrap();
    let guard = crate::RtScope::enter();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(reader)));
    drop(guard);
    assert!(result.is_err());
}

fn two_threads<W: Send + 'static, R>(
    push: impl FnOnce(u32) -> W + Send + 'static,
    pop: impl Fn(&mut R) -> Option<u32>,
    reader: R,
    done: impl Fn(&R) -> bool,
) -> (Vec<u32>, W) {
    let producer = std::thread::spawn(move || push(PUSHES));
    let mut reader = reader;
    let mut got = Vec::new();
    loop {
        let gone = done(&reader);
        match pop(&mut reader) {
            Some(n) => got.push(n),
            None if gone => break,
            None => std::thread::yield_now(),
        }
    }
    (got, producer.join().unwrap())
}

const PUSHES: u32 = if cfg!(miri) { 200 } else { 20_000 };

/// **Across two threads every accepted value arrives once, in order**, on a
/// `DropNewest` ring whose losses plus arrivals are the pushes.
#[test]
fn two_threads_lose_nothing_unaccounted() {
    let (mut writer, reader) = ring::<u32>(8, Overflow::DropNewest);
    let (got, lost) = two_threads(
        move |pushes| {
            for n in 0..pushes {
                let _ = writer.push(n);
            }
            writer.lost()
        },
        |r: &mut RingReader<u32>| r.pop(),
        reader,
        RingReader::is_disconnected,
    );
    assert!(
        got.windows(2).all(|w| w[0] < w[1]),
        "order kept, none repeated"
    );
    assert_eq!(got.len() as u64 + lost, u64::from(PUSHES));
}

/// **Bulk runs across threads arrive whole and in order**: `push_slice` and
/// `pop_slice` with one publish each.
#[test]
fn bulk_runs_across_threads_keep_order() {
    let (mut writer, mut reader) = ring::<u32>(16, Overflow::Refuse);
    let producer = std::thread::spawn(move || {
        let mut next = 0u32;
        while next < PUSHES {
            let run: Vec<u32> = (next..(next + 5).min(PUSHES)).collect();
            next += writer.push_slice(&run) as u32;
        }
    });
    let mut expect = 0u32;
    let mut out = [0u32; 7];
    while expect < PUSHES {
        let n = reader.pop_slice(&mut out);
        for v in &out[..n] {
            assert_eq!(*v, expect);
            expect += 1;
        }
        if n == 0 {
            std::thread::yield_now();
        }
    }
    producer.join().unwrap();
}

/// **A reader dropped while the writer pushes frees every value exactly
/// once**, whichever side finds it queued.
#[test]
fn a_reader_dropped_during_pushes_leaves_no_value_behind() {
    let rounds = if cfg!(miri) { 20 } else { 5_000 };
    for _ in 0..rounds {
        let token = std::sync::Arc::new(());
        let values: Vec<_> = (0..6).map(|_| token.clone()).collect();
        let (mut writer, mut reader) = ring::<std::sync::Arc<()>>(2, Overflow::DropNewest);
        let producer = std::thread::spawn(move || {
            for value in values {
                let _ = writer.push(value);
            }
        });
        let first = reader.pop();
        drop(reader);
        producer.join().unwrap();
        drop(first);
        assert_eq!(std::sync::Arc::strong_count(&token), 1);
    }
}

/// **A value whose drop panics is dropped exactly once**: the reader's drop
/// gives up each slot before dropping its value, so the unwind leaves the
/// slots after it for the ring's own drop and takes none twice.
#[test]
fn a_panicking_drop_in_the_readers_drop_double_drops_nothing() {
    static DROPS: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
    struct Value(usize);
    impl Drop for Value {
        fn drop(&mut self) {
            DROPS[self.0].fetch_add(1, StdOrdering::SeqCst);
            assert!(self.0 != 1, "the second value's drop panics");
        }
    }
    let (mut writer, reader) = ring::<Value>(4, Overflow::Refuse);
    for n in 0..4 {
        writer.push(Value(n)).unwrap();
    }
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(reader)));
    assert!(unwound.is_err(), "the drop of the second value panicked");
    drop(writer);
    let counts: Vec<usize> = DROPS.iter().map(|d| d.load(StdOrdering::SeqCst)).collect();
    assert_eq!(counts, [1, 1, 1, 1], "each value dropped exactly once");
}

/// **A panic in `push_iter`'s iterator publishes what was written**, so the
/// reader sees the values and none is lost.
#[test]
fn a_panicking_iterator_in_push_iter_publishes_what_it_wrote() {
    let (mut writer, mut reader) = ring::<u32>(8, Overflow::Refuse);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        writer.push_iter((0..5).inspect(|n| {
            assert!(*n < 3, "the iterator panics at 3");
        }));
    }));
    assert!(unwound.is_err());
    assert_eq!(reader.drain().collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(
        writer.push_iter([7, 8]),
        2,
        "the writer carries on from there"
    );
    assert_eq!(reader.drain().collect::<Vec<_>>(), [7, 8]);
}

/// **A panic in `pop_each`'s closure leaves the handed values taken**: the
/// next pop starts after them.
#[test]
fn a_panicking_closure_in_pop_each_leaves_the_handed_values_taken() {
    let (mut writer, mut reader) = ring::<u32>(8, Overflow::Refuse);
    writer.push_slice(&[0, 1, 2, 3]);
    let mut seen = Vec::new();
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reader.pop_each(4, |n| {
            seen.push(n);
            assert!(n < 1, "the closure panics at 1");
        });
    }));
    assert!(unwound.is_err());
    assert_eq!(seen, [0, 1]);
    assert_eq!(reader.drain().collect::<Vec<_>>(), [2, 3]);
}
