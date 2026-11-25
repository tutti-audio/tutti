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

/// A kind of feedback delay network: [`FdnLayout`] (plain) or [`ShimmerLayout`]. Sealed.
pub trait FdnKind: sealed::Sealed {}

impl FdnKind for FdnLayout {}
impl FdnKind for ShimmerLayout {}

pub(super) mod sealed {
    use super::*;

    /// What a [`FdnBank`](crate::FdnBank) asks of its kind.
    pub trait Sealed: Copy + Debug + PartialEq {
        /// What the kind adds to [`FdnControls`].
        type Controls: Copy + PartialEq + Debug;
        /// What the kind keeps in the feedback path.
        type State<F: FftFloat>: Shifter<F>;

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
        /// The pitch and share of the feedback `k` shifts.
        fn shift(k: &Self::Controls) -> (Cents, Feedback);
    }

    /// The shifter in the feedback path of a kind: none for a plain network.
    pub trait Shifter<F: FftFloat>: Clone + Debug {
        /// The frames from a sample of the feedback to its shifted copy re-entering the lines.
        fn delay(&self) -> Samples;
        /// Back to silence.
        fn reset(&mut self);
        /// Shifts by `pitch` from the next hop on.
        fn retarget(&mut self, pitch: Cents);
        /// The shifted share added to each line at frame `into` of the chunk: `k` of the
        /// mean of the feedback `d` (`root` the weight of a line in it), the shifted mean
        /// scaled by `bound`.
        fn share(&mut self, into: usize, d: &[F], k: F, root: F, bound: F) -> F;
        /// Hands the finished chunk to the shifter.
        fn end_chunk(&mut self);
    }

    impl<F: FftFloat> Shifter<F> for () {
        fn delay(&self) -> Samples {
            Samples::ZERO
        }
        fn reset(&mut self) {}
        fn retarget(&mut self, _: Cents) {}
        fn share(&mut self, _: usize, _: &[F], _: F, _: F, _: F) -> F {
            F::ZERO
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
        fn retarget(&mut self, pitch: Cents) {
            Shimmer::retarget(self, pitch);
        }
        fn share(&mut self, into: usize, d: &[F], k: F, root: F, bound: F) -> F {
            let mean = d.iter().copied().sum::<F>() * root;
            self.push(into, mean);
            // A send of exactly 0 leaves the lines untouched, a non-finite shifted sample
            // included.
            if k > F::ZERO {
                k * (self.back(into) * bound - mean) * root
            } else {
                F::ZERO
            }
        }
        fn end_chunk(&mut self) {
            Shimmer::end_chunk(self);
        }
    }

    impl Sealed for FdnLayout {
        type Controls = FreezeControls;
        type State<F: FftFloat> = ();

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
        fn shift(_: &FreezeControls) -> (Cents, Feedback) {
            (Cents::new(0.0), Feedback::NONE)
        }
    }

    impl Sealed for ShimmerLayout {
        type Controls = ShimmerControls;
        type State<F: FftFloat> = Shimmer<F>;

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
        fn shift(k: &ShimmerControls) -> (Cents, Feedback) {
            (k.pitch, k.send)
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
