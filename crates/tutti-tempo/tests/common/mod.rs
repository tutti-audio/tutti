#![allow(dead_code)]
use tutti_rt::SplitMix64;
use tutti_tempo::{TempoPoint, Transition};
use tutti_units::{Beat, Bpm, SampleRate};

pub const R: SampleRate = SampleRate::SR_48K;

pub fn pt(beat: f64, tempo: f64, shape: Transition) -> TempoPoint {
    TempoPoint {
        beat: Beat::new(beat),
        tempo: Bpm::of(tempo),
        transition: shape,
    }
}

/// A fixed-seed generator: the same sequence on every platform.
pub struct Rng(SplitMix64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(SplitMix64::new(seed))
    }

    /// Uniform in `[0, 1)`.
    pub fn next(&mut self) -> f64 {
        (self.0.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}
