//! [`VelvetNoise`] and the [`Decorrelator`] built on it.
//!
//! Velvet noise (Karjalainen and Jarvelainen 2007) is a sparse sequence of `+1` and `-1`
//! impulses: one impulse in each period `Td = fs / density` frames, at the position
//! `round(k Td + r_k (Td - 1))` for the period `k` and a uniform `r_k` in `[0, 1)`, with a sign
//! from a fair coin. The positions fall in `[round(k Td), round((k + 1) Td) - 1]`, so they
//! are distinct and rise with `k`. A sequence of `length` seconds holds `floor(length * density)`
//! impulses (at least one).
//!
//! A decorrelator (Alary, Politis and Valimaki 2017) is a sparse FIR on each channel whose taps
//! are a velvet sequence under a decaying envelope: tap `k` is `s_k e(m_k)` at the position
//! `m_k`, with `e` falling 60 dB over the length, scaled by `1 / sqrt(sum e(m_k)^2)` so that
//! white noise keeps its power: the energy gain `sum h^2` is exactly 1. Each channel draws its
//! own sequence from the seed, and two sequences' zero-lag inner product, `sum h_a[n] h_b[n]`,
//! is the correlation of their outputs to one white input. The convolution visits only the
//! nonzero taps.

use core::num::NonZeroU16;
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_rt::SplitMix64;
use tutti_units::{Db, Gain, Hz, SampleRate, SampleSpan, Samples, Seconds};

use crate::delay::DelayRing;

tutti_units::ranged! {
    /// The impulses a second of velvet noise holds: `1..=20000`. Above the sample rate a
    /// sequence has one impulse a frame.
    VelvetDensity(Hz), VelvetDensityError, "a density from 1 to 20000 impulses a second",
    |raw| raw.get() >= 1.0 && raw.get() <= 20_000.0
}

impl VelvetDensity {
    /// 2000 impulses a second: Alary et al.'s density for a decorrelator, past the point
    /// where the sequence sounds like noise rather than a train of clicks.
    pub const DEFAULT: Self = Self(Hz::new(2_000.0));
}

impl Default for VelvetDensity {
    fn default() -> Self {
        Self::DEFAULT
    }
}

tutti_units::ranged! {
    /// How long a velvet sequence is: above zero and at most one second.
    VelvetLength(Seconds), VelvetLengthError, "a length above 0 and at most 1 second",
    |raw| raw.get() > 0.0 && raw.get() <= 1.0
}

impl VelvetLength {
    /// 50 ms: 100 impulses at the default density.
    pub const DEFAULT: Self = Self(Seconds::new(0.05));
}

impl Default for VelvetLength {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One impulse of a [`VelvetNoise`] sequence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VelvetPulse {
    /// The frame it sits on.
    pub position: Samples,
    /// Its sign: [`Gain::UNITY`] or [`Gain::INVERTING`].
    pub sign: Gain,
}

/// A velvet-noise sequence: the impulses in order of position.
#[derive(Clone, Debug, PartialEq)]
pub struct VelvetNoise {
    pulses: Vec<VelvetPulse>,
    /// The length in frames, which every position is below.
    length: Samples,
}

impl VelvetNoise {
    /// The sequence of `length` at `density` and `rate` that the stream `seed` names: for each
    /// period, one word for the position and one for the sign, in order. Allocates.
    #[must_use]
    pub fn new(density: VelvetDensity, length: VelvetLength, rate: SampleRate, seed: u64) -> Self {
        // A density past the rate is one impulse a frame: `Td >= 1` keeps positions distinct.
        let density = density.get().min(Hz::new(rate.get()));
        let period = density
            .period()
            .and_then(|t| SampleSpan::from_seconds(t, rate))
            .map_or(1.0, SampleSpan::get);
        let count = (libm::floor(length.get().get() * density.get()) as usize).max(1);
        let mut rng = SplitMix64::new(seed);
        let pulses = (0..count)
            .map(|k| {
                let r = f64::from_fraction_bits(rng.next_u64());
                let position = libm::round(k as f64 * period + r * (period - 1.0)) as usize;
                let sign = if rng.next_u64() >> 63 == 0 {
                    Gain::UNITY
                } else {
                    Gain::INVERTING
                };
                VelvetPulse {
                    position: Samples::new(position),
                    sign,
                }
            })
            .collect();
        Self {
            pulses,
            length: length.get().to_samples_ceil(rate),
        }
    }

    /// The impulses, in order of position.
    #[must_use]
    pub fn pulses(&self) -> &[VelvetPulse] {
        &self.pulses
    }

    /// The length in frames: every impulse is before it.
    #[must_use]
    pub fn length(&self) -> Samples {
        self.length
    }
}

/// One channel's sparse FIR: its history ring and the taps, by distance back.
#[derive(Clone, Debug)]
struct Channel<F: Float> {
    ring: DelayRing<F>,
    /// `(frames back, gain)`, the position and the signed, enveloped, normalized tap.
    taps: Vec<(usize, F)>,
}

/// A velvet-noise decorrelator of any width: each channel through its own sparse FIR.
///
/// Built on the control side, where it allocates the rings and the taps; nothing it does
/// after that allocates.
#[derive(Clone, Debug)]
pub struct Decorrelator<F: Float> {
    sequences: Vec<VelvetNoise>,
    channels: Vec<Channel<F>>,
    tail: Samples,
}

impl<F: Float> Decorrelator<F> {
    /// A decorrelator of `width` channels at `rate`. Channel `c`'s
    /// sequence is [`VelvetNoise::new`] of the `c`-th word of `seed`'s stream. Allocates.
    #[must_use]
    pub fn new(
        width: NonZeroU16,
        density: VelvetDensity,
        length: VelvetLength,
        rate: SampleRate,
        seed: u64,
    ) -> Self {
        let mut seeds = SplitMix64::new(seed);
        let sequences: Vec<VelvetNoise> = (0..width.get())
            .map(|_| VelvetNoise::new(density, length, rate, seeds.next_u64()))
            .collect();
        let frames = length.get().get() * rate.get();
        let channels: Vec<Channel<F>> = sequences
            .iter()
            .map(|s| {
                let envelope = |p: &VelvetPulse| {
                    Db::new(-60.0 * p.position.get() as f64 / frames)
                        .to_amplitude()
                        .get()
                };
                let norm = 1.0
                    / s.pulses
                        .iter()
                        .map(|p| envelope(p).powi(2))
                        .sum::<f64>()
                        .sqrt();
                let last = s.pulses.last().map_or(0, |p| p.position.get());
                Channel {
                    ring: DelayRing::new(last + 2),
                    taps: s
                        .pulses
                        .iter()
                        .map(|p| {
                            let gain = p.sign.get() * envelope(p) * norm;
                            (p.position.get() + 1, F::from_design(gain))
                        })
                        .collect(),
                }
            })
            .collect();
        let tail = sequences
            .iter()
            .filter_map(|s| s.pulses.last())
            .map(|p| p.position)
            .max()
            .unwrap_or(Samples::ZERO);
        Self {
            sequences,
            channels,
            tail,
        }
    }

    /// Channel `c`'s velvet sequence, before its envelope and scale.
    #[must_use]
    pub fn sequence(&self, c: usize) -> Option<&VelvetNoise> {
        self.sequences.get(c)
    }

    /// How long it rings after its input stops: the latest impulse of any channel.
    #[must_use]
    pub fn tail(&self) -> Samples {
        self.tail
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.ring.clear();
        }
    }

    /// Runs `input` into `output`, channel by channel: the fewer of the two views' channels
    /// and frames. No allocation.
    pub fn process(&mut self, input: Planes<'_, F>, mut output: PlanesMut<'_, '_, F>) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input
            .channels()
            .min(output.channels())
            .min(self.channels.len());
        for (c, channel) in self.channels.iter_mut().enumerate().take(width) {
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            for (y, &x) in plane.samples_mut()[..frames].iter_mut().zip(x) {
                channel.ring.push(x);
                let mut sum = F::ZERO;
                for &(back, gain) in &channel.taps {
                    sum += gain * channel.ring.back(back);
                }
                *y = sum;
            }
        }
    }
}

#[cfg(test)]
mod tests;
