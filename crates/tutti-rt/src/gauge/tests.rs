use super::{Count, Counter, CounterWriter, Gauge, GaugeWriter, counter, gauge};

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

#[test]
fn a_gauge_reads_the_latest_value_written() {
    let (writer, reading) = gauge(1.0f32);
    assert_eq!(reading.get(), 1.0, "before any write: the initial value");
    writer.set(-4.5);
    writer.set(2.5);
    assert_eq!(reading.get(), 2.5);
    assert_eq!(reading.clone().get(), 2.5, "a clone reads the same cell");
}

#[test]
fn a_gauge_moves_both_ways() {
    let (writer, reading) = gauge(0.0f32);
    for level in [3.0, -3.0, 0.5, -9.0] {
        writer.set(level);
        assert_eq!(reading.get(), level);
    }
}

#[test]
fn a_gauge_holds_its_last_value_when_the_writer_drops() {
    let (writer, reading) = gauge([7u32, 9]);
    assert!(!reading.is_closed());
    writer.set([1, 2]);
    drop(writer);
    assert!(reading.is_closed());
    assert_eq!(reading.get(), [1, 2]);
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
    send::<GaugeWriter<f32>>();
    send::<CounterWriter<u64>>();
    send_sync::<Gauge<f32>>();
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
