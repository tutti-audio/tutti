//! **A stretched voice follows a loop's wrap when the tempo changes at it.** Over a tempo
//! map that changes tempo, the engine cuts a block where the tempo does, and a loop's wrap
//! can fall on that frame: the block's first segment ends at the loop's end and the next
//! starts at its start, at the tempo there. The wrap is then not inside any run, and what
//! shows of it is a transport change to the loop's start. A voice that reads from a
//! cursor of its own (a stretched voice's vocoder) must see that as a jump, or it plays
//! on past the loop's end.
//!
//! The setup: an `Engine` over a `Transport` at 48 kHz, 64-frame blocks, one placed voice
//! from beat 0 playing a ramp (sample `i` is `i / 131072`, so the output says which source
//! frame it played), stretched 1.5 and set back to unity so a vocoder stays resident. The
//! tempo map is 60 BPM until beat 1.38 and 240 BPM after, the loop beats 0.25 to 1.5, so
//! the loop's end is at 1.38 s + 0.12 beat / (4 beats per s) = 1.41 s, frame 67 680, and
//! the ramp must fall back to the loop's start (source frame 12 000, beat 0.25 at 60 BPM,
//! 0.25 s) there. A fall is the ramp standing more than 0.05 (6 500 source frames) below
//! where it stood 256 frames before.

use std::sync::Arc;

use tutti_buffers::{ChannelLayout, Interleaved, InterleavedMut, MaxBlock, Wave};
use tutti_engine::Engine;
use tutti_graph::{Editor, NodeKey, OutPort, PortSource, Prepare};
use tutti_sampler::{MemorySource, Playback, SlotId, VoicePool, VoiceWindow};
use tutti_tempo::{TempoMap, TempoPoint, Transition};
use tutti_timeline::{LoopEdit, MotionCommand, Transport};
use tutti_units::{At, Beat, Bpm, Cents, SampleRate, Samples, StretchFactor};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
const SCALE: f32 = 131_072.0;

/// The ramp voice's left channel over `frames` frames, under `points` (beat, BPM, all
/// steps) and the loop beats 0.25 to 1.5.
fn render(points: &[(f64, f64)], stretched: bool, frames: usize) -> Vec<f32> {
    render_with(points, stretched, frames, |i| i as f32 / SCALE)
}

/// [`render`] with the source `sample(i)` at frame `i`.
fn render_with(
    points: &[(f64, f64)],
    stretched: bool,
    frames: usize,
    sample: impl Fn(usize) -> f32,
) -> Vec<f32> {
    let transport = Transport::new(SampleRate::of(SR));
    let mut wave = Wave::new(1, SampleRate::of(SR));
    for i in 0..4 * SR as usize {
        wave.append(Interleaved::new(&[sample(i)], wave.layout()).unwrap())
            .unwrap();
    }
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(4_096)),
    ));
    let source = MemorySource::placed(Arc::new(wave), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    let pool = VoicePool::new().with_tempo(transport.settings().tempo_reader());
    let handle = ed.insert(NodeKey(1), "voice", pool).controls;
    handle
        .add(
            SlotId(1),
            source,
            Playback {
                stretch: StretchFactor::new(if stretched { 1.5 } else { 1.0 }),
                ..Playback::default()
            },
        )
        .expect("room");
    if stretched {
        // A vocoder resident at unity.
        handle
            .set_stretch(SlotId(1), StretchFactor::UNITY, Cents::new(0.0))
            .expect("room");
    }
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec, tutti_engine::BlockDates::Os)
        .expect("within the limits");
    let map = TempoMap::new(points.iter().map(|&(beat, bpm)| TempoPoint {
        beat: Beat::new(beat),
        tempo: Bpm::of(bpm),
        transition: Transition::Step,
    }))
    .expect("a map");
    transport.settings().set_tempo_map(Arc::new(map));
    transport
        .motion()
        .schedule(
            At::NextBlock,
            LoopEdit::over(
                tutti_music::LoopRange::new(Beat::new(0.25), Beat::new(1.5)).expect("a loop"),
            ),
        )
        .expect("room");
    transport
        .motion()
        .try_send(MotionCommand::Play)
        .expect("room");
    let mut left = Vec::with_capacity(frames + BLOCK);
    let mut buf = vec![0.0f32; BLOCK * 2];
    while left.len() < frames {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        left.extend(buf.as_chunks::<2>().0.iter().map(|f| f[0]));
    }
    left
}

/// The frames at which the ramp stands more than 0.05 (6 500 source frames) below where it
/// stood 256 frames before. A plain voice crossfades the wrap over a few hundred frames, a
/// vocoder voice steps, so this lag finds either; the first frame is within the
/// crossfade of the wrap's own.
fn falls(x: &[f32]) -> Vec<usize> {
    (256..x.len())
        .filter(|&i| x[i - 256] - x[i] > 0.05)
        .collect()
}

/// The wrap is due at frame 67 680, and the ramp falls to the loop's start within 512
/// frames of it.
#[test]
fn a_stretched_voice_follows_a_loop_wrap_that_falls_on_a_tempo_change() {
    let x = render(&[(0.0, 60.0), (1.38, 240.0)], true, 130_000);
    let falls = falls(&x);
    assert!(
        falls
            .first()
            .is_some_and(|&f| (67_680..67_680 + 512).contains(&f)),
        "the ramp falls at {:?}, want 67 680",
        falls.first()
    );
}

/// With the voice's second vocoder primed for the wrap, a stretched clip sounds on across
/// it. The same map and loop, the source a 1500 Hz tone of amplitude 0.5, whose phase at
/// both ends of the loop is whole turns (the loop's start at 0.25 s is 375 periods, its
/// end at 1.41 s is 2 115), so what is heard is the tone unbroken if the vocoder for the
/// wrap is ready on its frame. Every block of 128 frames (four periods) from 6 016 on, past
/// the first priming, is within 1 dB of the tone's RMS, which is two overlapped frames'
/// phase mismatch of `acos(10^(-1/20)) = 0.48` rad, through both wraps (frames 67 680 and
/// 123 360). The wrap comes after the tempo's step to 240 BPM at beat 1.38, and the
/// spare is primed for it by counting through the map how far off it is.
#[test]
fn a_looping_stretched_clip_sounds_across_a_wrap_after_a_tempo_change() {
    let tone = |i: usize| (0.5 * (std::f64::consts::TAU * 1500.0 * i as f64 / SR).sin()) as f32;
    let x = render_with(&[(0.0, 60.0), (1.38, 240.0)], true, 130_000, tone);
    let steady = 0.5 / 2.0f64.sqrt();
    for (b, block) in x[6_016..].as_chunks::<128>().0.iter().enumerate() {
        let rms = (block.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / 128.0).sqrt();
        let db = 20.0 * (rms / steady).log10();
        assert!(db.abs() < 1.0, "frame {}: {db} dB", 6_016 + b * 128);
    }
}
