//! [`FrequencyShifterBank`]: single-sideband modulation by a shift in hertz, through a
//! Hilbert phase network of two all-pass chains.
//!
//! **The network.** Two chains of four [`AllPassFilter`]s, each section
//! `H_k(z) = (z^-2 - a_k^2) / (1 - a_k^2 z^-2)` (unit magnitude), with the coefficients of
//! Olli Niemitalo's "Hilbert transform" (Polyphase IIR half-band design, 2003): `a` =
//! 0.6923878, 0.9360654322959, 0.9882295226860, 0.9987488452737 for the first chain and
//! 0.4021921162426, 0.8561710882420, 0.9722909545651, 0.9952884791278 for the second. The
//! first chain runs on the input delayed by one frame. The second chain's output leads the
//! first's by 90 degrees to within [`QUADRATURE_ERROR`] from 0.002 to 0.998 of the Nyquist
//! frequency ([`quadrature_band`]), and both have unit gain at every frequency, so the pair is
//! the analytic signal `I - jQ` of the input (a quadrature pair) to that error.
//!
//! **The shift.** With the first chain's output `I`, the second's `Q` and the carrier
//! `theta[n] = 2 pi |s| n / fs`, a shift by `s` is `y = I cos(theta) + sign(s) Q sin(theta)`, the
//! real part of `(I - jQ) e^{j sign(s) theta}`: a sine at `f` becomes one at `f + s` (at `s - f`
//! when that is negative, folding through zero), and the image the network's quadrature error
//! leaves is at `s - f`, at most `tan(error / 2)` of the wanted sine (-44 dB).
//!
//! The network is a phase network, not a delay, so it reports no latency. It rings for the
//! slowest pole's decay ([`tail`](FrequencyShifterBank::tail)).

use core::num::{NonZeroU16, NonZeroUsize};
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_units::{Feedback, Hz, Phase, Radians, SampleRate, Samples, Tail};

use crate::delay::{ring_seconds, trips};
use crate::{AllPassFilter, AllPassForm, Oscillator, OscillatorStep, OscillatorWave, TAIL_FLOOR};

/// The coefficients `a_k` of the first chain: the sections are `a_k^2` in `z^-2`.
const FIRST: [f64; 4] = [
    0.692_387_8,
    0.936_065_432_295_9,
    0.988_229_522_686_0,
    0.998_748_845_273_7,
];
/// The coefficients `a_k` of the second chain.
const SECOND: [f64; 4] = [
    0.402_192_116_242_6,
    0.856_171_088_242_0,
    0.972_290_954_565_1,
    0.995_288_479_127_8,
];

/// The most the network's phase difference is from 90 degrees anywhere in [`quadrature_band`]:
/// 0.7032 degrees, rounded up to 0.01228 radians.
pub const QUADRATURE_ERROR: Radians = Radians::new(0.012_28);

/// The band the network holds [`QUADRATURE_ERROR`] over, at `rate`: 0.002 to 0.998 of the
/// Nyquist frequency.
#[must_use]
pub fn quadrature_band(rate: SampleRate) -> (Hz, Hz) {
    (rate.nyquist_scaled(0.002), rate.nyquist_scaled(0.998))
}

/// The controls of a [`FrequencyShifterBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrequencyShifterControls {
    /// How far every frequency moves: up when positive, down when negative. At most the
    /// Nyquist frequency in magnitude; one that is not finite holds the carrier.
    pub shift: Hz,
}

impl FrequencyShifterControls {
    /// No shift.
    pub const DEFAULT: Self = Self { shift: Hz::ZERO };
}

impl Default for FrequencyShifterControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One channel's network: the two chains and the frame the first one is late by.
#[derive(Clone, Debug)]
struct Network<F: Float> {
    first: [AllPassFilter<F>; 4],
    second: [AllPassFilter<F>; 4],
    held: F,
}

/// The delay of each all-pass of the Hilbert network, in frames.
const TWO: NonZeroUsize = NonZeroUsize::new(2).unwrap();

impl<F: Float> Network<F> {
    fn new() -> Self {
        let chain = |a: [f64; 4]| {
            a.map(|a| AllPassFilter::new(AllPassForm::Lattice, TWO, Feedback::new(a * a)))
        };
        Self {
            first: chain(FIRST),
            second: chain(SECOND),
            held: F::ZERO,
        }
    }

    fn reset(&mut self) {
        self.first.iter_mut().for_each(AllPassFilter::reset);
        self.second.iter_mut().for_each(AllPassFilter::reset);
        self.held = F::ZERO;
    }

    /// One frame in, the quadrature pair `(I, Q)` out.
    #[inline]
    fn tick(&mut self, x: F) -> (F, F) {
        let late = core::mem::replace(&mut self.held, x);
        let i = self.first.iter_mut().fold(late, |y, ap| ap.tick(y));
        let q = self.second.iter_mut().fold(x, |y, ap| ap.tick(y));
        (i, q)
    }
}

/// A frequency shifter of any width: one network per channel, one carrier across them.
///
/// Built on the control side, where it allocates the networks; nothing after that does.
#[derive(Clone, Debug)]
pub struct FrequencyShifterBank<F: Float> {
    rate: SampleRate,
    networks: Vec<Network<F>>,
    sine: Oscillator,
    cosine: Oscillator,
    controls: FrequencyShifterControls,
}

impl<F: Float> FrequencyShifterBank<F> {
    /// A silent shifter of `width` channels at `rate`, not shifting, the
    /// carrier at phase 0. Allocates.
    #[must_use]
    pub fn new(width: NonZeroU16, rate: SampleRate) -> Self {
        Self {
            rate,
            networks: (0..width.get()).map(|_| Network::new()).collect(),
            sine: Oscillator::new(OscillatorWave::Sine),
            cosine: Oscillator::at(OscillatorWave::Sine, Phase::new(0.25)),
            controls: FrequencyShifterControls::DEFAULT,
        }
    }

    /// Takes `controls`; the carrier's phase is kept. No allocation.
    pub fn retarget(&mut self, controls: &FrequencyShifterControls) {
        self.controls = *controls;
    }

    /// Back to silence and carrier phase 0. No allocation.
    pub fn reset(&mut self) {
        self.networks.iter_mut().for_each(Network::reset);
        self.sine = Oscillator::new(OscillatorWave::Sine);
        self.cosine = Oscillator::at(OscillatorWave::Sine, Phase::new(0.25));
    }

    /// How long it rings after its input stops, to [`TAIL_FLOOR`], erring long: each section's
    /// echoes (`1 + trips` periods of its two frames) summed over the slower chain, and the
    /// frame the chain is late by.
    #[must_use]
    pub fn tail(&self) -> Tail {
        let chain = FIRST.iter().map(|a| {
            ring_seconds(2, trips(a * a, TAIL_FLOOR), self.rate).to_samples_ceil(self.rate)
        });
        Tail::Finite(chain.fold(Samples::new(1), |sum, s| Samples::new(sum.get() + s.get())))
    }

    /// Runs `input` into `output`, the fewer of the two views' channels and frames. No
    /// allocation.
    pub fn process(&mut self, input: Planes<'_, F>, mut output: PlanesMut<'_, '_, F>) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input
            .channels()
            .min(output.channels())
            .min(self.networks.len());
        let shift = self.controls.shift;
        let step = OscillatorStep::<F>::new(Hz::new(shift.get().abs()), self.rate);
        let side = if shift.get() < 0.0 { -F::ONE } else { F::ONE };
        let (sine, cosine) = (self.sine, self.cosine);
        let (mut end_sine, mut end_cosine) = (sine, cosine);
        for (c, network) in self.networks.iter_mut().enumerate().take(width) {
            let (mut sin, mut cos) = (sine, cosine);
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            for (y, &x) in plane.samples_mut()[..frames].iter_mut().zip(x) {
                let (i, q) = network.tick(x);
                *y = i * cos.step(step) + side * q * sin.step(step);
            }
            (end_sine, end_cosine) = (sin, cos);
        }
        (self.sine, self.cosine) = (end_sine, end_cosine);
    }
}

#[cfg(test)]
mod tests;
