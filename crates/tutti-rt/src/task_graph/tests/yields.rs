use super::*;

/// The caller, with nothing to take while a helper holds the only task
/// in flight, yields its core once its budget passes with no task
/// finished: what lets a helper queued behind a `SCHED_FIFO` caller on
/// one core run at all (`src/spin.rs`). The caller's clock is a
/// stepped one (a quarter of the budget per read), and the helper holds
/// task 0 until it sees the caller's yield counted, so no timing is
/// involved; the 10 s bound is only how a missing yield fails rather
/// than hangs. Linux only, as the yield is.
#[test]
fn the_caller_yields_while_a_helper_holds_a_task() {
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    static YIELDED: AtomicU32 = AtomicU32::new(0);
    // 0 → 1: while the helper holds 0, the caller has nothing to take.
    let mut g = TaskGraph::new(&[0, 1], &[0, 1, 1], &[1]);
    g.begin();
    let g = &g;
    let (held_tx, held_rx) = mpsc::channel();
    let saw_yield = std::thread::scope(|s| {
        let helper = s.spawn(move || {
            let mut saw = false;
            g.work(Participant::Helper(1), BlockWaits::DEFAULT, |t| {
                if t != 0 {
                    return;
                }
                held_tx.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while YIELDED.load(std::sync::atomic::Ordering::Acquire) == 0 {
                    if Instant::now() > deadline {
                        return;
                    }
                    std::thread::yield_now();
                }
                saw = true;
            });
            saw
        });
        held_rx.recv().unwrap();
        crate::spin::tests::yielding::count_yields_into(&YIELDED);
        crate::spin::tests::step_clock(BlockWaits::DEFAULT.caller_yield_after / 4);
        g.work(Participant::Caller, BlockWaits::DEFAULT, |t| {
            panic!("the caller took task {t}, which the helper holds or runs inline")
        });
        helper.join().unwrap()
    });
    assert!(
        saw_yield,
        "the caller never yielded while the helper held task 0"
    );
    assert!(g.done());
}

/// The caller yields after the budget it is given, not the default: with
/// a budget of an hour and its clock stepping the default budget per
/// read, it reads the clock `READS` times waiting on a helper holding
/// task 0 without yielding. The helper holds the task until it sees the
/// reads counted; the 10 s bound is only how a caller that never waits
/// fails rather than hangs. Linux only, as the yield is.
#[test]
fn the_caller_yields_after_the_pools_budget() {
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    static YIELDED: AtomicU32 = AtomicU32::new(0);
    static READ: AtomicU32 = AtomicU32::new(0);
    const READS: u32 = 8;
    let waits = BlockWaits {
        caller_yield_after: Duration::from_secs(3600),
        ..BlockWaits::DEFAULT
    };
    let mut g = TaskGraph::new(&[0, 1], &[0, 1, 1], &[1]);
    g.begin();
    let g = &g;
    let (held_tx, held_rx) = mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(move || {
            g.work(Participant::Helper(1), waits, |t| {
                if t == 0 {
                    held_tx.send(()).unwrap();
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while READ.load(std::sync::atomic::Ordering::Acquire) < READS
                        && Instant::now() < deadline
                    {
                        std::thread::yield_now();
                    }
                }
            });
        });
        held_rx.recv().unwrap();
        crate::spin::tests::yielding::count_yields_into(&YIELDED);
        crate::spin::tests::count_clock_reads_into(&READ);
        crate::spin::tests::step_clock(BlockWaits::DEFAULT.caller_yield_after);
        g.work(Participant::Caller, waits, |t| {
            panic!("the caller took task {t}, which the helper holds or runs inline")
        });
    });
    assert!(g.done());
    assert!(READ.load(std::sync::atomic::Ordering::Acquire) >= READS);
    assert_eq!(YIELDED.load(std::sync::atomic::Ordering::Acquire), 0);
}
