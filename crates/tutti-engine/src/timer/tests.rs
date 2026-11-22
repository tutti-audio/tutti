use tutti_units::SampleRate;

use super::overran;
use super::pacer::{Due, MAX_LAG_BLOCKS, Pacer};
use crate::faults::{StreamFaultKind, StreamFaults};

const MS: i64 = 1_000_000;

/// 480 frames at 48 kHz: ten milliseconds.
fn pacer_at(now_ns: i64) -> Pacer {
    Pacer::new(now_ns, 480, SampleRate::SR_48K)
}

/// Plays the timer's loop against a clock the test moves: waiting jumps it to the
/// deadline plus `late(deadline)`, the lateness of that wake-up. Returns the
/// deadlines of the first `blocks` blocks rendered and how many times the
/// schedule re-anchored.
fn blocks_on(
    pacer: &mut Pacer,
    blocks: usize,
    mut late: impl FnMut(i64) -> i64,
) -> (Vec<i64>, usize) {
    let mut now = 0;
    let mut rendered = Vec::new();
    let mut overruns = 0;
    while rendered.len() < blocks {
        match pacer.poll(now) {
            Due::Wait(until_ns) => now = until_ns + late(until_ns),
            Due::Render(os_ns) => rendered.push(os_ns),
            Due::Reanchored => overruns += 1,
        }
    }
    (rendered, overruns)
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
    let mut pacer = pacer_at(0);
    let (rendered, overruns) = blocks_on(&mut pacer, 200, |_| 3 * MS);
    assert_eq!(overruns, 0);
    assert_eq!(rendered.len(), 200);
    for (n, os_ns) in rendered.iter().enumerate() {
        assert_eq!(*os_ns, n as i64 * 10 * MS, "block {n}");
    }
}

/// A stall of many blocks re-anchors exactly once, and the blocks after it are
/// paced from the new anchor.
#[test]
fn a_stall_re_anchors_once_and_paces_from_the_new_anchor() {
    let mut pacer = pacer_at(0);
    let mut stalled = false;
    // The first sleep past block 5 wakes a second late.
    let (rendered, overruns) = blocks_on(&mut pacer, 30, |deadline| {
        if !stalled && deadline >= 50 * MS {
            stalled = true;
            1_000 * MS
        } else {
            0
        }
    });
    assert_eq!(overruns, 1, "one stall, one re-anchor");
    // Block spacing is unbroken before the stall and after the re-anchor; the
    // jump is the single place it is not.
    let gaps: Vec<i64> = rendered.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(gaps.iter().filter(|g| **g != 10 * MS).count(), 1);
    assert!(gaps.iter().all(|g| *g >= 10 * MS));
}

/// A re-anchor reaches the stream's faults as one overrun, and an overrun does
/// not end the stream.
#[test]
fn a_re_anchor_records_one_overrun_and_does_not_disconnect() {
    let faults = StreamFaults::new();
    overran(&faults);
    assert_eq!(faults.count(), 1);
    assert_eq!(
        faults.last().map(|f| f.kind),
        Some(StreamFaultKind::Overrun)
    );
    assert!(!faults.is_disconnected());
}
