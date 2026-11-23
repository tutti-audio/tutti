use tutti_units::{Db, Losses, SamplePosition, Samples};

use super::{Count, Counter, CounterWriter, Gauge, GaugeWriter, Reading, counter, gauge, tally};

/// A count of dropped events, as an owner would name one.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Dropped(u64);

impl Count for Dropped {
    type Words = [u64; 1];
    fn to_words(self) -> [u64; 1] {
        [self.0]
    }
    fn from_words([count]: [u64; 1]) -> Self {
        Self(count)
    }
}

/// An owner's two-tally count.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Two {
    lost: u64,
    late: u64,
}

impl Count for Two {
    type Words = [u64; 2];
    fn to_words(self) -> [u64; 2] {
        [self.lost, self.late]
    }
    fn from_words([lost, late]: [u64; 2]) -> Self {
        Self { lost, late }
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
    let (mut writer, reduction) = gauge(Db::new(0.0));
    writer.set(Db::new(-12.5));
    assert_eq!(reduction.get(), Db::new(-12.5));
    let (mut writer, cursor) = gauge(SamplePosition::new(0.0));
    writer.set(SamplePosition::new(48_000.25));
    assert_eq!(
        cursor.get(),
        SamplePosition::new(48_000.25),
        "an f64 unit keeps its width"
    );
}

#[test]
fn an_owners_own_reading_round_trips() {
    let (mut writer, state) = gauge(Pair([0, 0]));
    writer.set(Pair([u32::MAX, 1]));
    assert_eq!(state.get(), Pair([u32::MAX, 1]));
}

#[test]
fn a_counter_counts_frames() {
    let (mut writer, lost) = counter::<Samples>();
    writer.add(Samples::new(512));
    writer.add(Samples::new(64));
    assert_eq!(lost.get(), Samples::new(576));
}

#[test]
fn a_gauge_reads_the_latest_value_written() {
    let (mut writer, reading) = gauge(Db::new(1.0));
    assert_eq!(
        reading.get(),
        Db::new(1.0),
        "before any write: the initial value"
    );
    writer.set(Db::new(-4.5));
    writer.set(Db::new(2.5));
    assert_eq!(reading.get(), Db::new(2.5));
    assert_eq!(
        reading.clone().get(),
        Db::new(2.5),
        "a clone reads the same cell"
    );
}

#[test]
fn a_gauge_moves_both_ways() {
    let (mut writer, reading) = gauge(Db::new(0.0));
    for level in [Db::new(3.0), Db::new(-3.0), Db::new(0.5), Db::new(-9.0)] {
        writer.set(level);
        assert_eq!(reading.get(), level);
    }
}

#[test]
fn a_gauge_holds_its_last_value_when_the_writer_drops() {
    let (mut writer, reading) = gauge(Pair([7, 9]));
    assert!(!reading.is_closed());
    writer.set(Pair([1, 2]));
    drop(writer);
    assert!(reading.is_closed());
    assert_eq!(reading.get(), Pair([1, 2]));
}

#[test]
fn a_counter_never_decreases() {
    let (mut writer, lost) = counter::<Dropped>();
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
    let (mut writer, lost) = counter::<u64>();
    writer.add(4);
    assert_eq!((lost.get(), lost.get()), (4, 4));
}

/// **A reader's take is what was counted since its own baseline, per tally,
/// and leaves the total and every other reader's baseline alone.**
#[test]
fn take_since_answers_each_tallys_growth_and_leaves_the_total() {
    let (mut writer, a) = counter::<Two>();
    let b = a.clone();
    let (mut seen_a, mut seen_b) = (Two { lost: 0, late: 0 }, Two { lost: 0, late: 0 });
    writer.add(Two { lost: 3, late: 1 });
    assert_eq!(a.take_since(&mut seen_a), Two { lost: 3, late: 1 });
    writer.add(Two { lost: 2, late: 0 });
    assert_eq!(a.take_since(&mut seen_a), Two { lost: 2, late: 0 });
    assert_eq!(a.take_since(&mut seen_a), Two { lost: 0, late: 0 });
    // `b` took nothing yet: it sees everything, and `a`'s takes moved no total.
    assert_eq!(b.take_since(&mut seen_b), Two { lost: 5, late: 1 });
    assert_eq!(a.get(), Two { lost: 5, late: 1 });
    // A baseline ahead of the total (one from another counter) takes 0, never wraps.
    let mut ahead = Two { lost: 9, late: 0 };
    assert_eq!(a.take_since(&mut ahead), Two { lost: 0, late: 1 });
    assert_eq!(ahead, Two { lost: 5, late: 1 });
}

#[test]
fn a_counter_saturates_instead_of_wrapping() {
    let (mut writer, lost) = counter::<u64>();
    writer.add(u64::MAX - 1);
    writer.add(5);
    assert_eq!(lost.get(), u64::MAX);
}

#[test]
fn a_counter_is_final_when_the_writer_drops() {
    let (mut writer, lost) = counter::<u64>();
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
    let (mut writer, lost) = counter::<u64>();
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

#[test]
fn a_cell_reopens_only_once_its_writer_has_dropped() {
    let (writer, total) = counter::<u64>();
    assert!(total.reopen().is_none(), "a live writer is the only one");
    drop(writer);
    assert!(total.is_closed());
    let again = total.reopen().expect("the writer dropped");
    assert!(!total.is_closed());
    assert!(total.reopen().is_none(), "one writer at a time");
    drop(again);
    assert!(total.is_closed());
    assert!(total.reopen().is_some(), "and again after that one drops");
}

#[test]
fn a_counter_stays_cumulative_across_writers() {
    let (mut first, total) = counter::<Samples>();
    first.add(Samples::new(5));
    drop(first);
    let mut second = total.reopen().unwrap();
    second.add(Samples::new(7));
    assert_eq!(total.get(), Samples::new(12));
}

#[test]
fn a_gauge_holds_its_last_reading_across_writers() {
    let (mut first, level) = gauge(Db::new(0.0));
    first.set(Db::new(-9.0));
    drop(first);
    let mut second = level.reopen().unwrap();
    assert_eq!(level.get(), Db::new(-9.0));
    second.set(Db::new(-2.0));
    assert_eq!(level.get(), Db::new(-2.0));
}

#[test]
fn a_two_tally_counter_adds_each_tally_on_its_own() {
    let (mut writer, total) = counter::<Two>();
    writer.add(Two { lost: 1, late: 10 });
    writer.add(Two { lost: 2, late: 0 });
    assert_eq!(total.get(), Two { lost: 3, late: 10 });
}

#[test]
fn a_tally_saturates_without_carrying_into_the_next() {
    let (mut writer, total) = counter::<Two>();
    writer.add(Two {
        lost: u64::MAX - 1,
        late: 4,
    });
    writer.add(Two { lost: 5, late: 1 });
    assert_eq!(
        total.get(),
        Two {
            lost: u64::MAX,
            late: 5
        },
        "the full tally stops at the top and the other goes on"
    );
}

#[test]
fn a_two_tally_counter_stays_cumulative_across_writers() {
    let (mut first, total) = counter::<Two>();
    first.add(Two { lost: 2, late: 3 });
    drop(first);
    assert!(total.is_closed());
    let mut second = total.reopen().unwrap();
    second.add(Two { lost: 1, late: 1 });
    assert_eq!(total.get(), Two { lost: 3, late: 4 });
}

#[test]
fn a_reader_of_a_two_tally_counter_never_sees_a_tally_go_back() {
    let (mut writer, total) = counter::<Two>();
    let reader = std::thread::spawn(move || {
        let mut last = Two { lost: 0, late: 0 };
        while !total.is_closed() {
            let now = total.get();
            assert!(now.lost >= last.lost && now.late >= last.late);
            last = now;
        }
        total.get()
    });
    for _ in 0..10_000 {
        writer.add(Two { lost: 1, late: 2 });
    }
    drop(writer);
    assert_eq!(
        reader.join().unwrap(),
        Two {
            lost: 10_000,
            late: 20_000
        }
    );
}

#[test]
fn two_tally_writers_add_to_one_exact_total() {
    let total = tally::<u64>();
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let writer = total.writer();
            std::thread::spawn(move || {
                for _ in 0..50_000 {
                    writer.add(1);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(total.get(), 100_000, "an add was lost to the other writer");
}

#[test]
fn a_tally_adds_each_tally_of_a_two_tally_count() {
    let total = tally::<Two>();
    let a = total.writer();
    let b = a.clone();
    a.add(Two { lost: 1, late: 10 });
    b.add(Two { lost: 2, late: 0 });
    assert_eq!(total.clone().get(), Two { lost: 3, late: 10 });
}

#[test]
fn a_tally_reader_never_sees_a_tally_go_back() {
    let total = tally::<Two>();
    let writers = [total.writer(), total.writer()];
    let reader = std::thread::spawn({
        let total = total.clone();
        move || {
            let mut last = Two { lost: 0, late: 0 };
            for _ in 0..10_000 {
                let now = total.get();
                assert!(now.lost >= last.lost && now.late >= last.late);
                last = now;
            }
        }
    });
    for _ in 0..10_000 {
        for w in &writers {
            w.add(Two { lost: 1, late: 2 });
        }
    }
    reader.join().unwrap();
    assert_eq!(
        total.get(),
        Two {
            lost: 20_000,
            late: 40_000
        }
    );
}

#[test]
fn a_tally_outlives_every_writer_and_mints_more() {
    let total = tally::<u64>();
    let first = total.writer();
    first.add(2);
    drop(first);
    total.writer().add(3);
    assert_eq!(total.get(), 5);
}

#[test]
fn a_tally_writer_reads_its_own_total_and_mints_a_reader_of_it() {
    let lost = crate::tally::<u64>().writer();
    lost.add(4);
    let report = lost.tally();
    lost.add(3);
    assert_eq!(lost.get(), 7);
    assert_eq!(report.get(), 7, "the minted reader reads the same cell");
}

/// A counter of losses adds field by field, saturating each on its own,
/// and carries on across a reopened writer.
#[test]
fn a_losses_counter_tallies_each_field() {
    let (mut writer, total) = counter::<Losses>();
    writer.add(Losses::lost(2) + Losses::late(1));
    writer.add(Losses::refused(4) + Losses::lost(1));
    assert_eq!(
        total.get(),
        Losses::lost(3) + Losses::refused(4) + Losses::late(1)
    );
    writer.add(Losses::evicted(u64::MAX));
    writer.add(Losses::evicted(1));
    assert_eq!(total.get().evicted, u64::MAX);
    assert_eq!(total.get().lost, 3, "a full field does not carry");
    drop(writer);
    let mut again = total.reopen().expect("the writer dropped");
    again.add(Losses::refused(1));
    assert_eq!(total.get().refused, 5);
    assert_eq!(total.get().lost, 3);
}
