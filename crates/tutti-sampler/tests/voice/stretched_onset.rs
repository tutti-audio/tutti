//! A stretched voice sounds from the frame its beat falls on, from either tier.
//!
//! The voice seats its vocoder on the source's own history (silence before frame 0,
//! which is its true signal) and primes ahead of its window, so a hit on the first
//! frame of a clip placed at beat 1 (frame 24 000 at 120 BPM and 48 kHz) is the first
//! non-silent frame the voice sounds, there, at any varispeed. The vocoder stays at
//! unity (a voice turned back to 1.0 keeps it), where it is the identity within the
//! transforms' rounding: four overlapped frames, each in error by it.
//!
//! A disk voice reads the stream's ring, hand-stepped by the test beside each block, and
//! declares no latency.

use std::path::Path;
use std::sync::Arc;

use tutti_buffers::{ChannelLayout, Interleaved, Planes, Wave};
use tutti_graph::{Published, contract};
use tutti_sampler::testing::{MockTransport, controlled_pool, placed_config, wait_for_open};
use tutti_sampler::{
    DiskStreamer, DiskVoice, Manual, Row, SlotId, StepOutcome, StreamHandle, StreamStart,
    VoiceNodeConfig, VoicePool, VoicePoolConfig, VoicePoolControls, VoiceWindow,
};
use tutti_units::{
    Amplitude, Beat, BeatDuration, Bpm, Cents, PlaybackRate, SamplePosition, SampleRate,
    StretchFactor,
};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
/// Beat 1 at 120 BPM.
const DUE: usize = 24_000;
const FRAMES: usize = DUE + 6_000;
const SPEEDS: [f64; 3] = [0.5, 1.0, 2.0];

/// The second hit's source frame: it lands `SECOND / speed` frames after the first.
const SECOND: usize = 100;

/// A hit on the first frame of a stereo clip and another on frame `SECOND`, in a clip long
/// enough for any speed.
fn hit() -> Vec<f32> {
    let mut samples = vec![0.0f32; 2 * 30_000];
    for frame in [0, SECOND] {
        samples[2 * frame] = 1.0;
        samples[2 * frame + 1] = 1.0;
    }
    samples
}

/// What four overlapped `f32` transform pairs cost (`2 log2(2048)` epsilons each).
fn bound() -> f64 {
    4.0 * 2.0 * 11.0 * f64::from(f32::EPSILON)
}

/// A pool over `row` under key 1, prepared, and its controls.
fn pool(row: Row<f32>) -> (VoicePool<f32>, VoicePoolControls<f32>) {
    let (pool, controls) = controlled_pool(VoicePool::from(table(row)));
    (
        contract::prepared(pool, SampleRate::of(SR), BLOCK),
        controls,
    )
}

/// The pool config holding `row` under key 1.
fn table(row: Row<f32>) -> VoicePoolConfig<f32> {
    VoicePoolConfig::new(ChannelLayout::STEREO)
        .expect("stereo is a width a pool runs")
        .row(SlotId(1), row)
        .expect("one row in a free key")
}

/// A stretching row at unity: it owns its vocoder, where it is the identity within the
/// transforms' rounding.
fn unity(row: impl Into<Row<f32>>) -> Row<f32> {
    row.into().stretching(StretchFactor::UNITY, Cents::new(0.0))
}

/// Renders `FRAMES` frames of the pool's left channel under a transport rolling from beat
/// 0, `between` run after each block (the test's streamer).
fn render(pool: &mut impl tutti_graph::Node, mut between: impl FnMut()) -> Vec<f32> {
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut out = Vec::new();
    while out.len() < FRAMES {
        let block = contract::drive_input(
            pool,
            &transport.env(BLOCK, SampleRate::of(SR)),
            Planes::default(),
            &[],
            &[],
        )
        .audio;
        out.extend_from_slice(&block[0]);
        transport.advance(BLOCK as i64, SampleRate::of(SR));
        between();
    }
    out
}

/// Nothing sounds before `DUE`, the first hit does on it, and the second `SECOND / speed`
/// frames later (the voice's varispeed is what puts it there).
fn assert_onset(out: &[f32], speed: f64, what: &str) {
    assert!(
        out[..DUE].iter().all(|&x| x == 0.0),
        "{what}: sounded before frame {DUE}"
    );
    assert!(
        (f64::from(out[DUE]) - 1.0).abs() < bound(),
        "{what}: frame {DUE} is {}, the hit",
        out[DUE]
    );
    let second = DUE + (SECOND as f64 / speed) as usize;
    assert!(
        (f64::from(out[second]) - 1.0).abs() < bound(),
        "{what}: frame {second} is {}, the second hit",
        out[second]
    );
}

/// A memory voice placed at beat 1 sounds its first frame on frame 24 000, at every
/// varispeed, and its second hit where the varispeed puts it.
#[test]
fn a_memory_voice_sounds_from_the_frame_its_beat_falls_on() {
    for speed in SPEEDS {
        let mut wave = Wave::new(2, SampleRate::of(SR));
        wave.append(Interleaved::new(&hit(), wave.layout()).unwrap())
            .unwrap();
        let voice = VoiceNodeConfig::placed(placed_config(
            Arc::new(wave),
            VoiceWindow::from(Beat::new(1.0)),
        ))
        .expect("a finite gain")
        .speed(PlaybackRate::of(speed));
        let (mut pool, _controls) = pool(unity(voice));
        assert_onset(
            &render(&mut pool, || {}),
            speed,
            &format!("memory, speed {speed}"),
        );
    }
}

/// The `hit` file, a hand-stepped streamer on it with the stream primed, and the voice of
/// its stream placed at beat 1.
fn disk_voice(
    path: &Path,
) -> (
    DiskStreamer<Manual<f32>, f32>,
    StreamHandle<f32>,
    DiskVoice<f32>,
) {
    tutti_fixtures::signal::write_wav_f32(path, SampleRate::of(SR), 2, &hit());
    let mut streamer =
        DiskStreamer::manual(SampleRate::of(SR), Default::default()).expect("builds");
    let opening = streamer.open_stream(path, StreamStart::HEAD);
    let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    assert!(streamer.step_until_idle(1_000) < 1_000, "the ring primes");
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(1.0)));
    (streamer, stream, voice)
}

/// A disk voice placed at beat 1 sounds its first frame on frame 24 000, at every
/// varispeed, with the ring read ahead of its seat.
#[test]
fn a_disk_voice_sounds_from_the_frame_its_beat_falls_on() {
    for speed in SPEEDS {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (mut streamer, stream, voice) = disk_voice(&dir.path().join("hit.wav"));
        let voice = VoiceNodeConfig::streaming(
            voice,
            stream,
            Amplitude::new(1.0),
            StretchFactor::UNITY,
            Cents::new(0.0),
        )
        .expect("a finite gain and a nonempty width")
        .speed(PlaybackRate::of(speed));
        let (mut pool, _controls) = pool(unity(voice));
        let out = render(&mut pool, || {
            let _ = streamer.step();
        });
        assert_onset(&out, speed, &format!("disk, speed {speed}"));
        assert_ne!(streamer.step(), StepOutcome::Busy);
    }
}

/// A voice that leaves its window and re-enters it reseats: a clip with a window of a quarter
/// beat (6 000 frames) from beat 0 is silent after it, and when its window is moved to open at
/// beat 1 (frame 24 000) while the transport is past the old end, the voice primes ahead of
/// the new window and sounds its first hit on frame 24 000 and its second a varispeed of
/// `SECOND` frames later, with nothing between the window's end and that frame. A voice that
/// kept its old seat would read on from where it left off.
#[test]
fn a_voice_that_leaves_its_window_and_re_enters_sounds_from_the_re_entry_frame() {
    let mut wave = Wave::new(2, SampleRate::of(SR));
    wave.append(Interleaved::new(&hit(), wave.layout()).unwrap())
        .unwrap();
    let voice = VoiceNodeConfig::placed(placed_config(
        Arc::new(wave),
        VoiceWindow::span(Beat::new(0.0), BeatDuration::new(0.25)),
    ))
    .expect("a finite gain");
    let (mut pool, controls) = pool(unity(voice.clone()));
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut out = Vec::new();
    while out.len() < FRAMES {
        if out.len() == 12_032 {
            let moved = unity(voice.clone().window(VoiceWindow::from(Beat::new(1.0))));
            controls.replace(table(moved).table());
        }
        let block = contract::drive_input(
            &mut pool,
            &transport.env(BLOCK, SampleRate::of(SR)),
            Planes::default(),
            &[],
            &[],
        )
        .audio;
        out.extend_from_slice(&block[0]);
        transport.advance(BLOCK as i64, SampleRate::of(SR));
    }
    assert!(
        out[6_000..DUE].iter().all(|&x| x == 0.0),
        "sounded between the window's end and its re-entry"
    );
    for frame in [DUE, DUE + SECOND] {
        assert!(
            (f64::from(out[frame]) - 1.0).abs() < bound(),
            "frame {frame} is {}, a hit",
            out[frame]
        );
    }
}

/// A disk voice whose ring has not caught up plays on time with a gap, and counts the
/// underrun. The stream opens 30 000 frames into the file, so the voice's reads (from frame 0)
/// fall outside the ring's window; the streamer is not serviced until frame 24 000, so the
/// voice's priming reads miss, and it runs on regardless (it never waits). Once the streamer
/// runs, a tone that starts 4 000 frames into the file starts `4 000 x 1.5` frames after
/// frame 24 000, within the edge's smear and the 128 frames the RMS is read in, and the
/// handle's status shows the missed frames.
#[test]
fn a_disk_voice_whose_ring_has_not_caught_up_plays_on_time_and_counts_an_underrun() {
    use tutti_graph::Node;
    use tutti_sampler::VoiceNode;
    const AT: usize = 4_000;
    const RES: usize = 128;
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("tone.wav");
    let tone: Vec<f32> = (0..90_000)
        .flat_map(|i| {
            let x = if i >= AT {
                0.5 * (std::f64::consts::TAU * 1_000.0 * i as f64 / SR).sin()
            } else {
                0.0
            };
            [x as f32; 2]
        })
        .collect();
    tutti_fixtures::signal::write_wav_f32(&path, SampleRate::of(SR), 2, &tone);
    let mut streamer =
        DiskStreamer::manual(SampleRate::of(SR), Default::default()).expect("builds");
    let start = StreamStart::new(SamplePosition::new(30_000.0)).expect("a position in the file");
    let opening = streamer.open_stream(&path, start);
    let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    assert!(streamer.step_until_idle(1_000) < 1_000, "the ring primes");
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(1.0)));
    let (node, handle) = tutti_sampler::testing::controlled(VoiceNode::from(
        VoiceNodeConfig::streaming(
            voice,
            stream,
            tutti_units::Amplitude::new(1.0),
            StretchFactor::new(1.5),
            tutti_units::Cents::new(0.0),
        )
        .expect("a finite gain and a nonempty width"),
    ));
    let mut node = contract::prepared(node, SampleRate::of(SR), BLOCK);
    assert_eq!(node.shape().latency.samples(), tutti_units::Samples::new(0));
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut out = Vec::new();
    while out.len() < DUE + 12_000 {
        let block = contract::drive_input(
            &mut node,
            &transport.env(BLOCK, SampleRate::of(SR)),
            Planes::default(),
            &[],
            &[],
        )
        .audio;
        out.extend_from_slice(&block[0]);
        transport.advance(BLOCK as i64, SampleRate::of(SR));
        if out.len() >= DUE {
            let _ = streamer.step();
        }
    }
    assert!(
        handle.status().expect("a stream").underruns > 0,
        "the voice found the ring short"
    );
    assert!(
        out[..DUE].iter().all(|&x| x == 0.0),
        "sounded before frame {DUE}"
    );
    let half = 0.5 / 2.0f64.sqrt() / 2.0;
    let first = out[DUE..]
        .as_chunks::<RES>()
        .0
        .iter()
        .position(|block| {
            (block.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / RES as f64).sqrt() >= half
        })
        .expect("the tone sounds once the ring catches up")
        * RES
        + RES / 2;
    let want = AT as f64 * 1.5;
    let bound = tutti_fixtures::signal::stretched_edge_spread(
        StretchFactor::new(1.5),
        tutti_units::Samples::new(2_048),
    )
    .get()
        + RES;
    assert!(
        (first as f64 - want).abs() <= bound as f64,
        "the tone starts {first} frames in, want {want} within {bound}"
    );
}
