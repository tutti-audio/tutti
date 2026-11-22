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

use tutti_buffers::{Interleaved, Planes, Wave};
use tutti_graph::contract;
use tutti_sampler::testing::{MockTransport, wait_for_open};
use tutti_sampler::{
    DiskStreamer, DiskVoice, Manual, MemorySource, Playback, SlotId, StepOutcome, StreamHandle,
    StreamStart, VoicePool, VoicePoolHandle, VoiceWindow,
};
use tutti_units::{Beat, Bpm, Cents, PlaybackRate, SampleRate, StretchFactor};

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

/// The pool, prepared, and the stretch to hold at unity once it has a vocoder.
fn pool() -> (impl tutti_graph::Node, VoicePoolHandle<f32>) {
    let (pool, handle) = VoicePool::new().with_handle();
    (contract::prepared(pool, SampleRate::of(SR), BLOCK), handle)
}

/// A stretched voice added at 1.5 and turned back to unity: it keeps its vocoder.
fn play(speed: f64) -> Playback {
    Playback {
        speed: PlaybackRate::of(speed),
        stretch: StretchFactor::new(1.5),
        ..Default::default()
    }
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
        let (mut pool, handle) = pool();
        handle
            .add(
                SlotId(1),
                MemorySource::placed(Arc::new(wave), VoiceWindow::from(Beat::new(1.0)))
                    .expect("a placeable wave"),
                play(speed),
            )
            .expect("the command queue has room in a test");
        handle
            .set_stretch(SlotId(1), StretchFactor::UNITY, Cents::new(0.0))
            .expect("the command queue has room in a test");
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
        let (mut pool, handle) = pool();
        handle
            .add_streaming(SlotId(1), voice, stream, play(speed))
            .expect("the command queue has room in a test");
        handle
            .set_stretch(SlotId(1), StretchFactor::UNITY, Cents::new(0.0))
            .expect("the command queue has room in a test");
        let out = render(&mut pool, || {
            let _ = streamer.step();
        });
        assert_onset(&out, speed, &format!("disk, speed {speed}"));
        assert_ne!(streamer.step(), StepOutcome::Busy);
    }
}
