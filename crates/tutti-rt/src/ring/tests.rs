use super::*;

use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};

/// **A `DropNewest` ring holds exactly `capacity` values**: the next push is
/// handed back, counted, and nothing queued is overwritten.
#[test]
fn drop_newest_refuses_the_pushed_value_and_counts_it() {
    let (mut writer, mut reader) = Ring::<u32>::pair(2, Overflow::DropNewest);
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
    let (mut writer, reader) = Ring::<u32>::pair(1, Overflow::Refuse);
    writer.push(1).unwrap();
    assert_eq!(writer.push(2).unwrap_err().value, 2);
    assert_eq!(writer.lost(), 0);
    assert_eq!(reader.lost(), 0);
}

/// **An `Evict` ring keeps the newest `capacity` values in order** and counts
/// one loss per value it dropped.
#[test]
fn evict_keeps_the_newest_values_in_order() {
    let (mut writer, mut reader) = Ring::<u32>::pair(3, Overflow::Evict);
    for n in 0..10 {
        writer.push(n).unwrap();
    }
    assert_eq!(writer.lost(), 7);
    assert_eq!(reader.drain().collect::<Vec<_>>(), [7, 8, 9]);
    writer.push(10).unwrap();
    assert_eq!(reader.pop(), Some(10));
}

/// **Evicting drops the oldest value on the pushing thread, exactly once**:
/// the dropped values are the oldest ones.
#[test]
fn evict_drops_each_evicted_value_once() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    struct Counted(u32);
    impl Drop for Counted {
        fn drop(&mut self) {
            DROPS.fetch_add(1, StdOrdering::Relaxed);
        }
    }
    let (mut writer, mut reader) = Ring::<Counted>::pair(2, Overflow::Evict);
    for n in 0..5 {
        writer.push(Counted(n)).unwrap();
    }
    assert_eq!(DROPS.load(StdOrdering::Relaxed), 3);
    assert_eq!(reader.pop().map(|c| c.0), Some(3));
    drop((writer, reader));
    assert_eq!(
        DROPS.load(StdOrdering::Relaxed),
        5,
        "the queued one frees with the ring"
    );
}

/// **Values come out in order across many laps**, for a ring of three.
#[test]
fn values_wrap_around_in_order() {
    let (mut writer, mut reader) = Ring::<u64>::pair(3, Overflow::Refuse);
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

/// **`take_lost` resets and `lost` is cumulative**, per end, and a view
/// outlives both ends.
#[test]
fn take_lost_resets_and_lost_accumulates() {
    let (mut writer, mut reader) = Ring::<u8>::pair(1, Overflow::DropNewest);
    let view = writer.losses();
    writer.push(0).unwrap();
    let _ = writer.push(1);
    let _ = writer.push(2);
    assert_eq!(reader.take_lost(), 2);
    assert_eq!(reader.take_lost(), 0);
    let _ = writer.push(3);
    assert_eq!(reader.take_lost(), 1);
    assert_eq!(writer.take_lost(), 3, "each end's reset is its own");
    drop((writer, reader));
    assert_eq!(view.get(), 3, "cumulative, after both ends are gone");
}

/// **A dropped reader disconnects pushes, and the value comes back**; a
/// dropped writer is seen by the reader after what it pushed.
#[test]
fn a_dropped_end_is_seen_by_the_other() {
    let (mut writer, reader) = Ring::<u8>::pair(2, Overflow::DropNewest);
    drop(reader);
    assert!(writer.is_disconnected());
    let back = writer.push(7).unwrap_err();
    assert_eq!((back.error, back.value), (PushError::Disconnected, 7));
    assert_eq!(writer.lost(), 0, "a gone reader is not a loss");

    let (mut writer, mut reader) = Ring::<u8>::pair(2, Overflow::DropNewest);
    writer.push(1).unwrap();
    drop(writer);
    assert!(reader.is_disconnected());
    assert_eq!(reader.pop(), Some(1), "what was pushed is still there");
    assert_eq!(reader.pop(), None);
}

/// **A zero-capacity ring refuses everything and holds nothing.**
#[test]
fn a_zero_capacity_ring_is_always_full() {
    let (mut writer, mut reader) = Ring::<u8>::pair(0, Overflow::Refuse);
    assert!(writer.is_full());
    assert_eq!(writer.push(1).unwrap_err().error, PushError::Full);
    assert_eq!(reader.pop(), None);
    assert!(reader.iter().next().is_none());
}

/// **The slice helpers move whole runs in order** and report the count.
#[test]
fn slices_move_in_order() {
    let (mut writer, mut reader) = Ring::<f32>::pair(4, Overflow::Refuse);
    assert_eq!(writer.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 4);
    assert_eq!(writer.vacant(), 0);
    let mut out = [0.0; 3];
    assert_eq!(reader.pop_slice(&mut out), 3);
    assert_eq!(out, [1.0, 2.0, 3.0]);
    assert_eq!(reader.skip(5), 1);
    assert!(reader.is_empty());
}

/// **`iter` and `peek` read without taking**, oldest first, and `drain` takes
/// only what was queued when it began.
#[test]
fn iter_and_peek_leave_values_queued_and_drain_is_bounded() {
    let (mut writer, mut reader) = Ring::<u32>::pair(4, Overflow::Refuse);
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

/// **A borrow of an evicting ring's values is refused**: its writer may take
/// them.
#[test]
#[should_panic(expected = "evicting")]
fn an_evicting_ring_cannot_be_borrowed() {
    let (_writer, reader) = Ring::<u32>::pair(2, Overflow::Evict);
    let _ = reader.peek();
}

/// **Queued values drop with the ring**, none leaked and none twice.
#[test]
fn queued_values_drop_with_the_ring() {
    let live = std::sync::Arc::new(());
    let (mut writer, reader) = Ring::<std::sync::Arc<()>>::pair(4, Overflow::Refuse);
    for _ in 0..3 {
        writer.push(live.clone()).unwrap();
    }
    assert_eq!(std::sync::Arc::strong_count(&live), 4);
    drop((writer, reader));
    assert_eq!(std::sync::Arc::strong_count(&live), 1);
}

/// **Across two threads every accepted value arrives once, in order**, on a
/// `DropNewest` ring whose losses plus arrivals are the pushes.
#[test]
fn two_threads_lose_nothing_unaccounted() {
    const PUSHES: u32 = if cfg!(miri) { 200 } else { 20_000 };
    let (mut writer, mut reader) = Ring::<u32>::pair(8, Overflow::DropNewest);
    let producer = std::thread::spawn(move || {
        for n in 0..PUSHES {
            let _ = writer.push(n);
        }
        writer.lost()
    });
    let mut got = Vec::new();
    loop {
        let gone = reader.is_disconnected();
        match reader.pop() {
            Some(n) => got.push(n),
            None if gone => break,
            None => std::thread::yield_now(),
        }
    }
    let lost = producer.join().unwrap();
    assert!(
        got.windows(2).all(|w| w[0] < w[1]),
        "order kept, none repeated"
    );
    assert_eq!(got.len() as u64 + lost, u64::from(PUSHES));
}

/// **An evicting ring under a racing reader never repeats or reorders a
/// value**, and what it lost plus what arrived is what was pushed.
#[test]
fn an_evicting_ring_with_a_reader_keeps_order_and_counts() {
    const PUSHES: u32 = if cfg!(miri) { 200 } else { 20_000 };
    let (mut writer, mut reader) = Ring::<u32>::pair(4, Overflow::Evict);
    let producer = std::thread::spawn(move || {
        for n in 0..PUSHES {
            let _ = writer.push(n);
        }
        writer.lost()
    });
    let mut got = Vec::new();
    loop {
        let gone = reader.is_disconnected();
        match reader.pop() {
            Some(n) => got.push(n),
            None if gone => break,
            None => std::thread::yield_now(),
        }
    }
    let lost = producer.join().unwrap();
    assert!(
        got.windows(2).all(|w| w[0] < w[1]),
        "order kept, none repeated"
    );
    assert_eq!(got.len() as u64 + lost, u64::from(PUSHES));
}
