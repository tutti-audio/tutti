//! [`HarmonyNode`]: chord and scale changes go out of an event port on the
//! frames playback reaches their beats, and the context in force is
//! re-stated wherever playback jumps (a start, a seek, a loop wrap) or the
//! lanes are replaced.
//!
//! Every test renders a harmony node into a sink that logs each harmony
//! event with its absolute frame. 120 BPM at 48 kHz: a beat is 24 000 frames.

use tutti_buffers::MaxBlock;

use crate::common::{beat_of, connect, rolling};
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, EventKind, Executor, ForkMode, ForkTarget, Harmony, HarmonyKind, Io, Node, Prepare,
    Shape, Status, TransportPosition,
};
use tutti_graph::{OutPort, PortSource};
use tutti_midi_runtime::{
    HarmonyChanges, HarmonyChangesError, HarmonyControls, HarmonyNode, TimedHarmony,
};
use tutti_music::LoopRange;
use tutti_units::{Beat, Bpm, SampleRate, Samples};

const RATE: f64 = 48_000.0;

/// `(absolute frame, harmony)` of every harmony event the sink received.
type Seen = tutti_fixtures::Record<(u64, Harmony)>;

/// Logs harmony; a silent mono output so a fork targeting it has one. A
/// fork's clone logs into the same list.
#[derive(Clone)]
struct Sink(Seen);

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(tutti_buffers::ChannelLayout::MONO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            if let EventKind::Harmony(h) = e.kind {
                seen.push((cx.env.frame.get() + u64::from(e.offset.get()), h));
            }
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Replaces the lanes `controls` publishes with `changes`.
fn publish(controls: &HarmonyControls, list: impl IntoIterator<Item = TimedHarmony>) {
    controls.set(changes(list));
}

/// The lanes of `list`: finite beats, one chord and one scale to a beat.
fn changes(list: impl IntoIterator<Item = TimedHarmony>) -> HarmonyChanges {
    HarmonyChanges::new(list).expect("the lanes are well formed")
}

fn rig(node: HarmonyNode, max: usize) -> (Editor, Executor, HarmonyControls, Seen) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(max)),
    ));
    let controls = ed.insert(NodeKey(1), "harmony", node).controls;
    let seen = tutti_fixtures::Record::new();
    ed.insert(
        NodeKey(2),
        "sink",
        tutti_graph::ForkByClone(Sink(seen.clone())),
    );
    connect(&mut ed, 1, 2);
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutPort {
            node: NodeKey(2),
            port: 0,
        })]);
    ed.commit().expect("commits");
    (ed, exec, controls, seen)
}

fn at(frame: u64, h: Harmony) -> TimedHarmony {
    TimedHarmony::new(beat_of(frame), h)
}

/// Render one block of `len` frames with the transport at `frame`.
fn block(exec: &mut Executor, len: usize, t: TransportPosition) {
    let mut out = vec![0.0f32; len];
    exec.process(len, &t, &[], &mut [&mut out[..]]);
}

const C: Harmony = Harmony::chord(60, 60, 0b1001_0001);
const G: Harmony = Harmony::chord(67, 67, 0b1001_0001);
const F_OVER_A: Harmony = Harmony::chord(65, 69, 0b1001_0001);
const C_MAJOR: Harmony = Harmony::scale(60, 0b1010_1011_0101);
const A_MINOR: Harmony = Harmony::scale(69, 0b0101_1010_1101);

/// A chord progression and a key change over two beats.
fn lanes() -> Vec<TimedHarmony> {
    vec![
        at(0, C),
        at(0, C_MAJOR),
        at(10_000, G),
        at(24_000, F_OVER_A),
        at(30_000, A_MINOR),
    ]
}

/// **Changes land on their frames, and a continuous run sends each once.**
/// From frame 0 in 512-frame blocks through frame 32 768: the chord and scale
/// at 0, G at 10 000, F/A at 24 000, A minor at 30 000; nothing re-stated.
#[test]
fn changes_land_on_their_frames_and_are_sent_once() {
    let (_ed, mut exec, _c, seen) = rig(HarmonyNode::new(changes(lanes())), 512);
    for b in 0..64 {
        block(&mut exec, 512, rolling(b * 512));
    }
    assert_eq!(
        seen.lock().unwrap().to_vec(),
        vec![
            (0, C),
            (0, C_MAJOR),
            (10_000, G),
            (24_000, F_OVER_A),
            (30_000, A_MINOR)
        ]
    );
}

/// **A start mid-lane re-states the context in force**, on the start's
/// frame: rolling from frame 20 000 (between G and F/A), the first block
/// sends G and C major at 0, then F/A on its frame.
#[test]
fn a_start_mid_lane_restates_the_context() {
    let (_ed, mut exec, _c, seen) = rig(HarmonyNode::new(changes(lanes())), 512);
    for b in 0..10 {
        block(&mut exec, 512, rolling(20_000 + b * 512));
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen[..2], [(0, G), (0, C_MAJOR)]);
    assert_eq!(seen[2], (4_000, F_OVER_A));
}

/// **A seek re-states the context there; a stop sends nothing.** Rolling
/// from 0, stopped for a block, then a seek to frame 31 000: A minor and F/A
/// in force at the seek, sent on its frame (the executor's 1 536).
#[test]
fn a_seek_restates_the_context_and_a_stop_sends_nothing() {
    let (_ed, mut exec, _c, seen) = rig(HarmonyNode::new(changes(lanes())), 512);
    block(&mut exec, 512, rolling(0));
    block(
        &mut exec,
        512,
        TransportPosition::new(false, Bpm::of(120.0), beat_of(512), None),
    );
    block(&mut exec, 512, rolling(31_000));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert_eq!(seen[2..], [(1_024, F_OVER_A), (1_024, A_MINOR)]);
}

/// **A loop wrap inside a block re-states the context at the wrap.** Loop
/// [0.5, 1.5) beats (frames 12 000–36 000); a block from 35 800 wraps 200
/// frames in: at the wrap (loop start, beat 0.5) G and C major are in force.
#[test]
fn a_loop_wrap_restates_the_context_at_the_wrap() {
    let (_ed, mut exec, _c, seen) = rig(HarmonyNode::new(changes(lanes())), 512);
    let lp = Some(LoopRange::new(Beat::new(0.5), Beat::new(1.5)).expect("a loop"));
    block(
        &mut exec,
        512,
        TransportPosition::new(true, Bpm::of(120.0), beat_of(35_800), lp),
    );
    let seen = seen.lock().unwrap();
    let at_wrap: Vec<Harmony> = seen.iter().filter(|s| s.0 == 200).map(|s| s.1).collect();
    assert_eq!(at_wrap, vec![G, C_MAJOR], "{seen:?}");
}

/// **A loop pass re-states only what changed since the last.** Lanes: C and
/// C major at 0, G at beat 0.5 (frame 12 000). Beats are sixty-fourths, so
/// every wrap lands on an exact frame.
///
/// A loop with no change in it, [0.5 + 1/64, 0.5 + 2/64) (375 frames), in
/// one 2 048-frame block from its start: the start re-states G and C major,
/// and its five wraps send nothing, the context they start in being the one
/// already sent. A loop with G inside, [0.5 - 1/64, 0.5 + 1/64) (750
/// frames), from its start: C and C major at the start, G at 375, and at
/// each wrap (750, 1 500) the chord in force at the loop's start, C, which
/// differs from the G sent; C major, unchanged, is not re-sent.
#[test]
fn a_loop_pass_restates_only_what_changed() {
    const SIXTY_FOURTH: u64 = 375;
    let lanes = || vec![at(0, C), at(0, C_MAJOR), at(12_000, G)];
    let render = |start: u64, end: u64| {
        let (_ed, mut exec, _c, seen) = rig(HarmonyNode::new(changes(lanes())), 2_048);
        let lp = Some(LoopRange::new(beat_of(start), beat_of(end)).expect("a loop"));
        block(
            &mut exec,
            2_048,
            TransportPosition::new(true, Bpm::of(120.0), beat_of(start), lp),
        );

        seen.lock().unwrap().to_vec()
    };
    assert_eq!(
        render(12_000 + SIXTY_FOURTH, 12_000 + 2 * SIXTY_FOURTH),
        vec![(0, G), (0, C_MAJOR)]
    );
    assert_eq!(
        render(12_000 - SIXTY_FOURTH, 12_000 + SIXTY_FOURTH),
        vec![
            (0, C),
            (0, C_MAJOR),
            (375, G),
            (750, C),
            (1_125, G),
            (1_500, C),
            (1_875, G)
        ]
    );
}

/// **A change on the loop's start is the context of every pass: no chord it
/// replaces goes out first.** Lanes: F/A at 0, C at 12 000, G at 12 200;
/// loop [12 000, 12 375), one 2 048-frame block from its start. The start
/// and each wrap state C alone (the chord in force on the pass's first
/// frame, C on it included), and G follows 200 frames into each pass. F/A,
/// in force only before the loop, never goes out, and C is not sent twice
/// on one frame.
#[test]
fn a_change_on_the_loops_start_is_each_passs_context() {
    let lanes = vec![at(0, F_OVER_A), at(12_000, C), at(12_200, G)];
    let (_ed, mut exec, _c, seen) = rig(HarmonyNode::new(changes(lanes)), 2_048);
    let lp = Some(LoopRange::new(beat_of(12_000), beat_of(12_375)).expect("a loop"));
    block(
        &mut exec,
        2_048,
        TransportPosition::new(true, Bpm::of(120.0), beat_of(12_000), lp),
    );
    let want: Vec<(u64, Harmony)> = (0..6)
        .flat_map(|k| [(375 * k, C), (375 * k + 200, G)])
        .filter(|&(f, _)| f < 2_048)
        .collect();
    assert_eq!(seen.lock().unwrap().to_vec(), want);
}

/// **New lanes re-state from the next block; the same lanes cut nothing.**
#[test]
fn replaced_lanes_restate_and_the_same_lanes_do_not() {
    let (_ed, mut exec, controls, seen) = rig(HarmonyNode::new(changes(lanes())), 512);
    block(&mut exec, 512, rolling(0));
    // The same changes, sorted by beat as they were.
    publish(&controls, lanes());
    block(&mut exec, 512, rolling(512));
    assert_eq!(seen.lock().unwrap().len(), 2, "the same lanes: nothing new");
    publish(&controls, [at(0, G)]);
    block(&mut exec, 512, rolling(1_024));
    assert_eq!(seen.lock().unwrap()[2..], [(1_024, G)]);
    assert_eq!(controls.len(), 1);
}

/// **Blocks without a publish send nothing again; two publishes between
/// blocks restate once.**
#[test]
fn a_restate_follows_a_publish_and_only_a_publish() {
    let (_ed, mut exec, controls, seen) = rig(HarmonyNode::new(changes(lanes())), 512);
    block(&mut exec, 512, rolling(0));
    block(&mut exec, 512, rolling(512));
    block(&mut exec, 512, rolling(1_024));
    assert_eq!(seen.lock().unwrap().len(), 2, "no publish, no restate");
    publish(&controls, [at(0, G)]);
    publish(&controls, [at(0, F_OVER_A)]);
    block(&mut exec, 512, rolling(1_536));
    block(&mut exec, 512, rolling(2_048));
    assert_eq!(seen.lock().unwrap()[2..], [(1_536, F_OVER_A)]);
}

/// **A fork sends the lanes as they stood.** A fork taken, then the live
/// lanes replaced: the fork re-states C and C major on its first block.
#[test]
fn a_fork_sends_the_lanes_as_they_stood() {
    let (live, _exec, controls, seen) = rig(HarmonyNode::new(changes(lanes())), 512);
    let prepare = *live.prepare();
    let (_fork, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect("forks");
    publish(&controls, [at(0, G)]);
    block(&mut fork_exec, 512, rolling(0));
    assert_eq!(seen.lock().unwrap()[..], [(0, C), (0, C_MAJOR)]);
}

/// **A change at a NaN or infinite beat is refused, with its place in the
/// input**: the first such change, whatever follows it.
#[test]
fn a_change_at_a_non_finite_beat_is_refused_with_its_index() {
    let nan = TimedHarmony::new(Beat::new(f64::NAN), G);
    let inf = TimedHarmony::new(Beat::new(f64::INFINITY), G);
    assert_eq!(
        HarmonyChanges::new([at(0, C), nan, at(24_000, G)]),
        Err(HarmonyChangesError::NonFiniteBeat { index: 1 })
    );
    assert_eq!(
        HarmonyChanges::new([inf, nan]),
        Err(HarmonyChangesError::NonFiniteBeat { index: 0 })
    );
}

/// **Two chords at one beat are refused, and so are two scales**: neither is
/// picked. The clash is found wherever the two sit in the input.
#[test]
fn two_changes_of_one_kind_at_one_beat_are_refused() {
    let one = beat_of(24_000);
    assert_eq!(
        HarmonyChanges::new([at(24_000, C), at(0, G), at(24_000, G)]),
        Err(HarmonyChangesError::SameBeat {
            beat: one,
            kind: HarmonyKind::Chord
        })
    );
    assert_eq!(
        HarmonyChanges::new([at(24_000, C_MAJOR), at(24_000, A_MINOR)]),
        Err(HarmonyChangesError::SameBeat {
            beat: one,
            kind: HarmonyKind::Scale
        })
    );
}

/// **A chord and a scale at one beat are two lanes, both kept**, sorted by
/// beat from any input order.
#[test]
fn a_chord_and_a_scale_at_one_beat_are_both_kept_in_beat_order() {
    let lanes = HarmonyChanges::new([at(24_000, G), at(24_000, C_MAJOR), at(0, C)])
        .expect("distinct per kind");
    let beats: Vec<_> = lanes.as_slice().iter().map(|c| c.beat).collect();
    assert_eq!(beats, [beat_of(0), beat_of(24_000), beat_of(24_000)]);
    assert_eq!(lanes.len(), 3);
}
