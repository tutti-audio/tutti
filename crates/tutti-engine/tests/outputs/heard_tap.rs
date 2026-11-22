//! The tap after the fader: `Engine` writes each chunk of its device-width
//! output, as it leaves the fader, to the rings of a `TapReader`. Figures are
//! analytic: the source plays `(t + 1) * (c + 1)` on channel `c` at frame `t`
//! (exact in `f32`), so a frame's lanes name their own instant and a torn,
//! shifted or folded frame is visible. The fade itself is
//! `tutti-session`'s `tests/wiring/engine.rs`.

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_buffers::MaxBlock;
use tutti_buffers::{Pull, Source};
use tutti_engine::{Engine, HeardTaps};
use tutti_graph::NodeKey;
use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, TapReader, Unforkable};
use tutti_graph::{OutPort, PortSource};
use tutti_timeline::Transport;
use tutti_units::{SampleRate, Samples};

const RATE: f64 = 48_000.0;

/// `width` channels; channel `c` at frame `t` plays `(t + 1) * (c + 1)`.
struct Ramp {
    width: u16,
    at: usize,
}

impl Node for Ramp {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::from_count(self.width))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for c in 0..usize::from(self.width) {
            for (k, s) in io.output(c).iter_mut().enumerate() {
                *s = ((self.at + k + 1) * (c + 1)) as f32;
            }
        }
        self.at += io.output(0).len();
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn rig(width: u16) -> (Engine, HeardTaps) {
    let transport = Transport::new(SampleRate::of(RATE));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "ramp", Unforkable(Ramp { width, at: 0 }));
    ed.spec_mut().topology.set_outputs((0..width).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let mut engine = Engine::new(&transport, &mut ed, exec).expect("within limits");
    Box::leak(Box::new((ed, transport)));
    let taps = engine.take_heard_taps().expect("a fresh engine holds it");
    (engine, taps)
}

fn render(engine: &Engine, frames: usize, width: u16, segment: bool) -> Vec<f32> {
    let mut out = vec![0.0f32; frames * usize::from(width)];
    let mut block = InterleavedMut::new(&mut out, ChannelLayout::from_count(width)).unwrap();
    if segment {
        engine.process_segment(&mut block);
    } else {
        engine.process(&mut block);
    }
    out
}

/// Sets its flag when dropped, so a panic in the rendering thread still lets
/// the reader thread finish and the scope unwind (instead of hanging).
struct Raise<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Raise<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn drain(tap: &mut TapReader, into: &mut Vec<f32>) {
    let mut buf = vec![0.0f32; tap.width() * 4096];
    loop {
        let n = tap
            .pop_into(InterleavedMut::new(&mut buf, tap.layout()).unwrap())
            .unwrap()
            .get();
        if n == 0 {
            break;
        }
        into.extend_from_slice(&buf[..n * tap.width()]);
    }
}

/// The tap is the engine's output buffer, every channel, whole frames, bit for bit:
/// a 6-wide device is not folded, and a block longer than the graph's 512-frame
/// blocks (written chunk by chunk) arrives complete and in order. The pure
/// render (`process_segment`) writes too, on purpose: it is a render a host
/// pulls and the tap is "what the engine rendered".
#[test]
fn the_tap_is_the_device_output_on_every_channel() {
    for segment in [false, true] {
        let (engine, mut taps) = rig(6);
        let mut tap = taps.open(ChannelLayout::from_count(6), Samples::new(8192));
        let mut heard = Vec::new();
        let mut tapped = Vec::new();
        for frames in [64, 1500, 1, 700] {
            heard.extend(render(&engine, frames, 6, segment));
            drain(&mut tap, &mut tapped);
        }
        assert_eq!(tap.take_lost(), Samples::new(0));
        assert_eq!(heard.len(), 2265 * 6);
        let bits = |x: &[f32]| x.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&tapped), bits(&heard), "segment: {segment}");
        // And the figure itself: frame t, channel c.
        assert_eq!(tapped[(2000 * 6) + 3], (2001 * 4) as f32);
    }
}

/// The first frame is the first of the first call that begins after the
/// install is pushed; a close stops the next call's writes; a dropped reader
/// is closed by the next reclaim; and closing with nothing open is a no-op.
#[test]
fn a_tap_flows_from_the_next_call_and_stops_at_a_close() {
    let (engine, mut taps) = rig(2);
    render(&engine, 64, 2, false);
    let mut tap = taps.open(ChannelLayout::STEREO, Samples::new(1024));
    assert_eq!(
        tap.available(),
        Samples::new(0),
        "nothing before a call renders"
    );
    let second = render(&engine, 64, 2, false);
    assert_eq!(
        tap.available(),
        Samples::new(64),
        "exactly the next call's block"
    );
    let mut got = vec![0.0f32; 128];
    tap.pop_into(InterleavedMut::new(&mut got, tap.layout()).unwrap())
        .unwrap();
    assert_eq!(got, second);
    taps.close();
    render(&engine, 64, 2, false);
    assert_eq!(
        tap.available(),
        Samples::new(0),
        "a closed tap gets nothing more"
    );
    taps.reclaim(); // frees the retired writer, closing it
    let mut scratch = vec![0.0f32; 128];
    let tap_layout = tap.layout();
    assert!(
        matches!(
            tap.pull(InterleavedMut::new(&mut scratch, tap_layout).unwrap()),
            Pull::Ended
        ),
        "a closed reader ends once drained"
    );
    taps.close();
    render(&engine, 64, 2, false);

    let tap = taps.open(ChannelLayout::STEREO, Samples::new(1024));
    drop(tap);
    taps.reclaim();
    render(&engine, 64, 2, false);
    let third = taps.open(ChannelLayout::STEREO, Samples::new(1024));
    render(&engine, 64, 2, false);
    assert_eq!(third.available(), Samples::new(64), "open after a drop");
}

/// A second open supersedes the first: its reader ends once drained, and the
/// first writer was handed back to the control side (the engine's `RtScope`
/// makes a drop on the audio thread a debug panic).
#[test]
fn a_second_open_ends_the_first_reader_after_its_drain() {
    let (engine, mut taps) = rig(2);
    let mut first = taps.open(ChannelLayout::STEREO, Samples::new(1024));
    render(&engine, 64, 2, false);
    let second = taps.open(ChannelLayout::STEREO, Samples::new(1024));
    render(&engine, 32, 2, false);
    taps.reclaim(); // frees the retired first writer on this thread
    assert_eq!(
        first.available(),
        Samples::new(64),
        "only what it was written"
    );
    assert_eq!(second.available(), Samples::new(32), "the latest writes");
    let mut buf = vec![0.0f32; 128 * 2];
    let layout = first.layout();
    assert!(
        matches!(
            first.pull(InterleavedMut::new(&mut buf, layout).unwrap()),
            Pull::Frames(n) if n == Samples::new(64)
        ),
        "the drain"
    );
    assert!(
        matches!(
            first.pull(InterleavedMut::new(&mut buf, layout).unwrap()),
            Pull::Ended
        ),
        "then it ends"
    );
    assert!(!second.writer_gone(), "the latest does not end");
}

/// Closing with nothing open sends nothing: the next open still writes from
/// the first render after its push.
#[test]
fn closing_with_none_open_is_a_no_op() {
    let (engine, mut taps) = rig(2);
    taps.close();
    taps.close();
    let tap = taps.open(ChannelLayout::STEREO, Samples::new(1024));
    render(&engine, 64, 2, false);
    assert_eq!(tap.available(), Samples::new(64));
}

/// With the audio thread behind (no render since two installs filled the
/// queue) an open still returns its reader. It is the pending install: it
/// writes from the first render after the push, a later open replaces it
/// (the replaced reader ends, nothing was written to it), and the writer the
/// replacement displaced was dropped on the control side.
#[test]
fn an_open_while_behind_is_pending_and_the_latest_wins() {
    let (engine, mut taps) = rig(2);
    let layout = ChannelLayout::STEREO;
    let _a = taps.open(layout, Samples::new(256));
    let _b = taps.open(layout, Samples::new(256));
    // The queue is full: this one waits.
    let mut c = taps.open(layout, Samples::new(256));
    let d = taps.open(layout, Samples::new(256));
    let mut buf = vec![0.0f32; 8];
    let c_layout = c.layout();
    assert!(
        matches!(
            c.pull(InterleavedMut::new(&mut buf, c_layout).unwrap()),
            Pull::Ended
        ),
        "the replaced pending writer was closed here, unwritten"
    );
    assert_eq!(d.available(), Samples::new(0), "nothing before the push");
    render(&engine, 64, 2, false); // takes a and b
    assert_eq!(d.available(), Samples::new(0), "not yet pushed");
    taps.reclaim(); // pushes d
    render(&engine, 64, 2, false);
    assert_eq!(
        d.available(),
        Samples::new(64),
        "the first render after the push"
    );
}

/// A ring that cannot hold the block keeps its first frames, counts the rest
/// once, and a block of another width than the tap's is counted whole.
#[test]
fn drops_are_counted_and_a_wrong_width_is_lost_whole() {
    let (engine, mut taps) = rig(2);
    let mut tap = taps.open(ChannelLayout::STEREO, Samples::new(100));
    render(&engine, 300, 2, false);
    assert_eq!(tap.available(), Samples::new(100));
    assert_eq!(tap.take_lost(), Samples::new(200));
    assert_eq!(tap.take_lost(), Samples::new(0), "reading clears");
    let mut got = vec![0.0f32; 200];
    tap.pop_into(InterleavedMut::new(&mut got, tap.layout()).unwrap())
        .unwrap();
    assert_eq!(got[..4], [1.0, 2.0, 2.0, 4.0], "the first frames were kept");

    // The device now hands a 6-wide block to a 2-wide tap.
    let (wide, mut wide_taps) = rig(6);
    let narrow = wide_taps.open(ChannelLayout::STEREO, Samples::new(1024));
    render(&wide, 50, 6, false);
    assert_eq!(narrow.available(), Samples::new(0));
    assert_eq!(narrow.take_lost(), Samples::new(50));
}

/// Lanes cannot tear under a concurrent drain: a reader thread pops in small
/// bursts with pauses against a ring that overflows, and every frame that
/// arrives has all six channels from one instant (`(c + 1) * lane 0`), with
/// frames read plus frames lost equal to frames rendered.
#[test]
fn channels_do_not_tear_under_a_concurrent_drain() {
    const CALLS: usize = 200;
    const FRAMES: usize = 64;
    let (engine, mut taps) = rig(6);
    let mut tap = taps.open(ChannelLayout::from_count(6), Samples::new(256));
    let done = std::sync::atomic::AtomicBool::new(false);
    let (got, lost) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut got = Vec::new();
            let mut buf = vec![0.0f32; 6 * 5];
            loop {
                let n = tap
                    .pop_into(InterleavedMut::new(&mut buf, tap.layout()).unwrap())
                    .unwrap()
                    .get();
                got.extend_from_slice(&buf[..n * 6]);
                if n == 0 {
                    if done.load(std::sync::atomic::Ordering::Acquire) && tap.available().get() == 0
                    {
                        break;
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
            }
            (got, tap.take_lost().get())
        });
        let finished = Raise(&done);
        for _ in 0..CALLS {
            render(&engine, FRAMES, 6, false);
        }
        drop(finished);
        reader.join().expect("the reader finished")
    });
    assert!(lost > 0, "the ring overflowed, so the test saw drops");
    assert_eq!(got.len() / 6 + lost, CALLS * FRAMES);
    for frame in got.as_chunks::<6>().0 {
        for (c, &x) in frame.iter().enumerate() {
            assert_eq!(x, frame[0] * (c + 1) as f32, "a torn frame: {frame:?}");
        }
    }
}

/// Installing, replacing and closing a tap while blocks render never frees on
/// the audio thread (the engine enters `RtScope`, where a `TapWriter`'s drop
/// is a debug panic) and never loses the hand-over: the control thread opens,
/// drops and closes while another thread renders.
#[test]
fn opening_and_closing_while_rendering_never_frees_on_the_audio_thread() {
    let (engine, mut taps) = rig(2);
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let control = scope.spawn(|| {
            let mut opened = 0;
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                let tap = taps.open(ChannelLayout::STEREO, Samples::new(64));
                opened += 1;
                std::thread::yield_now();
                drop(tap);
                taps.reclaim();
                taps.close();
            }
            opened
        });
        let finished = Raise(&stop);
        for _ in 0..3000 {
            render(&engine, 16, 2, false);
        }
        drop(finished);
        assert!(control.join().expect("control finished") > 0);
    });
}
