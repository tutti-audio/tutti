//! [`WaveguideString`]: a string as two travelling waves between two reflecting ends.
//!
//! Two rails of `M` frames carry the waves, one from the nut to the bridge and one back. Each
//! end reflects with an inversion, a gain and the two-point average (`-g (1 + z^-1) / 2`, a
//! delay of half a frame), and the nut's reflection holds the all-pass that supplies the
//! fraction of the tuning. A round trip is `P = 2 M + 1 + d` frames, `d` the all-pass's phase
//! delay at the fundamental (the shared tuning), so the string sounds `fs / P`, and its
//! modes `j fs / P` for whole `j`.
//!
//! The input is injected at the excitation position, half into each rail, and the output is
//! the sum of the two waves at the pickup position. A position is a fraction of the string
//! from the nut, rounded to a whole frame of the rail and held in `1..M`. A mode `j` has the
//! shape `sin(j pi x)` along the string, so a pickup (or an excitation) at a node of it
//! leaves the mode out of the output (up to the end filters' loss, which leaves a standing
//! wave that is not exact).
//!
//! The end gains are equal and their product is the round trip's gain `rho = exp(-ln(1000) P
//! / (fs T60))`, below 1 for any finite decay: the signal falls 60 dB in the decay
//! time, the average's loss apart. A frequency is held between the string's lowest and `fs / 8`.

use crate::BlockMismatch;
use crate::block::expect_widths;
use core::num::NonZeroUsize;
use tutti_buffers::{ChannelLayout, Float, InputOutput};
use tutti_units::{Feedback, Hz, Normalized, SampleRate, SampleSpan, Samples, Seconds, Tail};

use crate::delay::DelayRing;
use crate::karplus_strong::ring_tail;
use crate::string_tuning::{LowestPitch, Tuned, per_trip_decay, period, tune};
use crate::{AllPassFilter, TAIL_FLOOR};

/// The controls of a [`WaveguideString`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveguideControls {
    /// The pitch. One that is not finite and above zero reads as the string's lowest.
    pub frequency: Hz,
    /// The time a round trip's gain takes to fall 60 dB, the end filters' loss apart. One
    /// that is not above zero is a string that does not ring.
    pub decay: Seconds,
}

impl WaveguideControls {
    /// A string at 220 Hz that rings for 2 s.
    pub const DEFAULT: Self = Self {
        frequency: Hz::new(220.0),
        decay: Seconds::new(2.0),
    };
}

impl Default for WaveguideControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Where along a string it is excited and where it is read, each a fraction from the nut.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveguidePositions {
    /// Where the input enters.
    pub excitation: Normalized,
    /// Where the output is read.
    pub pickup: Normalized,
}

impl WaveguidePositions {
    /// Excited a fifth of the way along, read a third of the way along.
    pub const DEFAULT: Self = Self {
        excitation: Normalized::of(0.2),
        pickup: Normalized::of(0.33),
    };
}

impl Default for WaveguidePositions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The two ends' reflections together hold a delay of one frame.
const ENDS_DELAY: f64 = 1.0;

/// One waveguide string over the float `F`.
///
/// Built on the control side for one rate and one lowest frequency, where it allocates its
/// rails; nothing after that does.
#[derive(Clone, Debug)]
pub struct WaveguideString<F: Float> {
    rate: SampleRate,
    lowest: LowestPitch,
    positions: WaveguidePositions,
    right: DelayRing<F>,
    left: DelayRing<F>,
    all_pass: AllPassFilter<F>,
    tuned: Tuned,
    /// The rail length `M`.
    rail: usize,
    /// The excitation and pickup, in frames from the nut.
    excite_at: usize,
    pick_at: usize,
    /// One end's gain, `sqrt(rho)`.
    end: F,
}

impl<F: Float> WaveguideString<F> {
    /// A silent string at [`WaveguideControls::DEFAULT`], excited and read at `positions`, whose
    /// pitch goes down to `lowest` at `rate`. Allocates.
    #[must_use]
    pub fn new(positions: WaveguidePositions, lowest: LowestPitch, rate: SampleRate) -> Self {
        let longest = period(Hz::ZERO, lowest, rate, 2);
        let rail = tune(longest, SampleSpan::of(ENDS_DELAY), 2).whole.get() / 2;
        let mut string = Self {
            rate,
            lowest,
            positions,
            right: DelayRing::new(rail + 2),
            left: DelayRing::new(rail + 2),
            all_pass: AllPassFilter::new(NonZeroUsize::MIN, Feedback::NONE),
            tuned: Tuned {
                whole: Samples::new(2),
                delay: SampleSpan::of(1.0),
                gain: Feedback::NONE,
            },
            rail: 1,
            excite_at: 1,
            pick_at: 1,
            end: F::ZERO,
        };
        string.retarget(&WaveguideControls::DEFAULT);
        string
    }

    /// The loop's split at the controls last taken: the rail length `M` and the all-pass's
    /// coefficient `g` (`H(z) = (z^-1 - g) / (1 - g z^-1)`).
    #[must_use]
    pub fn tuning(&self) -> (Samples, Feedback) {
        (Samples::new(self.rail), self.tuned.gain)
    }

    /// The excitation and the pickup, in whole frames from the nut.
    #[must_use]
    pub fn taps(&self) -> (Samples, Samples) {
        (Samples::new(self.excite_at), Samples::new(self.pick_at))
    }

    /// Takes `controls`; the waves are kept. No allocation.
    pub fn retarget(&mut self, controls: &WaveguideControls) {
        let period = period(controls.frequency, self.lowest, self.rate, 2);
        self.tuned = tune(period, SampleSpan::of(ENDS_DELAY), 2);
        self.rail = self.tuned.whole.get() / 2;
        self.all_pass.retarget(self.tuned.gain);
        let at = |x: Normalized| {
            let frames = libm::round(x.get() * self.rail as f64) as usize;
            frames.clamp(1, self.rail - 1)
        };
        self.excite_at = at(self.positions.excitation);
        self.pick_at = at(self.positions.pickup);
        let round_trip = period.to_seconds(self.rate);
        let rho = per_trip_decay(round_trip, controls.decay);
        self.end = F::from_design(libm::sqrt(rho.get()));
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.right.clear();
        self.left.clear();
        self.all_pass.reset();
    }

    /// One frame in, one out.
    #[inline]
    pub fn tick(&mut self, x: F) -> F {
        let m = self.rail;
        let half = F::lit(0.5);
        // Each rail's end arrives `M` frames after it entered; the average reads one more.
        let at_bridge = (self.right.back(m) + self.right.back(m + 1)) * half;
        let at_nut = (self.left.back(m) + self.left.back(m + 1)) * half;
        let into_left = -(self.end * at_bridge);
        let into_right = self.all_pass.tick(-(self.end * at_nut));
        self.right.push(into_right);
        self.left.push(into_left);
        // The frame just written is position 0 of its rail, at slot `1`.
        let injected = x * half;
        self.right.add_back(self.excite_at + 1, injected);
        self.left.add_back(m - self.excite_at + 1, injected);
        self.right.back(self.pick_at + 1) + self.left.back(m - self.pick_at + 1)
    }

    /// How long it rings after its input stops, to [`TAIL_FLOOR`], erring long, at
    /// `controls`: the round trip's gain is at most `rho`.
    #[must_use]
    pub fn tail(&self, controls: &WaveguideControls) -> Tail {
        let period = period(controls.frequency, self.lowest, self.rate, 2);
        let rho = per_trip_decay(period.to_seconds(self.rate), controls.decay);
        Tail::Finite(ring_tail(rho, period, self.rate, TAIL_FLOOR))
    }
}

/// A [`WaveguideString`] on every channel of a view, all at the same positions and controls.
#[derive(Clone, Debug)]
pub struct WaveguideBank<F: Float> {
    strings: Vec<WaveguideString<F>>,
}

impl<F: Float> WaveguideBank<F> {
    /// `width` silent strings. Allocates.
    #[must_use]
    pub fn new(
        positions: WaveguidePositions,
        lowest: LowestPitch,
        rate: SampleRate,
        width: ChannelLayout,
    ) -> Self {
        Self {
            strings: vec![
                WaveguideString::new(positions, lowest, rate);
                usize::from(width.count())
            ],
        }
    }

    /// Takes `controls` on every channel. No allocation.
    pub fn retarget(&mut self, controls: &WaveguideControls) {
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

    /// The ring-out at `controls`, as [`WaveguideString::tail`] reads it.
    #[must_use]
    pub fn tail(&self, controls: &WaveguideControls) -> Tail {
        self.strings[0].tail(controls)
    }

    /// Runs `input` into `output`, channel by channel. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process(&mut self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        let width = self.strings.len();
        expect_widths(block, width, width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        for (c, string) in self.strings.iter_mut().enumerate().take(width) {
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            for (y, &x) in plane.samples_mut()[..frames].iter_mut().zip(x) {
                *y = string.tick(x);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
