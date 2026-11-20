//! The block clock: each render call publishes the OS time it began and the
//! graph frame it began at, so a producer thread holding an OS time can place
//! it on a frame. Figures are
//! analytic: the frame of call `k` is the frames rendered before it.

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_buffers::MaxBlock;
use tutti_engine::{BlockDates, Engine};
use tutti_graph::{Editor, Prepare};
use tutti_timeline::MotionCommand;
use tutti_timeline::Transport;
use tutti_units::{Beat, Seconds};
use tutti_units::{Frame, SampleRate, Samples};

const RATE: f64 = 48_000.0;

fn engine() -> Engine {
    let transport = Transport::new(SampleRate::of(RATE));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec, tutti_engine::BlockDates::Os)
        .expect("within limits");
    Box::leak(Box::new(ed));
    TRANSPORTS.with(|t| t.borrow_mut().push(transport));
    engine
}

thread_local! {
    /// The transports of this test's engines: kept alive, and reachable by
    /// `motion`.
    static TRANSPORTS: std::cell::RefCell<Vec<Transport>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Sends `event` over the last engine this test built.
fn motion(event: MotionCommand) {
    TRANSPORTS.with(|t| {
        t.borrow()
            .last()
            .expect("an engine")
            .motion()
            .try_send(event)
            .expect("room");
    });
}

fn render(engine: &Engine, frames: usize, segment: bool) {
    let mut out = vec![0.0f32; frames * 2];
    let mut block = InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap();
    if segment {
        engine.process_segment(&mut block);
    } else {
        engine.process(&mut block);
    }
}

/// **A reading is the call's start**: nothing before the first call; after
/// each call the frame it began at (the frames rendered before it) and an OS
/// time between the moments around it, for both entry points.
#[test]
fn each_call_publishes_where_it_began() {
    for segment in [false, true] {
        let engine = engine();
        let reader = engine.block_clock();
        assert_eq!(reader.read(), None, "nothing before the first call");

        let before = tutti_platform::monotonic_ns();
        render(&engine, 256, segment);
        let first = reader.read().expect("published by the first call");
        assert_eq!(first.frame, Frame::new(0));
        assert_eq!(first.rate, SampleRate::of(RATE));
        assert!(first.os_ns >= before && first.os_ns <= tutti_platform::monotonic_ns());

        render(&engine, 512, segment);
        let second = reader.read().expect("published by the second call");
        assert_eq!(
            second.frame,
            Frame::new(256),
            "the second call begins where the first ended"
        );
        assert!(second.os_ns >= first.os_ns);
    }
}

/// **One read is one instant**: through play, a stop and a locate (a few
/// blocks each) the playhead's frame, beat, seconds and playing flag agree
/// with each other at every call. At 120 BPM and 48 kHz a frame is
/// 1 / 24 000 of a beat, and a beat is half a second.
#[test]
fn a_stop_and_a_locate_land_in_one_snapshot() {
    let engine = engine();
    let reader = engine.block_clock();
    let beat_of = |frames: u64| frames as f64 * 2.0 / RATE;

    render(&engine, 256, false);
    let p = reader.playhead();
    assert_eq!(
        (p.playing(), p.beat(), p.frame()),
        (false, Beat::new(0.0), Frame::new(0))
    );

    motion(MotionCommand::Play);
    render(&engine, 480, false);
    let p = reader.playhead();
    assert!(p.playing(), "the play landed on the call's first frame");
    assert_eq!(
        (p.frame(), p.beat(), p.seconds()),
        (Frame::new(256), Beat::new(0.0), Seconds::new(0.0))
    );

    render(&engine, 480, false);
    let p = reader.playhead();
    assert!(p.playing());
    assert_eq!(p.frame(), Frame::new(736));
    assert!((p.beat().get() - beat_of(480)).abs() < 1e-12);
    assert!((p.seconds().get() - p.beat().get() / 2.0).abs() < 1e-12);

    motion(MotionCommand::stop_unfaded());
    render(&engine, 480, false);
    let p = reader.playhead();
    assert!(
        !p.playing(),
        "the stop is in the snapshot of the call it landed on"
    );
    assert_eq!(p.frame(), Frame::new(1_216), "the graph's frame runs on");
    assert!(
        (p.beat().get() - beat_of(960)).abs() < 1e-12,
        "held where it stopped"
    );
    assert!((p.seconds().get() - p.beat().get() / 2.0).abs() < 1e-12);

    motion(MotionCommand::locate(Beat::new(8.0)));
    render(&engine, 480, false);
    let p = reader.playhead();
    assert_eq!(
        (p.playing(), p.beat(), p.seconds()),
        (false, Beat::new(8.0), Seconds::new(4.0))
    );
    assert_eq!(p.frame(), Frame::new(1_696));
}

/// **A reader never sees a torn playhead**: while the audio side renders a
/// rolling transport as fast as it can, every snapshot another thread reads
/// has its beat and seconds on the tempo's line and its beat at the frame's
/// closed form (a snapshot built from two calls would break one of them).
#[test]
fn a_reader_on_another_thread_never_sees_two_calls_mixed() {
    let engine = engine();
    let reader = engine.block_clock();
    motion(MotionCommand::Play);
    render(&engine, 480, false);
    let done = std::sync::atomic::AtomicBool::new(false);
    let reads = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let mut reads = 0u64;
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                let p = reader.playhead();
                if p.at().is_some() {
                    reads += 1;
                    assert!(p.playing());
                    let beat = (p.frame().get() as f64 - 0.0) * 2.0 / RATE;
                    assert!((p.beat().get() - beat).abs() < 1e-9, "beat {p:?}");
                    assert!(
                        (p.seconds().get() - p.beat().get() / 2.0).abs() < 1e-9,
                        "{p:?}"
                    );
                }
            }
            reads
        });
        for _ in 0..20_000 {
            render(&engine, 480, false);
        }
        done.store(true, std::sync::atomic::Ordering::Release);
        watcher.join().expect("the watcher")
    });
    assert!(reads > 0, "the watcher read");
}

/// An engine dated by `dates`, with its editor and transport kept for a
/// re-prepare.
fn dated(dates: BlockDates, rate: f64) -> (Engine, Editor, Transport) {
    let transport = Transport::new(SampleRate::of(rate));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(rate),
        MaxBlock::of(Samples::new(512)),
    ));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec, dates).expect("within limits");
    (engine, ed, transport)
}

fn gcd(a: u128, b: u128) -> u128 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The nanoseconds of `blocks`, each `(frames, rate in Hz)`, rounded to the
/// nearest: the sum of each block's `frames / rate` in exact integers.
fn exact_ns(blocks: &[(usize, u64)]) -> i64 {
    let lcm = blocks
        .iter()
        .map(|&(_, r)| u128::from(r))
        .fold(1, |a, r| a / gcd(a, r) * r);
    let scaled: u128 = blocks
        .iter()
        .map(|&(f, r)| f as u128 * 1_000_000_000 * (lcm / u128::from(r)))
        .sum();
    ((scaled + lcm / 2) / lcm) as i64
}

/// **`Frames` dates block `n` at the origin plus the frames before it over the
/// rate**: block 0 is the origin, and block `n` is `n * 512 / rate` after it,
/// to the nanosecond, at 48 kHz (a block is 10 666 666.67 ns) and at 44.1 kHz
/// (11 609 977.32 ns), where a total divided by a rounded block would drift.
#[test]
fn frames_date_block_n_at_the_origin_plus_n_blocks() {
    for rate in [48_000u64, 44_100] {
        let (engine, _ed, _transport) = dated(BlockDates::Frames, rate as f64);
        let reader = engine.block_clock();
        let mut origin = None;
        for n in 0..200usize {
            render(&engine, 512, n % 2 == 1);
            let at = reader.read().expect("published").os_ns;
            let origin = *origin.get_or_insert(at);
            assert_eq!(
                at - origin,
                exact_ns(&vec![(512, rate); n]),
                "block {n} at {rate} Hz"
            );
        }
    }
}

/// The nanoseconds of `blocks`, each `(frames, rate in Hz)`: each run of
/// blocks at one rate rounded once, and the runs added.
fn segmented_ns(blocks: &[(usize, u64)]) -> i64 {
    blocks.chunk_by(|a, b| a.1 == b.1).map(exact_ns).sum()
}

/// A re-prepare from `from` Hz to `to` Hz after three blocks, dated by
/// `Frames`: every block is `segmented_ns` of the blocks before it, using the
/// rate each reading carries.
fn rerated(from: f64, to: f64) {
    let (engine, mut ed, _transport) = dated(BlockDates::Frames, from);
    let reader = engine.block_clock();
    let mut seen: Vec<(usize, u64)> = Vec::new();
    let mut origin = None;
    let mut step = |engine: &Engine, frames: usize| {
        render(engine, frames, false);
        let reading = reader.read().expect("published");
        let origin = *origin.get_or_insert(reading.os_ns);
        assert_eq!(
            reading.os_ns - origin,
            segmented_ns(&seen),
            "the block after {} blocks",
            seen.len()
        );
        seen.push((frames, reading.rate.get() as u64));
    };
    for _ in 0..3 {
        step(&engine, 480);
    }
    ed.reprepare(Prepare::new(
        SampleRate::of(to),
        MaxBlock::of(Samples::new(512)),
    ))
    .expect("re-prepares");
    // The suspended block, then the resumed ones at the new rate.
    step(&engine, 480);
    ed.collect();
    for _ in 0..3 {
        step(&engine, 480);
    }
    let rates: Vec<u64> = seen.iter().map(|&(_, r)| r).collect();
    assert_eq!(rates[..3], [from as u64; 3]);
    assert_eq!(rates[rates.len() - 3..], [to as u64; 3], "the rate moved");
}

/// **A re-rate between blocks keeps every earlier date**: blocks before and
/// after a re-prepare are dated by the sum of each stretch at one rate, rounded
/// once per stretch. 480 frames over 24 kHz are exact in nanoseconds; over
/// 44.1 kHz they are not (10 884 353.74 ns each), so the second pair pins the
/// rounding per stretch.
#[test]
fn frames_add_each_block_at_its_own_rate() {
    rerated(48_000.0, 24_000.0);
    rerated(44_100.0, 48_000.0);
}

/// **`Os` dates from the OS clock**: through both entry points the dates never
/// go back.
#[test]
fn os_dates_never_go_back() {
    for segment in [false, true] {
        let (engine, _ed, _transport) = dated(BlockDates::Os, RATE);
        let reader = engine.block_clock();
        let mut last = i64::MIN;
        for _ in 0..50 {
            render(&engine, 128, segment);
            let at = reader.read().expect("published").os_ns;
            assert!(at >= last, "{at} after {last}");
            last = at;
        }
    }
}
