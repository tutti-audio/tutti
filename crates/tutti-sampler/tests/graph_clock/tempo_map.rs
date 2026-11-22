//! A beat-locked sampler voice follows the project's tempo map: a real render of a
//! `VoicePool` through the live engine over a
//! published `TempoMap`, the voice's output read back as the source position
//! it played.
//!
//! A placed voice is a clip: its window is in beats, so it enters on the
//! frame the playhead reaches its start beat and (with a duration) leaves on
//! the frame it reaches its end, and between them it plays its samples at
//! their own speed. So the oracle is the map's definition, written here and
//! never read back from `tutti-tempo` or another render. A step segment of
//! `bpm` takes `60 / bpm` seconds a beat; a ramp linear in time from `r0` to
//! `r1` beats per second over `D` beats lasts `T = 2 D / (r0 + r1)`, has
//! slope `a = (r1 - r0) / T` and reaches beat `b` at
//! `s = (sqrt(r0^2 + 2 a b) - r0) / a`. A clip starting at beat `S` begins at
//! frame `seconds(S) x rate`, and `f` frames on the voice is at source frame
//! `f - seconds(S) x rate`.
//!
//! The wave is a ramp (sample `i` is `i / 2^17`, exact in `f32`), so the
//! output says which source frame it is, and a re-seat, a jump or a fade at a
//! tempo change shows as a source frame that is not where the map puts it.
//!
//! **The bound for a ramp**: the engine runs a ramp as
//! constant-tempo cells of 128 frames whose beats are the chord of the curve
//! between the cell's ends, so the playhead is within `|a| h^2 / 8` beats of
//! the curve inside a cell (`h` = 128 / rate): 8.3e-7 beats for 60 to 240 BPM
//! over 8 beats at 48 kHz, which at the ramp's slowest rate (1 beat/s) is
//! 0.04 frames. The voice's position is a time difference, so it is within
//! that of the map's, plus the ramp wave's `f32` rounding (0.02 frames at the
//! largest source frame read here).

use tutti_buffers::Interleaved;
use tutti_sampler::VoiceWindow;

use std::sync::Arc;
use tutti_buffers::MaxBlock;

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_buffers::Wave;
use tutti_engine::Engine;
use tutti_graph::{Editor, NodeKey, OutPort, PortSource, Prepare};
use tutti_sampler::{MemorySource, Playback, SlotId, VoicePool};
use tutti_tempo::{TempoMap, TempoPoint, Transition};
use tutti_timeline::{MotionCommand, Transport};
use tutti_units::{Beat, BeatDuration, Bpm, Cents, SampleRate, Samples, StretchFactor};

const SR: f64 = 48_000.0;
const MAX_BLOCK: usize = 4_096;
/// The wave is `i / SCALE` at frame `i`.
const SCALE: f32 = 131_072.0;
/// The most a ramp's position sits from the map's, in frames (see the module
/// docs: 0.04 for the grid, 0.02 for the wave's `f32`, and some slack).
const E: f64 = 0.08;
const BLOCKS: [usize; 6] = [1, 64, 100, 512, 1_000, 4_096];

fn map(points: &[(f64, f64, Transition)]) -> Arc<TempoMap> {
    Arc::new(
        TempoMap::new(points.iter().map(|&(beat, bpm, shape)| TempoPoint {
            beat: Beat::new(beat),
            tempo: Bpm::of(bpm),
            transition: shape,
        }))
        .expect("a map"),
    )
}

/// 120 BPM, then 60 from beat 2.3 (frame 55 200: in the middle of the cell
/// 431 of the grid, so the step is not on a cell edge), then 180 from 4.
fn steps() -> Arc<TempoMap> {
    map(&[
        (0.0, 120.0, Transition::Step),
        (2.3, 60.0, Transition::Step),
        (4.0, 180.0, Transition::Step),
    ])
}

/// The seconds of beat `b` of `steps()`: each segment takes `60 / bpm` a beat.
fn step_seconds(b: f64) -> f64 {
    let seg = |from: f64, to: f64, bpm: f64| (b.min(to) - from).max(0.0) * 60.0 / bpm;
    seg(0.0, 2.3, 120.0) + seg(2.3, 4.0, 60.0) + seg(4.0, f64::INFINITY, 180.0)
}

/// 60 to 240 BPM over 8 beats, linear in time, then 240.
fn ramp() -> Arc<TempoMap> {
    map(&[
        (0.0, 60.0, Transition::RampTime { end: None }),
        (8.0, 240.0, Transition::Step),
    ])
}

/// The seconds of beat `b` of `ramp()`: `r0` = 1, `r1` = 4 beats/s, `D` = 8.
fn ramp_seconds(b: f64) -> f64 {
    const R0: f64 = 1.0;
    const T: f64 = 2.0 * 8.0 / (1.0 + 4.0);
    const A: f64 = (4.0 - R0) / T;
    if b <= 8.0 {
        ((R0 * R0 + 2.0 * A * b).sqrt() - R0) / A
    } else {
        T + (b - 8.0) * 60.0 / 240.0
    }
}

/// The first frame at or after `x`, with the 1e-6 of slack the engine's rule
/// has for a rate's rounding.
fn frame_after(x: f64) -> usize {
    (x - 1e-6).ceil().max(0.0) as usize
}

/// One placed voice (from `start`, for `duration` beats or the whole wave)
/// played from beat 0 over `map` for `frames`, in blocks of `block`, through
/// a pool bound to the map; the left channel.
fn render(
    map: &Arc<TempoMap>,
    voice: Option<(f64, Option<f64>)>,
    frames: usize,
    block: usize,
) -> Vec<f32> {
    let transport = Transport::new(SampleRate::of(SR));
    let mut wave = Wave::new(1, SampleRate::of(SR));
    for i in 0..4 * SR as usize {
        wave.append(Interleaved::new(&[i as f32 / SCALE], wave.layout()).unwrap())
            .unwrap();
    }
    let wave = Arc::new(wave);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(MAX_BLOCK)),
    ));
    match voice {
        Some((start, duration)) => {
            let source = MemorySource::placed(
                wave,
                match duration {
                    Some(d) => VoiceWindow::span(Beat::new(start), BeatDuration::new(d)),
                    None => VoiceWindow::from(Beat::new(start)),
                },
            )
            .expect("a placeable wave");
            let mut pool = VoicePool::new().tempo(transport.settings().tempo_reader());
            pool.insert(SlotId(1), source, Playback::default())
                .expect("a finite gain and a nonempty width");
            ed.insert(NodeKey(1), "voice", pool);
        }
        // Free-running: a node of its own, from its first frame, at its own
        // speed, on no window.
        None => {
            let source = MemorySource::new(wave).expect("a placeable wave");
            source.trigger();
            ed.insert(NodeKey(1), "voice", source);
        }
    }
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings().set_tempo_map(Arc::clone(map));
    transport.motion().send(MotionCommand::Play).expect("room");
    let mut left = Vec::with_capacity(frames + block);
    let mut buf = vec![0.0f32; block * 2];
    while left.len() < frames {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        left.extend(buf.as_chunks::<2>().0.iter().map(|f| f[0]));
    }
    left.truncate(frames);
    left
}

/// The source frame the output says it played, when it played one.
fn played(sample: f32) -> f64 {
    f64::from(sample) * f64::from(SCALE)
}

/// **A step map: a clip is where the map puts it, exactly.** 120 BPM, 60 from
/// beat 2.3 (mid-cell) and 180 from beat 4; clips from beat 0 (through every
/// step), from beat 1 (through two), from beat 3 (inside the 60 segment, then
/// the last step), and one from beat 1 for 2.5 beats (ends in the 60
/// segment). Each enters on the first frame at or after `seconds(S) x rate`,
/// plays its source frames one a frame without a seam at either step (a step
/// is exact: the engine seats the segment on the map's beat), leaves on the
/// first frame at or after `seconds(S + d) x rate`, and is silent outside.
/// At every block size.
#[test]
fn a_step_map_puts_a_clips_frames_where_the_map_says() {
    let m = steps();
    let frames = 3 * 48_000;
    for (start, duration) in [(0.0, None), (1.0, None), (3.0, None), (1.0, Some(2.5))] {
        let enter = frame_after(step_seconds(start) * SR);
        let leave = duration.map_or(frames, |d| {
            frame_after(step_seconds(start + d) * SR).min(frames)
        });
        assert!(enter < leave && leave <= frames, "clip {start}");
        for block in BLOCKS {
            let out = render(&m, Some((start, duration)), frames, block);
            for (f, &got) in out.iter().enumerate() {
                let what = format!("clip {start} {duration:?} block {block} frame {f}");
                if (enter..leave).contains(&f) {
                    let want = (f - enter) as f32 / SCALE;
                    assert_eq!(got.to_bits(), want.to_bits(), "{what}");
                } else {
                    assert_eq!(got, 0.0, "{what}: silent outside the window");
                }
            }
        }
    }
}

/// **A ramp: the clip is within the bound of the map's
/// frames, and a cell edge is no seam.** A clip from beat 0 and one from
/// beat 2 (mid-ramp, so it enters at a fractional frame: its first frame is
/// the fraction past the start) over the 60 to 240 ramp: at every frame the
/// voice is at source frame `f - seconds(S) x rate` within 0.08 frames, and
/// it advances one source frame a frame within 0.1 at every frame, in
/// particular at each of the ramp's cell edges (a re-seat at the cell's tempo
/// is a jump of frames; a wrap fade or a declick is a level step). The
/// bound is reached somewhere (some frame is more than 0.001 from the exact
/// position) or the test would pass on a clock that is not the curve.
#[test]
fn a_ramp_puts_a_clips_frames_within_the_grid_bound_of_the_maps() {
    let m = ramp();
    let frames = 2 * 48_000;
    let mut off = 0.0f64;
    for start in [0.0, 2.0] {
        let x_start = ramp_seconds(start) * SR;
        let enter = frame_after(x_start);
        for block in [64, 100, 4_096] {
            let out = render(&m, Some((start, None)), frames, block);
            assert!(out[..enter].iter().all(|&s| s == 0.0), "clip {start}");
            for f in enter..frames {
                let want = f as f64 - x_start;
                let err = (played(out[f]) - want).abs();
                off = off.max(err);
                assert!(
                    err <= E,
                    "clip {start} block {block} frame {f}: source frame {}, the map says {want}",
                    played(out[f])
                );
                if f > enter {
                    let step = played(out[f]) - played(out[f - 1]);
                    assert!(
                        (step - 1.0).abs() <= 0.1,
                        "clip {start} block {block} frame {f}: a step of {step} frames"
                    );
                }
            }
        }
    }
    assert!(off > 0.001, "the bound is reached: {off}");
}

/// **A constant map is what it was.** A clip from beat 1 at 120 BPM through a
/// pool bound to a constant map is at source frame `f - 24 000` exactly from
/// frame 24 000, at every block size: the voice reads a constant map the way
/// it reads no map (the beats over the tempo, not the map's seconds). The bits
/// are held by `block_length_invariance`; this case pins the figure through a
/// bound reader.
#[test]
fn a_constant_map_keeps_the_clip_where_it_was() {
    let m = map(&[(0.0, 120.0, Transition::Step)]);
    let frames = 48_000;
    for block in BLOCKS {
        let out = render(&m, Some((1.0, None)), frames, block);
        for (f, &got) in out.iter().enumerate() {
            let want = f.checked_sub(24_000).map_or(0.0, |i| i as f32 / SCALE);
            assert_eq!(got.to_bits(), want.to_bits(), "block {block} frame {f}");
        }
    }
}

/// **A time-locked voice ignores the map.** A free-running source (no
/// window, from its first frame at its own speed) is at source frame `f` at
/// every frame across the steps and the ramp's cells, whatever the tempo.
///
/// A free-running source reads no transport and no map, so the property holds by
/// construction and this case pins it against a change that gives an unplaced source a
/// window on the map.
#[test]
fn a_time_locked_voice_ignores_the_tempo_map() {
    for m in [steps(), ramp()] {
        for block in [64, 1_000] {
            let out = render(&m, None, 3 * 48_000, block);
            for (f, &got) in out.iter().enumerate() {
                assert_eq!(got.to_bits(), (f as f32 / SCALE).to_bits(), "frame {f}");
            }
        }
    }
}

/// 60 BPM, then 240 from beat 2.9: a step 0.1 beat before a clip placed at beat 3.
fn late_step() -> Arc<TempoMap> {
    map(&[
        (0.0, 60.0, Transition::Step),
        (2.9, 240.0, Transition::Step),
    ])
}

/// The seconds of beat `b` of `late_step()`.
fn late_step_seconds(b: f64) -> f64 {
    b.min(2.9) * 60.0 / 60.0 + (b - 2.9).max(0.0) * 60.0 / 240.0
}

/// **A clip placed ahead of a tempo change primes in time for its start.** A stretching
/// voice primes before its window opens, `prime / 2` frames and a block ahead, and it must
/// find how far off the start is across the map's tempo changes: here the playhead is at
/// 60 BPM, and 240 BPM comes 0.1 beat before the clip (beat 3, which is at
/// `2.9 + 0.1 / 4 = 2.925` s, frame 140 400). Counted at the playhead's own tempo the
/// start looks 4 times as far off past the step, so priming would start late and the clip's
/// first frames would be silent. A hit on the clip's first source frame, through a vocoder
/// at unity, comes out on the clip's start frame, within the kernel's bound (four
/// overlapped frames, each in error by an `f32` transform pair's rounding) and nothing
/// sounds before it.
#[test]
fn a_clip_placed_ahead_of_a_tempo_change_lands_its_first_frame_on_its_start() {
    let m = late_step();
    let transport = Transport::new(SampleRate::of(SR));
    let mut wave = Wave::new(1, SampleRate::of(SR));
    for i in 0..4 * SR as usize {
        wave.append(Interleaved::new(&[if i == 0 { 1.0 } else { 0.0 }], wave.layout()).unwrap())
            .unwrap();
    }
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(MAX_BLOCK)),
    ));
    let source = MemorySource::placed(Arc::new(wave), VoiceWindow::from(Beat::new(3.0)))
        .expect("a placeable wave");
    let pool = VoicePool::new().tempo(transport.settings().tempo_reader());
    // The handle the insert hands back is the pool's live one.
    let handle = ed.insert(NodeKey(1), "voice", pool).controls;
    handle
        .add(
            SlotId(1),
            source,
            Playback {
                stretch: StretchFactor::new(1.5),
                ..Playback::default()
            },
        )
        .expect("room");
    handle
        .set_stretch(SlotId(1), StretchFactor::UNITY, Cents::new(0.0))
        .expect("room");
    ed.spec_mut().topology.set_outputs((0..2).map(|port| {
        PortSource::Node(OutPort {
            node: NodeKey(1),
            port,
        })
    }));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    transport.settings().set_tempo_map(Arc::clone(&m));
    transport.motion().send(MotionCommand::Play).expect("room");
    let frames = 3 * 48_000 + 12_000;
    let mut left = Vec::with_capacity(frames + 64);
    let mut buf = vec![0.0f32; 64 * 2];
    while left.len() < frames {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        left.extend(buf.as_chunks::<2>().0.iter().map(|f| f[0]));
    }
    let due = frame_after(late_step_seconds(3.0) * SR);
    assert_eq!(due, 140_400);
    let bound = 4.0 * 2.0 * 11.0 * f64::from(f32::EPSILON);
    for (n, &y) in left.iter().enumerate() {
        let want = if n == due { 1.0 } else { 0.0 };
        assert!(
            (f64::from(y) - want).abs() < bound,
            "frame {n}: {y}, the hit is due at {due}"
        );
    }
}
