//! [`KarplusStrong`]: a plucked string as a delay loop with a two-point-average loss.
//!
//! The loop is `y[n] = x[n] + rho A(z) (y[n - N] + y[n - N - 1]) / 2`: a delay of `N` whole
//! frames, the two-point average (magnitude `cos(w / 2)`, a delay of exactly half a frame at
//! every frequency) and the all-pass `A` that supplies the fraction. The input `x` is the
//! excitation, summed into the loop, and `y` is its output.
//!
//! The loop is `P = N + 1/2 + d` frames long, `d` the phase delay of `A` at the fundamental
//! (see the module of the shared tuning), so the string sounds `fs / P` exactly. Per period the
//! fundamental falls by the decay factor `rho` and by the average's `cos(w0 / 2)`; `rho` is the
//! gain that falls 60 dB in the decay time, `rho = exp(-ln(1000) P / (fs T60))`, held under
//! [`Feedback::MAX_STABLE`](tutti_units::Feedback::MAX_STABLE). The loss filter shortens the
//! decay of the upper partials more than the fundamental's, so the tone darkens as it rings.
//!
//! A frequency is held between the string's lowest (which sizes the ring) and `fs / 6`.
//! A control that moves the frequency re-tunes the loop at once: the delay line's length jumps
//! and the loop's content is kept, so a sweep is not click-free.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_units::{Db, Feedback, Hz, SampleRate, SampleSpan, Samples, Seconds, Tail};

use crate::delay::{DelayRing, ring_seconds, trips};
use crate::string_tuning::{Tuned, per_trip_decay, period, tune};
use crate::{AllPassFilter, AllPassForm, TAIL_FLOOR};

/// The controls of a [`KarplusStrong`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KarplusStrongControls {
    /// The pitch. One that is not finite and above zero reads as the string's lowest.
    pub frequency: Hz,
    /// The time the loop's gain takes to fall 60 dB, the loss filter apart. One that is not
    /// above zero is a loop that does not ring.
    pub decay: Seconds,
}

impl KarplusStrongControls {
    /// A string at 220 Hz that rings for 2 s.
    pub const DEFAULT: Self = Self {
        frequency: Hz::new(220.0),
        decay: Seconds::new(2.0),
    };
}

impl Default for KarplusStrongControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The delay of the two-point average: half a frame.
const AVERAGE_DELAY: f64 = 0.5;

/// One plucked string over the float `F`.
///
/// Built on the control side for one rate and one lowest frequency, where it allocates its
/// ring; nothing after that does.
#[derive(Clone, Debug)]
pub struct KarplusStrong<F: Float> {
    rate: SampleRate,
    lowest: Hz,
    ring: DelayRing<F>,
    all_pass: AllPassFilter<F>,
    tuned: Tuned,
    decay: F,
}

impl<F: Float> KarplusStrong<F> {
    /// A silent string at [`KarplusStrongControls::DEFAULT`], whose pitch goes down to
    /// `lowest` at `rate`. Allocates.
    #[must_use]
    pub fn new(lowest: Hz, rate: SampleRate) -> Self {
        let longest = period(Hz::ZERO, lowest, rate, 1);
        let frames = tune(longest, SampleSpan::of(AVERAGE_DELAY), 1).whole.get() + 2;
        let mut string = Self {
            rate,
            lowest,
            ring: DelayRing::new(frames),
            all_pass: AllPassFilter::new(AllPassForm::Lattice, Samples::new(1), Feedback::NONE),
            tuned: Tuned {
                whole: Samples::new(1),
                delay: SampleSpan::of(1.0),
                gain: Feedback::NONE,
            },
            decay: F::ZERO,
        };
        string.retarget(&KarplusStrongControls::DEFAULT);
        string
    }

    /// The loop's split at the controls last taken: the delay line's whole frames `N` and the
    /// all-pass's coefficient `g` (`H(z) = (z^-1 - g) / (1 - g z^-1)`).
    #[must_use]
    pub fn tuning(&self) -> (Samples, Feedback) {
        (self.tuned.whole, self.tuned.gain)
    }

    /// Takes `controls`; the loop's content is kept. No allocation.
    pub fn retarget(&mut self, controls: &KarplusStrongControls) {
        let period = period(controls.frequency, self.lowest, self.rate, 1);
        self.tuned = tune(period, SampleSpan::of(AVERAGE_DELAY), 1);
        self.all_pass.retarget(self.tuned.gain);
        let trip = period.to_seconds(self.rate);
        self.decay = F::from_unit(per_trip_decay(trip, controls.decay));
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.ring.clear();
        self.all_pass.reset();
    }

    /// One frame in, one out.
    #[inline]
    pub fn tick(&mut self, x: F) -> F {
        let n = self.tuned.whole.get();
        let average = (self.ring.back(n) + self.ring.back(n + 1)) * F::lit(0.5);
        let y = x + self.decay * self.all_pass.tick(average);
        self.ring.push(y);
        y
    }

    /// How long it rings after its input stops, to [`TAIL_FLOOR`], erring long, at
    /// `controls`: the loop's per-period gain is at most `rho`.
    #[must_use]
    pub fn tail(&self, controls: &KarplusStrongControls) -> Tail {
        let period = period(controls.frequency, self.lowest, self.rate, 1);
        let rho = per_trip_decay(period.to_seconds(self.rate), controls.decay);
        Tail::Finite(ring_tail(rho, period, self.rate, TAIL_FLOOR))
    }
}

/// The frames a loop of gain `rho` per `period` rings to `floor`.
pub(crate) fn ring_tail(rho: Feedback, period: SampleSpan, rate: SampleRate, floor: Db) -> Samples {
    let trips = trips(rho.get(), floor);
    ring_seconds(period.to_samples_ceil().get(), trips, rate).to_samples_ceil(rate)
}

/// A [`KarplusStrong`] on every channel of a view, all at the same controls.
#[derive(Clone, Debug)]
pub struct KarplusStrongBank<F: Float> {
    strings: Vec<KarplusStrong<F>>,
}

impl<F: Float> KarplusStrongBank<F> {
    /// `width` silent strings (at least one) down to `lowest` at `rate`. Allocates.
    #[must_use]
    pub fn new(lowest: Hz, rate: SampleRate, width: usize) -> Self {
        Self {
            strings: vec![KarplusStrong::new(lowest, rate); width.max(1)],
        }
    }

    /// Takes `controls` on every channel. No allocation.
    pub fn retarget(&mut self, controls: &KarplusStrongControls) {
        for string in &mut self.strings {
            string.retarget(controls);
        }
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        for string in &mut self.strings {
            string.reset();
        }
    }

    /// The ring-out at `controls`, as [`KarplusStrong::tail`] reads it.
    #[must_use]
    pub fn tail(&self, controls: &KarplusStrongControls) -> Tail {
        self.strings[0].tail(controls)
    }

    /// Runs `input` into `output`, channel by channel: the fewer of the two views' channels
    /// and frames. No allocation.
    pub fn process(&mut self, input: Planes<'_, F>, mut output: PlanesMut<'_, '_, F>) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input
            .channels()
            .min(output.channels())
            .min(self.strings.len());
        for (c, string) in self.strings.iter_mut().enumerate().take(width) {
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            for (y, &x) in plane.samples_mut()[..frames].iter_mut().zip(x) {
                *y = string.tick(x);
            }
        }
    }
}

#[cfg(test)]
mod tests;
