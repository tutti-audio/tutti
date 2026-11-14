use tutti_types::{Db, SamplePosition, Samples};

use super::{Count, Counter, CounterWriter, Gauge, GaugeWriter, Reading, counter, gauge};

/// A count of dropped events, as an owner would name one.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Dropped(u64);

impl Count for Dropped {
    fn count(self) -> u64 {
        self.0
    }
    fn from_count(count: u64) -> Self {
        Self(count)
    }
}

/// An owner's own reading: two halves of one word.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pair([u32; 2]);

impl Reading for Pair {
    fn to_bits(self) -> u64 {
        u64::from(self.0[0]) << 32 | u64::from(self.0[1])
    }
    fn from_bits(bits: u64) -> Self {
        Self([(bits >> 32) as u32, bits as u32])
    }
}

#[test]
fn a_gauge_holds_a_unit() {
    let (writer, reduction) = gauge(Db(0.0));
    writer.set(Db(-12.5));
    assert_eq!(reduction.get(), Db(-12.5));
    let (writer, cursor) = gauge(SamplePosition(0.0));
    writer.set(SamplePosition(48_000.25));
    assert_eq!(
        cursor.get(),
        SamplePosition(48_000.25),
        "an f64 unit keeps its width"
    );
}

#[test]
fn an_owners_own_reading_round_trips() {
    let (writer, state) = gauge(Pair([0, 0]));
    writer.set(Pair([u32::MAX, 1]));
    assert_eq!(state.get(), Pair([u32::MAX, 1]));
}

#[test]
fn a_counter_counts_frames() {
    let (writer, lost) = counter::<Samples>();
    writer.add(Samples(512));
    writer.add(Samples(64));
    assert_eq!(lost.get(), Samples(576));
}

#[test]
fn a_gauge_reads_the_latest_value_written() {
    let (writer, reading) = gauge(Db(1.0));
    assert_eq!(
        reading.get(),
        Db(1.0),
        "before any write: the initial value"
    );
    writer.set(Db(-4.5));
    writer.set(Db(2.5));
    assert_eq!(reading.get(), Db(2.5));
    assert_eq!(
        reading.clone().get(),
        Db(2.5),
        "a clone reads the same cell"
    );
}

#[test]
fn a_gauge_moves_both_ways() {
    let (writer, reading) = gauge(Db(0.0));
    for level in [Db(3.0), Db(-3.0), Db(0.5), Db(-9.0)] {
        writer.set(level);
        assert_eq!(reading.get(), level);
    }
}

#[test]
fn a_gauge_holds_its_last_value_when_the_writer_drops() {
    let (writer, reading) = gauge(Pair([7, 9]));
    assert!(!reading.is_closed());
    writer.set(Pair([1, 2]));
    drop(writer);
    assert!(reading.is_closed());
    assert_eq!(reading.get(), Pair([1, 2]));
}

#[test]
fn a_counter_never_decreases() {
    let (writer, lost) = counter::<Dropped>();
    let mut last = lost.get().0;
    for by in [0, 1, 0, 7, 2] {
        writer.add(Dropped(by));
        let now = lost.get().0;
        assert!(now >= last, "{now} fell below {last}");
        assert_eq!(now, last + by);
        last = now;
    }
    assert_eq!(last, 10);
}

#[test]
fn a_counter_read_takes_nothing() {
    let (writer, lost) = counter::<u64>();
    writer.add(4);
    assert_eq!((lost.get(), lost.get()), (4, 4));
}

#[test]
fn a_counter_saturates_instead_of_wrapping() {
    let (writer, lost) = counter::<u64>();
    writer.add(u64::MAX - 1);
    writer.add(5);
    assert_eq!(lost.get(), u64::MAX);
}

#[test]
fn a_counter_is_final_when_the_writer_drops() {
    let (writer, lost) = counter::<u64>();
    writer.add(3);
    assert!(!lost.is_closed());
    drop(writer);
    assert!(lost.is_closed());
    assert_eq!(lost.get(), 3);
}

/// The writers move to the audio thread and the readers are shared.
const _: () = {
    const fn send<T: Send>() {}
    const fn send_sync<T: Send + Sync>() {}
    send::<GaugeWriter<Db>>();
    send::<CounterWriter<u64>>();
    send_sync::<Gauge<Db>>();
    send_sync::<Counter<u64>>();
};

#[test]
fn a_reader_on_another_thread_sees_a_final_count() {
    let (writer, lost) = counter::<u64>();
    let reader = std::thread::spawn(move || {
        while !lost.is_closed() {
            std::thread::yield_now();
        }
        lost.get()
    });
    for _ in 0..1_000 {
        writer.add(1);
    }
    drop(writer);
    assert_eq!(reader.join().unwrap(), 1_000);
}
