use super::*;

/// The close, waiting for a helper still inside the job, yields the
/// caller's core once its budget passes with `entered` unchanged: what
/// lets a helper queued behind a `SCHED_FIFO` caller on one core leave
/// at all (`src/spin.rs`). The caller's clock is a stepped one (a
/// quarter of the budget per read), and the helper stays inside until it
/// sees the caller's yield counted, so no timing is involved; the 10 s
/// bound is only how a missing yield fails rather than hangs. Linux
/// only, as the yield is.
#[test]
fn the_close_yields_while_a_helper_is_inside() {
    use std::time::{Duration, Instant};
    static YIELDED: AtomicU32 = AtomicU32::new(0);
    let gate = JobGate::new(2);
    let inside = AtomicBool::new(false);
    let saw = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut served = 0;
            while !stop.load(Ordering::Acquire) && !gate.help(&mut served) {
                std::hint::spin_loop();
            }
        });
        crate::spin::tests::yielding::count_yields_into(&YIELDED);
        crate::spin::tests::step_clock(BlockWaits::DEFAULT.caller_yield_after / 4);
        let joined = gate.run(
            &|i| {
                let deadline = Instant::now() + Duration::from_secs(10);
                if i == 0 {
                    // Hold participant 0 until the helper is inside, so
                    // the close has a helper to wait for.
                    while !inside.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                } else {
                    inside.store(true, Ordering::Release);
                    while YIELDED.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                    saw.store(YIELDED.load(Ordering::Acquire) > 0, Ordering::Relaxed);
                }
            },
            || {},
        );
        assert!(joined);
        stop.store(true, Ordering::Release);
    });
    assert!(inside.load(Ordering::Relaxed), "the helper joined");
    assert!(
        saw.load(Ordering::Relaxed),
        "the close never yielded while the helper was inside"
    );
}

/// The close yields after the gate's own budget, not the default: with
/// a budget of an hour and its clock stepping the default budget per
/// read, it waits 50 ms on a helper inside without yielding. Linux only,
/// as the yield is.
#[test]
fn the_close_yields_after_the_gates_budget() {
    use std::time::Duration;
    static YIELDED: AtomicU32 = AtomicU32::new(0);
    let waits = BlockWaits {
        caller_yield_after: Duration::from_secs(3600),
        ..BlockWaits::DEFAULT
    };
    let gate = JobGate::with_waits(2, waits);
    assert_eq!(gate.waits(), waits);
    let inside = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut served = 0;
            while !stop.load(Ordering::Acquire) && !gate.help(&mut served) {
                std::hint::spin_loop();
            }
        });
        crate::spin::tests::yielding::count_yields_into(&YIELDED);
        crate::spin::tests::step_clock(BlockWaits::DEFAULT.caller_yield_after);
        let joined = gate.run(
            &|i| {
                if i == 0 {
                    // Hold participant 0 until the helper is inside, so
                    // the close has a helper to wait for.
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while !inside.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                } else {
                    inside.store(true, Ordering::Release);
                    std::thread::sleep(Duration::from_millis(50));
                }
            },
            || {},
        );
        assert!(joined);
        stop.store(true, Ordering::Release);
    });
    assert!(inside.load(Ordering::Relaxed), "the helper joined");
    assert_eq!(YIELDED.load(Ordering::Acquire), 0);
}
