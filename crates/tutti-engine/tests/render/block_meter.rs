//! The block's transport carries the meter and the recording state.
//!
//! What is pinned here:
//!
//! - the project's meter, lent once per block, reaches `ClickNode` through
//!   `Cx::meter`: it accents the downbeat of the bar a meter change begins, on
//!   the change's frame, live and in an offline render, and it keeps counting
//!   the right bars through a loop whose range holds a meter change, on every
//!   pass;
//! - the click sounds while a take records.
//!
//! The oracles are arithmetic on the map: at 120 BPM and 48 kHz a beat is
//! 24 000 frames, and a click is heard a frame after its beat (its first
//! sample is `sin(0)`).

use std::sync::Arc;
use tutti_buffers::{ChannelLayout, MaxBlock, Planes, PlanesMut};
use tutti_engine::Engine;
use tutti_graph::{Editor, Executor, NodeKey, OutputPort, PortSource, Prepare};
use tutti_music::LoopRange;
use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_nodes::{ClickNode, ClickNodeConfig, ClickSettings, MetronomeMode};
use tutti_timeline::{
    LoopEdit, MotionCommand, OfflineTimeline, OfflineTimelineConfig, Recording, Transport,
    render_graph,
};
use tutti_units::{Amplitude, At, Beat, Frame, SampleRate, Samples};

use super::env_beat::{SR, onsets, render};
use crate::support::step_map;

/// 4/4 from beat 0, then `signature` from `at`.
fn four_four_then(at: f64, signature: TimeSignature) -> MeterMap {
    MeterMap::new([
        MeterChange::new(Beat::new(0.0), TimeSignature::of(4, 4)),
        MeterChange::new(Beat::new(at), signature),
    ])
}

// ---- the metronome -----------------------------------------------------------

/// The click at full volume on the main outputs, in `mode`.
fn click_graph(mode: MetronomeMode) -> (Editor, Executor) {
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
    ed.wire_outputs((0..2).map(|port| PortSource::Node(OutputPort::new(CLICK, port))))
        .unwrap();
    ed.commit().expect("commits");
    (ed, exec)
}

/// The click graph as a live engine under `transport`.
fn click_engine(transport: &Transport, mode: MetronomeMode) -> Engine {
    let (mut ed, exec) = click_graph(mode);
    Engine::new(transport, &mut ed, exec).expect("within the limits")
}

/// Whether each click of `stereo` at `onsets` is an accent: an accent peaks
/// at 1.0 and the other clicks at 0.7.
fn accents(stereo: &[f32], onsets: &[usize]) -> Vec<bool> {
    peaks(stereo, onsets).iter().map(|&p| p > 0.85).collect()
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
        .send(MotionCommand::play_from(Beat::new(5.0)))
        .expect("room");
    let mut out = Vec::new();
    for _ in 0..235 {
        out.extend(render(&engine, ChannelLayout::STEREO, 512));
    }
    let got = onsets(&out);
    assert_eq!(got, [1, 24_001, 36_001, 60_001, 84_001, 108_001]);
    assert_eq!(
        accents(&out, &got),
        [false, false, true, false, false, true]
    );
}

/// **A `RecordingOnly` click sounds from a take's start to its stop, and a
/// `PrerollOnly` one never does.** At 120 BPM and 48 kHz a beat is 24 000
/// frames. A take starts at frame 20 000 and stops at frame 50 000: the click
/// sounds the beat the start lands on, on its frame (a frame after it, as a
/// click's first sample is `sin(0)`), then beats 1 (frame 24 000) and 2 (frame
/// 48 000), and neither beat 0 nor beat 3. With no count-in, `PrerollOnly` is
/// silent through the same take.
#[test]
fn a_recording_only_click_sounds_from_a_takes_start_to_its_stop() {
    let clicks = |mode| {
        let transport = Transport::new(SampleRate::of(SR));
        let engine = click_engine(&transport, mode);
        let take = transport.motion().next_take_id();
        for (frame, recording) in [
            (20_000, Recording::Start(take)),
            (50_000, Recording::Stop(take)),
        ] {
            transport
                .motion()
                .schedule(At::Frame(Frame::new(frame)), recording)
                .expect("room");
        }
        transport
            .motion()
            .send(MotionCommand::play_from(Beat::new(0.0)))
            .expect("room");
        let mut out = Vec::new();
        for _ in 0..160 {
            out.extend(render(&engine, ChannelLayout::STEREO, 512));
        }
        onsets(&out)
    };
    assert_eq!(
        clicks(MetronomeMode::RecordingOnly),
        [20_001, 24_001, 48_001]
    );
    assert_eq!(clicks(MetronomeMode::PrerollOnly), Vec::<usize>::new());
}

/// **An offline render lends the timeline's meter, and the click accents the
/// same bars.** The same clicks as the live render above, from beat 5, under
/// an `OfflineTimeline` given the map; with none given the click counts 4/4,
/// where beat 6.5 is no downbeat and beat 8 is.
#[test]
fn an_offline_click_counts_the_timelines_meter() {
    let render_offline = |meter: Option<MeterMap>| {
        let mut timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat::new(5.0),
            tempo_map: step_map(&[(0.0, 120.0)]),
            sample_rate: SampleRate::of(SR),
            loop_range: None,
        });
        if let Some(meter) = meter {
            timeline = timeline.meter(Arc::new(meter));
        }
        let (_ed, mut exec) = click_graph(MetronomeMode::Always);
        let mut out = Vec::new();
        for _ in 0..235 {
            let mut planes = vec![vec![0.0f32; 512]; 2];
            render_graph(
                &timeline,
                &mut exec,
                Planes::default(),
                PlanesMut::from_vecs(&mut planes).expect("two planes"),
            );
            for f in 0..512 {
                out.extend([planes[0][f], planes[1][f]]);
            }
        }
        out
    };
    let under = render_offline(Some(four_four_then(6.5, TimeSignature::of(3, 4))));
    let got = onsets(&under);
    assert_eq!(got, [1, 24_001, 36_001, 60_001, 84_001, 108_001]);
    assert_eq!(
        accents(&under, &got),
        [false, false, true, false, false, true]
    );
    // Beats 5 to 10: 4/4's only downbeat in range is 8 (frame 72 000).
    let bare = render_offline(None);
    let got = onsets(&bare);
    assert_eq!(got, [1, 24_001, 48_001, 72_001, 96_001, 120_001]);
    assert_eq!(
        accents(&bare, &got),
        [false, false, false, true, false, false]
    );
}

/// **A loop whose range holds a meter change accents the right bars on every
/// pass.** Looping beats 5 to 8 under 4/4 then 3/4 from beat 6: beat 6 begins
/// a 3/4 bar and is accented on each pass, 5 and 7 are not, and the wrap back
/// to 5 (a block that wraps reads both meters) clicks again. A pass is 72 000
/// frames; clicks on 5, 6, 7, 5, 6, 7, 5.
#[test]
fn a_loop_across_a_meter_change_accents_the_same_bars_on_each_pass() {
    let transport = Transport::new(SampleRate::of(SR));
    transport
        .settings()
        .project_meter()
        .set_meter(four_four_then(6.0, TimeSignature::of(3, 4)));
    let engine = click_engine(&transport, MetronomeMode::Always);
    transport
        .motion()
        .schedule(
            At::NextBlock,
            LoopEdit::over(LoopRange::new(Beat::new(5.0), Beat::new(8.0)).expect("ordered")),
        )
        .expect("room");
    transport
        .motion()
        .send(MotionCommand::play_from(Beat::new(5.0)))
        .expect("room");
    let mut out = Vec::new();
    for _ in 0..293 {
        out.extend(render(&engine, ChannelLayout::STEREO, 512));
    }
    let got = onsets(&out);
    assert_eq!(
        got,
        [1, 24_001, 48_001, 72_001, 96_001, 120_001, 144_001],
        "a click on each beat of both passes"
    );
    assert_eq!(
        accents(&out, &got),
        [false, true, false, false, true, false, false]
    );
}
