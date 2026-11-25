//! [`FdnKind`]: what differs between a plain feedback delay network and a shimmer.
//!
//! A plain network ([`FdnLayout`]) can freeze ([`FreezeControls`]) and so rings without end;
//! a shimmer ([`ShimmerLayout`]) shifts the pitch of its feedback ([`ShimmerControls`]) and
//! always decays. [`FdnBank`](crate::FdnBank) is generic over the kind, so a control one kind
//! does not read is not a field of the other's controls.

use std::fmt::Debug;

use tutti_units::{Cents, Feedback, SampleRate, Samples};

use super::design::{FdnControls, FdnLayout};
use super::shimmer::Shimmer;
use crate::reverb::network::Network;
use crate::{DecayTime, FftFloat};

/// The controls only a plain network has: it holds its tail.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FreezeControls {
    /// Holds the tail: every gain exactly one and the input gated off, so the network is
    /// lossless and its tail never ends.
    pub freeze: bool,
}

impl FreezeControls {
    /// Not frozen.
    pub const DEFAULT: FreezeControls = FreezeControls { freeze: false };
}

impl Default for FreezeControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The controls only a shimmer has: the shift of its feedback and the share it takes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShimmerControls {
    /// How far the shimmer shifts the feedback.
    pub pitch: Cents,
    /// The share of the feedback the shimmer shifts: `0` is the plain network, and a send of
    /// [`Feedback::MAX_STABLE`] or more is held at it, so the loop's gain stays below 1.
    pub send: Feedback,
}

impl ShimmerControls {
    /// An octave up (1200 cents), none of the feedback sent.
    pub const DEFAULT: ShimmerControls = ShimmerControls {
        pitch: Cents::new(1200.0),
        send: Feedback::NONE,
    };
}

impl Default for ShimmerControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What a feedback delay network is built to when it shifts the pitch of its feedback: a
/// **shimmer**, with the lines and longest decay of its [`FdnLayout`]. Its tail always
/// decays, so it has no freeze, and the decay queries read the shifter's delay as part of
/// every line's loop (see [`ShimmerControls::send`]).
///
/// The shifter is one mono [`PhaseVocoder`](crate::PhaseVocoder), run once a chunk on the
/// lines' summed feedback. Its latency and one chunk are the **loop delay** of the shifted
/// path ([`FdnBank::shimmer_delay`](crate::FdnBank::shimmer_delay)): the reverb's onset,
/// not a processing latency.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShimmerLayout {
    layout: FdnLayout,
}

impl ShimmerLayout {
    /// A shimmer of `layout`'s lines and longest decay.
    #[must_use]
    pub const fn new(layout: FdnLayout) -> Self {
        Self { layout }
    }

    /// The lines and longest decay.
    #[must_use]
    pub const fn layout(&self) -> &FdnLayout {
        &self.layout
    }
}

/// A kind of feedback delay network, and the controls it adds to [`FdnControls`]: an
/// [`FdnLayout`] (plain) takes [`FreezeControls`], a [`ShimmerLayout`] takes
/// [`ShimmerControls`]. [`FdnBank`](crate::FdnBank) and [`FdnRamps`](crate::FdnRamps) take a
/// kind's controls beside the network's. Sealed.
pub trait FdnKind: sealed::Sealed {}

impl FdnKind for FdnLayout {}
impl FdnKind for ShimmerLayout {}

pub(super) mod sealed {
    use super::*;
    use crate::reverb::fdn::shimmer::GAIN_BOUND;
    use crate::{Lerp, Ramp, Steps};
    use tutti_buffers::Float;

    /// What a [`FdnBank`](crate::FdnBank) asks of its kind.
    pub trait Sealed: Copy + Debug + PartialEq + Send + 'static {
        /// What the kind adds to [`FdnControls`].
        type Controls: Copy + PartialEq + Debug + Send;
        /// What the kind keeps in the feedback path.
        type State<F: FftFloat>: Shifter<F> + Send;
        /// What the kind's ramps hold beyond the network's.
        type Ramps<F: Float + Lerp>: ShiftRamps<F, Self::Controls> + Send;

        /// The lines and longest decay.
        fn layout(&self) -> &FdnLayout;
        /// The state at `rate`. Allocates.
        fn state<F: FftFloat>(&self, rate: SampleRate) -> Self::State<F>;
        /// Designs `c` and the kind's `k` into `out`, in place.
        fn design_into(
            &self,
            c: &FdnControls,
            k: &Self::Controls,
            rate: SampleRate,
            out: &mut Network,
        );
        /// The ceiling over every control, from the network's own `tail` at the longest decay.
        fn ceiling(&self, tail: DecayTime, rate: SampleRate) -> DecayTime;
    }

    /// The ramps of what a kind adds to the feedback: nothing for a plain network.
    pub trait ShiftRamps<F: Float + Lerp, C>: Clone + Debug {
        /// At rest, nothing shifted.
        fn new() -> Self;
        /// Aims at the kind's controls `k`, over `ramp`.
        fn retarget(&mut self, k: &C, ramp: Steps);
        /// Takes one step.
        fn step(&mut self);
        /// Jumps to the target now.
        fn jump_to_target(&mut self);
        /// The share of the feedback shifted now.
        fn now(&self) -> F;
        /// The shift now.
        fn pitch(&self) -> Cents;
        /// Whether the shifted path is in the loop for `k`: its send is above zero, or its
        /// ramp has not reached zero.
        fn shifts(&self, k: &C) -> bool;
    }

    /// The shifter in the feedback path of a kind: none for a plain network.
    pub trait Shifter<F: FftFloat>: Clone + Debug {
        /// The frames from a sample of the feedback to its shifted copy re-entering the lines.
        fn delay(&self) -> Samples;
        /// Back to silence.
        fn reset(&mut self);
        /// Sets the chunk's constants: the share `send` of the feedback shifted by `pitch`
        /// (from the next hop on), over `lines` lines.
        fn begin_chunk(&mut self, send: F, pitch: Cents, lines: usize);
        /// Writes into `d[j]` the sample line `j` takes at frame `into` of the chunk, from
        /// the feedback `d` after its absorption: the line's share `c_in[j] * xd` of the input
        /// plus the reflection `d[j] - 2 v[j] sum`, and for a shimmer the shifted share of
        /// the mean of the reflected feedback.
        fn feed(&mut self, into: usize, d: &mut [F], v: &[F], c_in: &[F], xd: F, sum: F);
        /// Hands the finished chunk to the shifter.
        fn end_chunk(&mut self);
    }

    impl<F: Float + Lerp, C> ShiftRamps<F, C> for () {
        fn new() -> Self {}
        fn retarget(&mut self, _: &C, _: Steps) {}
        fn step(&mut self) {}
        fn jump_to_target(&mut self) {}
        fn now(&self) -> F {
            F::ZERO
        }
        fn pitch(&self) -> Cents {
            Cents::new(0.0)
        }
        fn shifts(&self, _: &C) -> bool {
            false
        }
    }

    /// The shimmer's send and shift.
    #[derive(Clone, Debug)]
    pub struct ShimmerRamps<F: Float> {
        /// The share of the feedback the shimmer shifts, `k`.
        send: Ramp<F>,
        /// The shimmer's shift, which takes the vocoder's own hop to land.
        pitch: Cents,
    }

    impl<F: Float + Lerp> ShiftRamps<F, ShimmerControls> for ShimmerRamps<F> {
        fn new() -> Self {
            Self {
                send: Ramp::new(F::ZERO),
                pitch: Cents::new(0.0),
            }
        }
        fn retarget(&mut self, k: &ShimmerControls, ramp: Steps) {
            // A send of 0.99 at most (`Feedback::MAX_STABLE`), whatever the control wrote.
            let send = k.send.get();
            let target = if send > 0.0 {
                F::from_design(send.min(Feedback::MAX_STABLE.get()))
            } else {
                F::ZERO
            };
            self.send.retarget_by(target, ramp);
            self.pitch = k.pitch;
        }
        fn step(&mut self) {
            self.send.advance(1);
        }
        fn jump_to_target(&mut self) {
            self.send.jump_to_target();
        }
        fn now(&self) -> F {
            self.send.now()
        }
        fn pitch(&self) -> Cents {
            self.pitch
        }
        fn shifts(&self, k: &ShimmerControls) -> bool {
            k.send > Feedback::NONE || self.send.now() > F::ZERO
        }
    }

    impl<F: FftFloat> Shifter<F> for () {
        fn delay(&self) -> Samples {
            Samples::ZERO
        }
        fn reset(&mut self) {}
        fn begin_chunk(&mut self, _: F, _: Cents, _: usize) {}
        fn feed(&mut self, _: usize, d: &mut [F], v: &[F], c_in: &[F], xd: F, sum: F) {
            let two = F::ONE + F::ONE;
            for j in 0..d.len() {
                d[j] = c_in[j] * xd + d[j] - two * v[j] * sum;
            }
        }
        fn end_chunk(&mut self) {}
    }

    impl<F: FftFloat> Shifter<F> for Shimmer<F> {
        fn delay(&self) -> Samples {
            Shimmer::delay(self)
        }
        fn reset(&mut self) {
            Shimmer::reset(self);
        }
        fn begin_chunk(&mut self, send: F, pitch: Cents, lines: usize) {
            self.set_chunk(send, F::from_design(1.0 / (lines as f64).sqrt()));
            self.retarget(pitch);
        }
        fn feed(&mut self, into: usize, d: &mut [F], v: &[F], c_in: &[F], xd: F, sum: F) {
            let two = F::ONE + F::ONE;
            let mut mean = F::ZERO;
            for j in 0..d.len() {
                d[j] -= two * v[j] * sum;
                mean += d[j];
            }
            let (k, root) = self.chunk();
            let mean = mean * root;
            self.push(into, mean);
            // A send of exactly 0 leaves the lines untouched, a non-finite shifted sample
            // included. The shifted feedback re-enters at `1 / GAIN_BOUND` of its level.
            let share = if k > F::ZERO {
                k * (self.back(into) * F::from_design(1.0 / GAIN_BOUND) - mean) * root
            } else {
                F::ZERO
            };
            for j in 0..d.len() {
                d[j] = c_in[j] * xd + d[j] + share;
            }
        }
        fn end_chunk(&mut self) {
            Shimmer::end_chunk(self);
        }
    }

    impl Sealed for FdnLayout {
        type Controls = FreezeControls;
        type State<F: FftFloat> = ();
        type Ramps<F: Float + Lerp> = ();

        fn layout(&self) -> &FdnLayout {
            self
        }
        fn state<F: FftFloat>(&self, _: SampleRate) {}
        fn design_into(
            &self,
            c: &FdnControls,
            k: &FreezeControls,
            rate: SampleRate,
            out: &mut Network,
        ) {
            FdnLayout::design_into(self, c, k, rate, out);
        }
        /// Unbounded: the freeze holds the tail.
        fn ceiling(&self, _: DecayTime, _: SampleRate) -> DecayTime {
            DecayTime::Unbounded
        }
    }

    impl Sealed for ShimmerLayout {
        type Controls = ShimmerControls;
        type State<F: FftFloat> = Shimmer<F>;
        type Ramps<F: Float + Lerp> = ShimmerRamps<F>;

        fn layout(&self) -> &FdnLayout {
            &self.layout
        }
        fn state<F: FftFloat>(&self, rate: SampleRate) -> Shimmer<F> {
            Shimmer::new(rate)
        }
        fn design_into(
            &self,
            c: &FdnControls,
            _: &ShimmerControls,
            rate: SampleRate,
            out: &mut Network,
        ) {
            ShimmerLayout::design_into(self, c, rate, out);
        }
        /// Finite: the network's own at the longest decay, with the rounding of the
        /// reflection vector.
        fn ceiling(&self, tail: DecayTime, rate: SampleRate) -> DecayTime {
            match tail {
                DecayTime::Finite(t) => DecayTime::Finite(tutti_units::Seconds::new(
                    t.get() * self.layout.rounding_margin(rate),
                )),
                other => other,
            }
        }
    }
}

impl ShimmerLayout {
    /// Designs `c` into `out` in place, at `rate`, as [`FdnLayout::design_into`] does for a
    /// plain network: the shimmer's own controls are not part of the design.
    pub fn design_into(&self, c: &FdnControls, rate: SampleRate, out: &mut Network) {
        self.layout.design(c, false, rate, out);
    }
}
