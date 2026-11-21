//! Whether a windowed-loudness step allocates once its state is built.
//!
//! `ebur128` does not document it, so this counts: it feeds block-sized, odd and
//! empty chunks and reads both windows, with the crate's own global allocator
//! as in `chroma_alloc.rs`. It counts per thread, so the harness's
//! other threads do not show up.

use super::Discard;
use tutti_buffers::Interleaved;
use tutti_buffers::{Bed, ChannelLayout};
use tutti_fixtures::allocations;
use tutti_measure::{Measure, StreamMeter, WindowedLoudnessConfig};
use tutti_units::SampleRate;

fn stereo(s: &[f32]) -> Interleaved<'_> {
    Interleaved::new(s, ChannelLayout::STEREO).unwrap()
}

/// Pushes after construction allocate nothing, the first included: block-sized chunks, chunks of
/// an odd frame count, an empty chunk, and a reset.
///
/// This is a measurement, not a guarantee of `ebur128`: it holds for the
/// version in `Cargo.lock`.
#[test]
fn a_windowed_loudness_step_does_not_allocate() {
    let cfg = WindowedLoudnessConfig::new(Bed::Stereo.topology());
    let mut state = cfg.measure(SampleRate::SR_48K).unwrap();
    let tone: Vec<f32> = (0..48_000 * 2)
        .map(|i| 0.1 * (std::f32::consts::TAU * 997.0 * (i / 2) as f32 / 48_000.0).sin())
        .collect();

    // The first step too, and then enough to fill the three-second window so
    // every getter reads a full one.
    let first = allocations(|| {
        for _ in 0..4 {
            state.push(stereo(&tone), &mut Discard).unwrap();
        }
    });
    assert_eq!(first, 0, "the first steps, which fill the windows");

    let aligned = allocations(|| {
        for chunk in tone.chunks(512) {
            state.push(stereo(chunk), &mut Discard).unwrap();
        }
    });
    assert_eq!(aligned, 0, "block-sized chunks");

    let odd = allocations(|| {
        for chunk in tone.chunks(777 * 2) {
            state.push(stereo(chunk), &mut Discard).unwrap();
        }
    });
    assert_eq!(odd, 0, "chunks of 777 frames");

    let empty = allocations(|| {
        state.push(stereo(&[]), &mut Discard).unwrap();
    });
    assert_eq!(empty, 0, "an empty chunk");

    let reset = allocations(|| {
        state.reset();
        state.push(stereo(&tone), &mut Discard).unwrap();
    });
    assert_eq!(reset, 0, "a reset and a step");
}

/// A push of 16-bit frames allocates nothing either, for a chunk longer than
/// the conversion block.
#[test]
fn an_integer_windowed_loudness_step_does_not_allocate() {
    let cfg = WindowedLoudnessConfig::new(Bed::Stereo.topology());
    let mut state = cfg.measure(SampleRate::SR_48K).unwrap();
    let tone: Vec<i16> = (0..48_000 * 2)
        .map(|i| {
            ((std::f32::consts::TAU * 997.0 * (i / 2) as f32 / 48_000.0).sin() * 3_000.0) as i16
        })
        .collect();
    let steps = allocations(|| {
        for _ in 0..4 {
            state
                .push(
                    Interleaved::new(&tone, ChannelLayout::STEREO).unwrap(),
                    &mut Discard,
                )
                .unwrap();
        }
    });
    assert_eq!(steps, 0, "i16 steps");
}
