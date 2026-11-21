//! A spectrum push allocates nothing once its meter is built.
//!
//! Counted with this binary's own global allocator, as in `chroma_alloc.rs`. It
//! counts per thread, so the harness's other threads do not show up.

use super::Discard;
use tutti_buffers::Mono;
use tutti_dsp::FrameCount;
use tutti_fixtures::allocations;
use tutti_measure::{
    Banding, BandsPerOctave, Framing, FrequencyBand, Measure, Measurement, PeakDecay, Smoothing,
    SpectrumConfig, SpectrumTime,
};
use tutti_units::{Db, Hz, SampleRate, Samples, Seconds};

/// Construction allocates (so the counter is live); then many frames across
/// block sizes that split frames, a reset, and more frames allocate nothing,
/// for bins and for bands, with smoothing, peaks and a wrapping spectrogram.
#[test]
fn a_spectrum_step_does_not_allocate() {
    let plain = SpectrumConfig::new().framing(Framing::new(Samples::new(1024), Samples::new(256)));
    let banded = plain
        .banding(Banding::Octave {
            per_octave: BandsPerOctave::of(3),
            band: FrequencyBand::new(Hz::new(400.0), Hz::new(20_000.0)).unwrap(),
        })
        .smoothing(Smoothing::Average(SpectrumTime::of(Seconds::new(0.2))))
        .peak_hold(SpectrumTime::of(Seconds::new(0.5)))
        .peak_decay(PeakDecay::of(Db::new(20.0)))
        .spectrogram(FrameCount(7));
    let signal: Vec<f32> = (0..48_000)
        .map(|i| 0.4 * (((std::f64::consts::TAU * 1234.5 * i as f64 / 48_000.0).sin()) as f32))
        .collect();

    for cfg in [plain, banded] {
        let mut state = None;
        let built = allocations(|| state = Some(cfg.measure(SampleRate::SR_48K).unwrap()));
        assert!(built > 0, "construction builds the buffers");
        let mut state = state.unwrap();

        let steps = allocations(|| {
            for size in [1, 100, 256, 777, 4096] {
                for block in signal.chunks(size) {
                    state.push(Mono::new(block), &mut Discard).unwrap();
                }
            }
        });
        assert_eq!(steps, 0, "steps");

        let after_reset = allocations(|| {
            state.reset();
            for block in signal.chunks(500) {
                state.push(Mono::new(block), &mut Discard).unwrap();
            }
            let _ = (state.levels(), state.peaks(), state.spectrogram().count());
        });
        assert_eq!(after_reset, 0, "reset and steps");
    }
}

/// A push of 16-bit audio allocates nothing either, for blocks longer than the
/// conversion block.
#[test]
fn an_integer_spectrum_step_does_not_allocate() {
    let cfg = SpectrumConfig::new().framing(Framing::new(Samples::new(1024), Samples::new(256)));
    let signal: Vec<i16> = (0..48_000)
        .map(|i| {
            ((((std::f64::consts::TAU * 1234.5 * i as f64 / 48_000.0).sin()) as f32) * 8_000.0)
                as i16
        })
        .collect();
    let mut state = cfg.measure(SampleRate::SR_48K).unwrap();
    let steps = allocations(|| {
        for block in signal.chunks(10_000) {
            state.push(Mono::new(block), &mut Discard).unwrap();
        }
    });
    assert_eq!(steps, 0, "i16 steps");
}
