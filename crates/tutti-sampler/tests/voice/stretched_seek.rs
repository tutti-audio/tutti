//! A stretched memory voice sounds the source, in full, as soon as its priming is done
//! after a seek.
//!
//! A seek unseats the vocoder, so pre-jump audio does not smear over the new
//! region, and the seat is primed: the vocoder runs a window of output frames from
//! source read before the seek's target, and discards them, over a few blocks (silent
//! meanwhile). Unprimed, a voice would ramp in over a window. `verify-audio`'s `seek_while_stretched` case
//! heard that; this pins the primed voice in the suite.

use tutti_buffers::Interleaved;
use tutti_buffers::Planes;
use tutti_sampler::VoiceWindow;

use std::f32::consts::TAU;
use std::sync::Arc;
use tutti_fixtures::signal::rms;

use tutti_buffers::Wave;
use tutti_graph::contract;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::{MemorySource, Playback, SlotId, VoicePool};
use tutti_units::{Beat, Bpm, SampleRate, StretchFactor};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;

/// Left channel of a 2x-stretched 440 Hz voice, seeked to beat 5 at the
/// block starting on frame `seek_at`.
fn render(frames: usize, seek_at: usize) -> Vec<f32> {
    let (pool, handle) = VoicePool::new().with_handle();
    let mut pool = contract::prepared(pool, SampleRate::of(SR), BLOCK);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut wave = Wave::new(1, SampleRate::of(SR));
    for i in 0..frames * 4 {
        wave.append(
            Interleaved::new(&[(TAU * 440.0 * i as f32 / SR as f32).sin()], wave.layout()).unwrap(),
        )
        .unwrap();
    }
    handle
        .add(
            SlotId(1),
            MemorySource::placed(Arc::new(wave), VoiceWindow::from(Beat::new(0.0)))
                .expect("a placeable wave"),
            Playback {
                stretch: StretchFactor::new(2.0),
                ..Default::default()
            },
        )
        .expect("the command queue has room in a test");

    let mut out = Vec::with_capacity(frames);
    while out.len() < frames {
        if out.len() == seek_at {
            transport.seek(Beat::new(5.0));
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
    out
}

/// After a seek the voice is silent for exactly the priming the closed form gives, then
/// plays the sine at its own level.
///
/// The seat owes a window of output frames (2 048), each frame that goes by adds one and
/// the priming runs `PRIME_SPEED` a frame, so the voice sounds `priming()` frames after
/// the seek: the frames before are exactly zero. From there the vocoder is full (the
/// priming ran a window ahead), so the next 2 048 frames have the
/// sine's RMS, `amplitude / sqrt(2)`, to within 1 dB (two overlapped frames' phase
/// mismatch of `acos(10^(-1/20)) = 0.48` rad) with no ramp: a voice left to fill from
/// silence would be 3 dB down over them.
#[test]
fn a_stretched_voice_sounds_the_sine_after_exactly_its_priming() {
    let seek_at = 375 * BLOCK; // 24 000
    let out = render(32_000, seek_at);
    let silent = crate::vocoder_voice::priming();
    assert!(
        out[seek_at..seek_at + silent].iter().all(|&x| x == 0.0),
        "silent for the {silent} frames of priming"
    );
    let after = rms(&out[seek_at + silent..seek_at + silent + 2_048]);
    let want = 1.0 / 2.0f32.sqrt();
    let db = 20.0 * (after / want).log10();
    assert!(
        db.abs() < 1.0,
        "rms {after} for a sine of rms {want} after the priming ({db} dB)"
    );
}
