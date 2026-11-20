//! The block's transport carries the meter and the count-in.
//!
//! What is pinned here:
//!
//! - the engine stamps each block's transport with the meter in force at its
//!   first frame (the signature and a bar line to count from) and the next
//!   change after it, under the project's meter, and each piece after a
//!   tempo step with the meter at its own beat;
//! - the count-in flag of the transport's settings reaches the block;
//! - an offline timeline stamps the meter it is given;
//! - `ClickNode`, reading both from its `Env`, accents the downbeat of the
//!   bar a meter change begins, on the change's frame, and sounds under the
//!   count-in.
//!
//! The oracles are arithmetic on the map: a bar of `n/d` is `4n/d` quarter
//! notes from the change that put it in force, and at 120 BPM and 48 kHz a
//! beat is 24 000 frames.

use std::sync::{Arc, Mutex};
use tutti_buffers::{ChannelLayout, MaxBlock, Planes, PlanesMut};
use tutti_engine::Engine;
use tutti_graph::{BlockMeter, Editor, Executor, NodeKey, OutPort, Prepare, Source};
use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_nodes::{ClickNode, ClickNodeConfig, ClickSettings, MetronomeMode};
use tutti_timeline::{
    MotionCommand, OfflineTimeline, OfflineTimelineConfig, Transport, render_graph,
};
use tutti_units::{Amplitude, Beat, SampleRate, Samples};

use super::env_beat::{SR, env_graph, onsets, render};
use crate::support::{beat_of_frame, step_map};

/// 4/4 from beat 0, then `signature` from `at`.
fn four_four_then(at: f64, signature: TimeSignature) -> MeterMap {
    MeterMap::new([
        MeterChange::new(Beat::new(0.0), TimeSignature::of(4, 4)),
        MeterChange::new(Beat::new(at), signature),
    ])
}

/// The engine of an `EnvLog` under `transport`, and the log.
fn logging(transport: &Transport) -> (Engine, Arc<Mutex<Vec<tutti_graph::Env>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (mut ed, exec) = env_graph(512, &log);
    let engine = Engine::new(transport, &mut ed, exec, tutti_engine::BlockDates::Os)
        .expect("within the limits");
    (engine, log)
}

/// The first block's transport meter, rolling from `start` under `map`.
fn first_block_meter(map: MeterMap, start: f64) -> Option<BlockMeter> {
    let transport = Transport::new(SampleRate::of(SR));
    transport.settings().project_meter().set_meter(map);
    let (engine, log) = logging(&transport);
    transport
        .motion()
        .try_send(MotionCommand::play_from(Beat::new(start)))
        .expect("room");
    render(&engine, ChannelLayout::MONO, 256);
    let first = log.lock().expect("log")[0].transport;
    first.meter()
}

/// **A block's transport carries the meter in force at its first frame and
/// the bar line its bars are counted from.** Under 4/4 then 7/8 from beat 8,
/// rolling from beat 9 or from beat 12 (in the bar from 8 + 3.5 = 11.5) the
/// bars are counted from the change at 8; rolling from beat 5 they are 4/4
/// bars from beat 0, with the change ahead.
#[test]
fn the_block_carries_the_meter_in_force_and_its_bar_start() {
    let seven_eight = TimeSignature::of(7, 8);
    let change = MeterChange::new(Beat::new(8.0), seven_eight);
    let in_seven = BlockMeter::new(seven_eight, Beat::new(8.0), None);
    let in_four = BlockMeter::new(TimeSignature::of(4, 4), Beat::new(0.0), Some(change));
    for (start, want) in [(9.0, in_seven), (12.0, in_seven), (5.0, in_four)] {
        assert_eq!(
            first_block_meter(four_four_then(8.0, seven_eight), start),
            Some(want),
            "rolling from beat {start}"
        );
    }
}

/// **A meter change inside the block is carried as the next change.** Rolling
/// from beat 7.99 under 4/4 then 3/4 from beat 8: the block is in the 4/4
/// bars from beat 0, and the change is 0.01 beat = 240 frames in, inside the
/// 256-frame block.
#[test]
fn a_meter_change_inside_the_block_is_the_next_change() {
    let three_four = TimeSignature::of(3, 4);
    let map = four_four_then(8.0, three_four);
    assert_eq!(
        first_block_meter(map, 7.99),
        Some(BlockMeter::new(
            TimeSignature::of(4, 4),
            Beat::new(0.0),
            Some(MeterChange::new(Beat::new(8.0), three_four))
        ))
    );
}

/// **Each piece after a tempo step carries the meter at its own beat.** Under
/// 4/4 then 3/4 from beat 0.5, a step to 151 BPM at the beat frame 20 111
/// reaches at 120 BPM (beat 0.838) lands inside a block, and its piece is in
/// the 3/4 bars counted from beat 0.5.
#[test]
fn a_piece_after_a_tempo_step_carries_the_meter_at_its_beat() {
    let transport = Transport::new(SampleRate::of(SR));
    let three_four = TimeSignature::of(3, 4);
    transport
        .settings()
        .project_meter()
        .set_meter(four_four_then(0.5, three_four));
    let step_beat = beat_of_frame(20_111, 120.0, SR);
    transport
        .settings()
        .set_tempo_map(step_map(&[(0.0, 120.0), (step_beat, 151.0)]));
    let (engine, log) = logging(&transport);
    transport
        .motion()
        .try_send(MotionCommand::play_from(Beat::new(0.0)))
        .expect("room");
    for _ in 0..60 {
        render(&engine, ChannelLayout::MONO, 512);
    }
    let log = log.lock().expect("log");
    let pieces: Vec<_> = log
        .iter()
        .flat_map(|env| env.changes.as_slice().iter().map(|c| c.to))
        .collect();
    assert!(!pieces.is_empty(), "the step lands inside a block");
    for piece in pieces {
        assert_eq!(
            piece.meter(),
            Some(BlockMeter::new(three_four, Beat::new(0.5), None)),
            "piece at beat {}",
            piece.beat().get()
        );
    }
}

/// **The count-in flag of the transport's settings is the block's.** Off, then
/// on between two blocks: the first block's transport is not counting in, the
/// second's is.
#[test]
fn the_count_in_of_the_settings_reaches_the_block() {
    let transport = Transport::new(SampleRate::of(SR));
    let (engine, log) = logging(&transport);
    render(&engine, ChannelLayout::MONO, 256);
    transport.settings().set_in_preroll(true);
    render(&engine, ChannelLayout::MONO, 256);
    let counted: Vec<bool> = log
        .lock()
        .expect("log")
        .iter()
        .map(|env| env.transport.count_in())
        .collect();
    assert_eq!(counted, [false, true]);
}

/// **An offline timeline stamps the meter it is given, and none otherwise.**
/// Rendering from beat 12 under 4/4 then 7/8 from beat 8, each block is in
/// the 7/8 bars counted from 8; a timeline without a meter hands the graph
/// none.
#[test]
fn an_offline_timeline_stamps_the_meter_it_is_given() {
    let seven_eight = TimeSignature::of(7, 8);
    let timeline = || {
        OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat::new(12.0),
            tempo_map: step_map(&[(0.0, 120.0)]),
            sample_rate: SampleRate::of(SR),
            loop_range: None,
        })
    };
    let first_meter = |timeline: OfflineTimeline| {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (_ed, mut exec) = env_graph(512, &log);
        let mut out = vec![0.0f32; 256];
        render_graph(
            &timeline,
            &mut exec,
            Planes::default(),
            PlanesMut::new(&mut [&mut out[..]]).unwrap(),
        );
        let first = log.lock().expect("log")[0].transport;
        first.meter()
    };
    assert_eq!(
        first_meter(timeline().meter(Arc::new(four_four_then(8.0, seven_eight)))),
        Some(BlockMeter::new(seven_eight, Beat::new(8.0), None))
    );
    assert_eq!(first_meter(timeline()), None);
}

// ---- the metronome -----------------------------------------------------------

/// The click at full volume on the global outputs, in `mode`, under
/// `transport`.
fn click_engine(transport: &Transport, mode: MetronomeMode) -> Engine {
    let settings = Arc::new(ClickSettings::new());
    settings.mode().store(mode);
    settings.set_volume(tutti_nodes::ClickLevel::of(Amplitude::new(1.0)));
    let (mut ed, exec): (Editor, Executor) = Editor::new(Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(512)),
    ));
    const CLICK: NodeKey = NodeKey(1);
    ed.insert(
        CLICK,
        "click",
        ClickNode::from(ClickNodeConfig::new(settings)),
    );
    ed.spec_mut()
        .topology
        .set_outputs((0..2).map(|port| Source::Node(OutPort { node: CLICK, port })));
    ed.commit().expect("commits");
    Engine::new(transport, &mut ed, exec, tutti_engine::BlockDates::Os).expect("within the limits")
}

/// The click's loudest sample in the 1 440 frames (30 ms, its length) from
/// each onset of `stereo`'s left channel.
fn peaks(stereo: &[f32], onsets: &[usize]) -> Vec<f32> {
    let left: Vec<f32> = stereo.iter().step_by(2).copied().collect();
    onsets
        .iter()
        .map(|&f| {
            left[f..(f + 1_440).min(left.len())]
                .iter()
                .fold(0.0f32, |m, x| m.max(x.abs()))
        })
        .collect()
}

/// **The click accents the downbeat of the bar a meter change begins, on the
/// change's frame.** Rolling from beat 5 under 4/4 then 3/4 from beat 6.5,
/// at 120 BPM and 48 kHz (24 000 frames a beat): clicks on beats 5 and 6
/// (4/4), then a bar begins at 6.5 (frame 36 000, mid-block and mid-beat of
/// the 4/4 grid) and is accented, then 7.5 and 8.5 are its other beats and
/// 9.5 begins the next bar. Each click is heard a frame after its beat (its
/// first sample is `sin(0)`); an accent peaks at 1.0 and the others at 0.7.
#[test]
fn the_click_accents_the_bar_a_meter_change_begins_on_its_frame() {
    let transport = Transport::new(SampleRate::of(SR));
    transport
        .settings()
        .project_meter()
        .set_meter(four_four_then(6.5, TimeSignature::of(3, 4)));
    let engine = click_engine(&transport, MetronomeMode::Always);
    transport
        .motion()
        .try_send(MotionCommand::play_from(Beat::new(5.0)))
        .expect("room");
    let mut out = Vec::new();
    for _ in 0..235 {
        out.extend(render(&engine, ChannelLayout::STEREO, 512));
    }
    let got = onsets(&out);
    assert_eq!(got, [1, 24_001, 36_001, 60_001, 84_001, 108_001]);
    let accents: Vec<bool> = peaks(&out, &got).iter().map(|&p| p > 0.85).collect();
    assert_eq!(accents, [false, false, true, false, false, true]);
}

/// **The click sounds under the transport's count-in.** In `PrerollOnly` it
/// is silent while the flag is off, and sounds on the first frames after the
/// flag is set.
#[test]
fn the_click_sounds_while_the_transport_counts_in() {
    let transport = Transport::new(SampleRate::of(SR));
    let engine = click_engine(&transport, MetronomeMode::PrerollOnly);
    transport
        .motion()
        .try_send(MotionCommand::play_from(Beat::new(0.0)))
        .expect("room");
    let before = render(&engine, ChannelLayout::STEREO, 2_048);
    assert!(before.iter().all(|&s| s == 0.0), "silent off the count-in");
    transport.settings().set_in_preroll(true);
    let during = render(&engine, ChannelLayout::STEREO, 2_048);
    assert!(
        during[..200].iter().any(|&s| s != 0.0),
        "sounding within the first frames of the count-in"
    );
}
