//! The timer driver, run for real: a thread that sleeps to each block's
//! deadline, through the same `OutputStream` a sound card's stream is.
//!
//! The schedule's arithmetic (deadlines, owed blocks, the re-anchor) is tested
//! against a scripted clock in `src/timer/tests.rs`; these tests hold what only
//! a running thread shows: whole blocks reach the engine, the block clock is
//! dated by deadlines, the latency reads zero, the stop joins, and a drop on a
//! driving thread returns.

use std::sync::Arc;
use std::time::Duration;
use tutti_platform::{Executor, Instant, join_within};

use crate::fixtures::{SAMPLE_RATE, fan, graph_engine};
use tutti_buffers::ChannelLayout;
use tutti_buffers::SampleFormat;
use tutti_engine::{
    AudioCallbackState, BlockClockReader, BlockReading, OutputQuantum, OutputSpec, OutputStream,
    TimerDriver,
};
use tutti_graph::NodeKey;
use tutti_nodes::testing::Const;
use tutti_timeline::Transport;
use tutti_units::SampleRate;

fn timer_state() -> (Arc<AudioCallbackState>, BlockClockReader) {
    let engine = graph_engine(&Transport::new(SampleRate::of(SAMPLE_RATE)), |ed| {
        ed.insert(NodeKey(1), "dc", Const::mono(0.5));
        fan(ed, NodeKey(1), 2);
    });
    let clock = engine.block_clock();
    (Arc::new(AudioCallbackState::new(engine)), clock)
}

fn spec(quantum: Option<usize>) -> OutputSpec {
    let spec = OutputSpec::new(
        SampleRate::of(SAMPLE_RATE),
        ChannelLayout::STEREO,
        SampleFormat::F32,
    );
    quantum.map_or(spec.clone(), |q| spec.with_quantum(OutputQuantum::of(q)))
}

/// Reads the block clock until `done` accepts a reading, or fails after ten
/// seconds, so a stream that never renders fails instead of hanging.
fn wait_for(clock: &BlockClockReader, done: impl Fn(&BlockReading) -> bool) -> BlockReading {
    let limit = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(reading) = clock.read().filter(&done) {
            return reading;
        }
        assert!(Instant::now() < limit, "the timer rendered too few blocks");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// **Every block is a whole quantum.** The block clock's frame at the start of
/// a render call is a multiple of the spec's quantum, so no call was split or
/// skipped, and the stream's latency reads zero.
#[test]
fn the_timer_renders_whole_quantum_blocks_and_reports_zero_latency() {
    const QUANTUM: usize = 480;
    let (state, clock) = timer_state();
    let executor = Executor::new();
    let mut stream = OutputStream::new(spec(Some(QUANTUM)));
    stream
        .start_with(state, TimerDriver::new(&executor))
        .expect("a thread starts");
    let reading = wait_for(&clock, |r| r.frame.get() >= 5 * QUANTUM as u64);
    assert_eq!(reading.frame.get() % QUANTUM as u64, 0);
    assert_eq!(stream.latency().get(), Some(Duration::ZERO));
    stream.stop();
}

/// Without a quantum the block is the driver's default, and the open stream's
/// spec says so: the timer reports the size it runs at, and a stop returns the
/// request.
#[test]
fn a_spec_with_no_quantum_renders_and_reports_the_default_block() {
    let block = tutti_engine::DEFAULT_BLOCK.get() as u64;
    let (state, clock) = timer_state();
    let executor = Executor::new();
    let mut stream = OutputStream::new(spec(None));
    stream
        .start_with(state, TimerDriver::new(&executor))
        .expect("a thread starts");
    assert_eq!(stream.spec().quantum, Some(tutti_engine::DEFAULT_BLOCK));
    let reading = wait_for(&clock, |r| r.frame.get() >= 3 * block);
    assert_eq!(reading.frame.get() % block, 0);
    stream.stop();
    assert_eq!(stream.spec().quantum, None, "the request after a stop");
}

/// **The block clock reads deadlines.** Between two readings taken with no
/// re-anchor between them, the OS time advanced by exactly the frames' span at
/// the rate (to the nanosecond the two roundings can differ by), whatever the
/// wake-ups were: a reading dated by the wake-up would carry the scheduler's
/// jitter.
#[test]
fn the_block_clock_is_dated_by_deadlines_not_wake_ups() {
    const QUANTUM: usize = 480;
    let (state, clock) = timer_state();
    let executor = Executor::new();
    let mut stream = OutputStream::new(spec(Some(QUANTUM)));
    let faults = stream.faults();
    stream
        .start_with(state, TimerDriver::new(&executor))
        .expect("a thread starts");

    // A re-anchor counts a fault and then writes the reading after it. A
    // reading not seen at a read was written after that read began, so the
    // count loaded before that read is a floor for the count at its write; the
    // count loaded after the read that sees a reading is a ceiling for it.
    // A pair is unbroken when the first reading's floor equals the count after
    // the second's read: the count held across both writes.
    let mut checked = 0;
    // The latest reading and its floor; `None` until a reading is first seen
    // after an earlier read, which is what gives it a floor.
    let mut latest: Option<(BlockReading, u64)> = None;
    // The last reading read, and the count loaded before that read.
    let mut seen: Option<(BlockReading, u64)> = None;
    let limit = Instant::now() + Duration::from_secs(10);
    while checked < 5 && Instant::now() < limit {
        let before = faults.count();
        let Some(reading) = clock.read() else {
            continue;
        };
        let after = faults.count();
        if let Some((last_seen, floor)) = seen
            && reading.frame != last_seen.frame
        {
            if let Some((last, last_floor)) = latest
                && last_floor == after
            {
                let frames = (reading.frame.get() - last.frame.get()) as f64;
                let expected = (frames / SAMPLE_RATE * 1e9).round() as i64;
                let got = reading.os_ns - last.os_ns;
                assert!(
                    (got - expected).abs() <= 1,
                    "{got} ns between readings, the schedule says {expected} ns"
                );
                checked += 1;
            }
            latest = Some((reading, floor));
        }
        seen = Some((reading, before));
        std::thread::sleep(Duration::from_millis(3));
    }
    assert!(checked >= 5, "too few unbroken pairs of readings");
    stream.stop();
}

/// **A stop joins the thread:** once it returns no block renders.
#[test]
fn stopping_the_timer_ends_its_blocks() {
    let (state, clock) = timer_state();
    let executor = Executor::new();
    let mut stream = OutputStream::new(spec(Some(240)));
    stream
        .start_with(state, TimerDriver::new(&executor))
        .expect("a thread starts");
    wait_for(&clock, |r| r.frame.get() > 0);
    stream.stop();
    let stopped_at = clock.read().expect("it rendered").frame;
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(clock.read().expect("it rendered").frame, stopped_at);
    assert!(!stream.is_running());
}

/// **A drop on a driving thread returns:** a stream dropped inside a future on a
/// dedicated executor thread stops its loop without joining it, where a join
/// would panic.
#[test]
fn dropping_the_timer_on_a_driving_thread_returns_without_joining() {
    let (state, clock) = timer_state();
    let executor = Executor::new();
    let mut stream = OutputStream::new(spec(Some(240)));
    stream
        .start_with(state, TimerDriver::new(&executor))
        .expect("a thread starts");
    wait_for(&clock, |r| r.frame.get() > 0);
    let dropper = executor
        .spawn_dedicated("drop-timer", None, async move { drop(stream) })
        .expect("a thread starts");
    // A hang guard only: the drop returns at once.
    assert!(
        matches!(join_within(dropper, Duration::from_secs(10)), Ok(Some(()))),
        "the drop on a driving thread returns"
    );
}
