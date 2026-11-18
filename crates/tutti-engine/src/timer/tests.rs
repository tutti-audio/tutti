use std::cell::{Cell, RefCell};

use tutti_rt::StopToken;
use tutti_units::SampleRate;

use super::pacer::{Due, MAX_LAG_BLOCKS, Pacer, Timebase};
use super::run;
use crate::faults::{StreamFaultKind, StreamFaults};

const MS: i64 = 1_000_000;

/// 480 frames at 48 kHz: ten milliseconds.
fn pacer_at(now_ns: i64) -> Pacer {
    Pacer::new(now_ns, 480, SampleRate::SR_48K)
}

/// A scripted clock: sleeping moves it to the deadline plus a late wake-up.
struct Scripted {
    now: Cell<i64>,
    wake_late: Cell<i64>,
}

impl Scripted {
    fn at(now: i64) -> Self {
        Self {
            now: Cell::new(now),
            wake_late: Cell::new(0),
        }
    }
}

impl Timebase for Scripted {
    fn now_ns(&self) -> i64 {
        self.now.get()
    }

    fn sleep_until_ns(&self, deadline_ns: i64) {
        self.now.set(deadline_ns + self.wake_late.get());
    }
}

/// Runs the loop until `blocks` have rendered; returns the deadlines it was
/// handed and how many times it re-anchored.
fn blocks_on(time: &Scripted, pacer: &mut Pacer, blocks: usize) -> (Vec<i64>, usize) {
    let stop = StopToken::new();
    let rendered = RefCell::new(Vec::new());
    let overruns = Cell::new(0);
    run(
        time,
        pacer,
        &stop,
        |os_ns| {
            rendered.borrow_mut().push(os_ns);
            if rendered.borrow().len() == blocks {
                stop.stop();
            }
        },
        || overruns.set(overruns.get() + 1),
    );
    (rendered.into_inner(), overruns.get())
}

/// Block n is due at start + n blocks - one block, with start one block after
/// the schedule began: block 0 now, block 3 thirty milliseconds on.
#[test]
fn block_n_is_due_at_start_plus_n_blocks_minus_one_block() {
    let pacer = pacer_at(5_000 * MS);
    assert_eq!(pacer.deadline_ns(0), 5_000 * MS);
    assert_eq!(pacer.deadline_ns(1), 5_010 * MS);
    assert_eq!(pacer.deadline_ns(3), 5_030 * MS);
}

/// A rate that does not divide a second into whole blocks: after exactly one
/// second of blocks the deadline is exactly one second on, not a rounding of
/// each block summed (512 frames at 44.1 kHz is 11 609 977.2 ns, rounded).
#[test]
fn a_deadline_is_computed_from_the_start_so_rounding_does_not_accumulate() {
    let pacer = Pacer::new(0, 512, SampleRate::SR_44K1);
    // 44 100 blocks of 512 frames are 512 seconds.
    assert_eq!(
        pacer.deadline_ns(44_101),
        pacer.deadline_ns(1) + 512_000 * MS
    );
}

/// Before its deadline the answer is to sleep until it, and nothing renders.
#[test]
fn a_block_not_yet_due_waits_for_its_deadline() {
    let mut pacer = pacer_at(0);
    assert_eq!(pacer.poll(-3 * MS), Due::Wait(0));
    assert_eq!(pacer.poll(-1), Due::Wait(0));
    assert_eq!(pacer.poll(0), Due::Render(0));
    assert_eq!(pacer.poll(1), Due::Wait(10 * MS));
}

/// A wake-up three and a half blocks late renders the four blocks it owes,
/// back to back and in order, each dated by its own deadline.
#[test]
fn a_late_wake_up_renders_every_owed_block_back_to_back() {
    let mut pacer = pacer_at(0);
    let now = 35 * MS;
    let polls: Vec<Due> = (0..5).map(|_| pacer.poll(now)).collect();
    assert_eq!(
        polls,
        [
            Due::Render(0),
            Due::Render(10 * MS),
            Due::Render(20 * MS),
            Due::Render(30 * MS),
            Due::Wait(40 * MS),
        ]
    );
}

/// Exactly four blocks late still renders; one nanosecond past re-anchors.
#[test]
fn only_more_than_four_blocks_behind_re_anchors() {
    let four = MAX_LAG_BLOCKS * 10 * MS;
    assert_eq!(pacer_at(0).poll(four), Due::Render(0));
    assert_eq!(pacer_at(0).poll(four + 1), Due::Reanchored);
}

/// After a re-anchor the schedule runs from the present: the next block is due
/// at that instant and the ones after at block spacing from it.
#[test]
fn a_re_anchored_schedule_runs_from_the_present() {
    let mut pacer = pacer_at(0);
    let now = 500 * MS;
    assert_eq!(pacer.poll(now), Due::Reanchored);
    assert_eq!(pacer.poll(now), Due::Render(now));
    assert_eq!(pacer.poll(now), Due::Wait(now + 10 * MS));
}

/// Every block renders once, in order, and none is skipped: the deadlines the
/// loop hands over are consecutive blocks apart, however late each wake-up.
#[test]
fn every_block_renders_in_order_with_none_skipped() {
    let time = Scripted::at(0);
    time.wake_late.set(3 * MS);
    let mut pacer = pacer_at(0);
    let (rendered, overruns) = blocks_on(&time, &mut pacer, 200);
    assert_eq!(overruns, 0);
    assert_eq!(rendered.len(), 200);
    for (n, os_ns) in rendered.iter().enumerate() {
        assert_eq!(*os_ns, n as i64 * 10 * MS, "block {n}");
    }
}

/// The os time a block carries is its deadline, not the wake-up that ran it:
/// with every wake-up 3 ms late, the blocks are still dated on the schedule.
#[test]
fn a_blocks_os_time_is_its_deadline_not_its_wake_up() {
    let time = Scripted::at(0);
    time.wake_late.set(3 * MS);
    let mut pacer = pacer_at(0);
    let stop = StopToken::new();
    let seen = RefCell::new(Vec::<i64>::new());
    run(
        &time,
        &mut pacer,
        &stop,
        |os_ns| {
            seen.borrow_mut().push(os_ns);
            if seen.borrow().len() == 20 {
                stop.stop();
            }
        },
        || {},
    );
    for (n, os_ns) in seen.into_inner().into_iter().enumerate() {
        assert_eq!(
            os_ns,
            n as i64 * 10 * MS,
            "block {n} is dated by its deadline"
        );
    }
}

/// A stall of many blocks re-anchors exactly once, the blocks after it are
/// paced from the new anchor, and one overrun reaches the stream's faults.
#[test]
fn a_stall_re_anchors_once_and_records_exactly_one_fault() {
    struct Stalling {
        inner: Scripted,
        stalled: Cell<bool>,
    }
    impl Timebase for Stalling {
        fn now_ns(&self) -> i64 {
            self.inner.now_ns()
        }
        fn sleep_until_ns(&self, deadline_ns: i64) {
            // The first sleep past block 5 wakes a second late.
            if !self.stalled.get() && deadline_ns >= 50 * MS {
                self.stalled.set(true);
                self.inner.wake_late.set(1_000 * MS);
            } else {
                self.inner.wake_late.set(0);
            }
            self.inner.sleep_until_ns(deadline_ns);
        }
    }
    let time = Stalling {
        inner: Scripted::at(0),
        stalled: Cell::new(false),
    };
    let faults = StreamFaults::new();
    let stop = StopToken::new();
    let rendered = RefCell::new(Vec::<i64>::new());
    let mut pacer = pacer_at(0);
    run(
        &time,
        &mut pacer,
        &stop,
        |os_ns| {
            rendered.borrow_mut().push(os_ns);
            if rendered.borrow().len() == 30 {
                stop.stop();
            }
        },
        || faults.record_overrun("late"),
    );
    assert_eq!(faults.count(), 1, "one stall, one fault");
    assert_eq!(
        faults.last().map(|f| f.kind),
        Some(StreamFaultKind::Overrun)
    );
    assert!(
        !faults.is_disconnected(),
        "an overrun does not end the stream"
    );
    let rendered = rendered.into_inner();
    // Block spacing is unbroken before the stall and after the re-anchor; the
    // jump is the single place it is not.
    let gaps: Vec<i64> = rendered.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(gaps.iter().filter(|g| **g != 10 * MS).count(), 1);
    assert!(gaps.iter().all(|g| *g >= 10 * MS));
}
