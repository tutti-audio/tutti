use super::*;

// ── Dimensionless amounts ───────────────────────────────────────────────────
//
// Six types (`Amplitude` is in `level`), because the roles differ in both
// range and algebra, which is this module's own test for whether two quantities are the same type:
//
//   Amplitude  [0, inf)   scalable      a gain multiplier
//   Mix        [0, 1]     NOT scalable  a blend position between two signals
//   Pan        [-1, 1]    NOT scalable  a position on the left/right axis
//   Feedback   [0, 0.99]  NOT scalable  a recirculation coefficient
//   Bipolar    [-1, 1]    signed, range-checked: a modulation depth, a bend, a macro
//   Drive      [0, inf)   NOT scalable  shaper-curve intensity
//   BitDepth   [1, 32]    NOT scalable  a quantizer's word length, fractional
//
// `Mix` declines scaling because half of a blend position is not a blend
// position; `Feedback` declines it because scaling a coefficient walks it
// through the stability bound; `Drive` declines it because it is never
// multiplied onto a signal at all.

unit! {
    /// A blend position between two signals, `0..1`: `0` is all of the first
    /// signal (the input, as it was before the node: the "dry" end), `1` all of
    /// the second (the node's output: the "wet" end). In between is
    /// `first * (1 - mix) + second * mix`, a linear interpolation.
    ///
    /// What the graph holds per node is this blend of the node's output with
    /// its own input delayed by the node's declared latency
    /// (`Editor::set_mix` of `tutti-graph`, doc 013).
    ///
    /// A *position on a scale*, not a magnitude — which is why it does not
    /// scale: half of a 50% blend is not a meaningful quantity, and
    /// `mix * 0.5` reads like it dims the wet signal when it actually moves
    /// the crossfade point.
    ///
    /// It is also a position on a `0..1` scale among a node's own settings,
    /// not only between dry and wet (a reverb's diffusion or size, which each
    /// algorithm maps onto its scale).
    pub struct Mix(f64): Unit, Ordered, Bounded;
}
// NOT `Scalable` / `Additive`: see the type doc. Two blends
// summing to 1.4 is off the end of the crossfade.

impl Mix {
    /// All of the first signal and none of the second: the input as it was
    /// before the node (a graph node at `DRY` writes its aligned input, what a
    /// bypass writes) and none of the node's output. The studio's "dry".
    pub const DRY: Mix = Mix(0.0);
    /// All of the second signal and none of the first: the node's output and
    /// none of the input (a graph node at `WET` is the node as it is, with
    /// nothing blended). The studio's "wet".
    pub const WET: Mix = Mix(1.0);

    /// Constrain into `0..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f64) -> Mix {
        Mix(v).clamp(Self::DRY, Self::WET)
    }
}

unit! {
    /// Position on the left/right axis, `-1..1`. `-1` is hard left, `0`
    /// center, `+1` hard right.
    ///
    /// A *position*, like [`Azimuth`] — but on a segment rather than a circle,
    /// so it saturates at the ends instead of wrapping, and unlike `Azimuth`
    /// it orders (left really is less than right).
    ///
    /// A control a caller sets. The engine's measured left/right balance is
    /// [`StereoBalance`], a reading; there is no conversion between them. A
    /// balance control on a stereo pair is [`Balance`].
    /// Distinct from [`Bipolar`]: a pan position and a modulation amount are
    /// different quantities that happen to share a range.
    pub struct Pan(f64): Unit, Ordered, Bounded, Signed;
}
// NOT `Scalable`: half of a pan position is not a pan position — the
// same argument `Mix` makes. Moving a source is `Azimuth::lerp_shortest`'s
// job on the circle, and interpolation here is a `lerp`, not a `*`.
// NOT `Additive`: two pan positions do not sum into a third.

impl Pan {
    /// Hard left.
    pub const LEFT: Pan = Pan(-1.0);
    /// Centered — equal in both channels.
    pub const CENTER: Pan = Pan(0.0);
    /// Hard right.
    pub const RIGHT: Pan = Pan(1.0);

    /// Constrain into `-1..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f64) -> Pan {
        Pan(v).clamp(Self::LEFT, Self::RIGHT)
    }
}

unit! {
    /// A stereo balance, `-1..1`: `-1` keeps only the left channel, `0` both at
    /// unity, `+1` only the right.
    ///
    /// A control on a signal that is already a left/right pair. It attenuates
    /// the side it moves away from and never boosts the other, so its centre is
    /// exactly unity, where a panner's equal-power law reads -3 dB on each
    /// side, and it holds no power constant. A [`Pan`] places a source with a
    /// panner's law, and [`StereoBalance`] is the engine's measured balance, a
    /// reading; there is no conversion between the three.
    pub struct Balance(f64): Unit, Ordered, Bounded, Signed;
}
// NOT `Scalable` / `Additive`: a position on a segment, as `Pan` is; half a
// balance is not a balance, and two do not sum.

impl Balance {
    /// Only the left channel.
    pub const LEFT: Balance = Balance(-1.0);
    /// Both channels at unity.
    pub const CENTER: Balance = Balance(0.0);
    /// Only the right channel.
    pub const RIGHT: Balance = Balance(1.0);

    /// Constrain into `-1..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f64) -> Balance {
        Balance(v).clamp(Self::LEFT, Self::RIGHT)
    }

    /// The `(left, right)` amplitudes of the linear balance law:
    /// `left = min(1, 1 - b)`, `right = min(1, 1 + b)`. `(1, 1)` at centre,
    /// `(1, 0)` at [`LEFT`](Self::LEFT), `(0, 1)` at [`RIGHT`](Self::RIGHT).
    /// A balance outside `-1..=1` reads as the nearer end, and a NaN as unity
    /// on both sides. Neither side exceeds unity.
    #[inline]
    #[must_use]
    pub fn gains(self) -> (Amplitude, Amplitude) {
        let b = self.0.clamp(-1.0, 1.0);
        (
            Amplitude::new((1.0 - b).min(1.0)),
            Amplitude::new((1.0 + b).min(1.0)),
        )
    }
}

unit! {
    /// A feedback (recirculation) coefficient.
    ///
    /// Must stay below 1.0 or the loop it feeds grows without bound — a
    /// delay line at unity feedback never decays. [`MAX_STABLE`](Self::MAX_STABLE)
    /// is the one ceiling every consumer clamps against.
    pub struct Feedback(f64): Unit, Ordered, Bounded;
}
// NOT `Scalable`: scaling a coefficient walks it across the stability
// bound with no check. `new_clamped` is the only way in.
// NOT `Additive`: two feedback paths summing past 1.0 is exactly the
// runaway this type exists to prevent — see the note on cross-feedback below.

impl Feedback {
    /// No recirculation.
    pub const NONE: Feedback = Feedback(0.0);

    /// The largest coefficient that still decays.
    ///
    /// 0.99. Audio-rate modulation paths take a coefficient straight off an
    /// input port without passing a constructor, so this is the bound they
    /// clamp against by hand.
    pub const MAX_STABLE: Feedback = Feedback(0.99);

    /// Constrain into the stable range.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f64) -> Feedback {
        Feedback(v).clamp(Self::NONE, Self::MAX_STABLE)
    }

    /// Constrain a *pair* of coefficients that feed the same loop.
    ///
    /// Cross-coupled delays add their direct and cross terms into one
    /// recirculation (`in_l + fb_l·fb + fb_r·cross`), so clamping each to
    /// [`Feedback::MAX_STABLE`] independently still permits a combined 1.98 and a
    /// runaway. This scales the pair down together when their sum would
    /// exceed the bound, preserving their ratio, so the pair sums to
    /// [`MAX_STABLE`](Self::MAX_STABLE) at most, to within `f64` rounding.
    ///
    /// Negative coefficients are raised to zero. A NaN stays NaN in its own
    /// coefficient and the other is clamped on its own, as [`new_clamped`]
    /// does. An infinite coefficient takes the whole bound (two infinite ones
    /// share it) and every finite one goes to zero.
    ///
    /// [`new_clamped`]: Self::new_clamped
    #[inline]
    #[must_use]
    pub fn stable_pair(direct: f64, cross: f64) -> (Feedback, Feedback) {
        if direct.is_nan() || cross.is_nan() {
            return (Self::new_clamped(direct), Self::new_clamped(cross));
        }
        let d = direct.max(0.0);
        let c = cross.max(0.0);
        let total = d + c;
        if total <= Self::MAX_STABLE.0 {
            return (Feedback(d), Feedback(c));
        }
        if total.is_finite() {
            let scale = Self::MAX_STABLE.0 / total;
            return (Feedback(d * scale), Feedback(c * scale));
        }
        // The sum overflowed, or an input is infinite.
        if d.is_infinite() || c.is_infinite() {
            let (wd, wc) = (f64::from(d.is_infinite()), f64::from(c.is_infinite()));
            let share = Self::MAX_STABLE.0 / (wd + wc);
            return (Feedback(wd * share), Feedback(wc * share));
        }
        // Halving is exact, and keeps the sum finite.
        let (d, c) = (d * 0.5, c * 0.5);
        let scale = Self::MAX_STABLE.0 / (d + c);
        (Feedback(d * scale), Feedback(c * scale))
    }
}

// `Bipolar` is a `Unit` (below), which is already a `Quantity`, so it takes the
// type and its error without the macro's own `Quantity` impl.
crate::ranged! {
    @items []
    /// A signed amount, `-1..=1`: the depth of a modulation route, a pitch
    /// bend, or the value of a control with no target of its own, such as a
    /// macro's position.
    ///
    /// Signed on purpose: a negative amount inverts, so an LFO at a depth of
    /// [`MIN`](Self::MIN) is the same shape phase-flipped. That is the whole
    /// reason this is not just an [`Amplitude`], and the reason it has a sign
    /// where `Mix` and `Feedback` do not. It negates (`-x` is the same amount
    /// the other way) and orders.
    ///
    /// **Range-checked**: [`new`](Self::new) refuses NaN, the infinities and
    /// anything outside `-1..=1`, and so do `TryFrom<f64>` and serde.
    /// [`of`](Self::of) is for a literal. [`clamped`](Self::clamped) is the one
    /// saturating constructor; [`Unit::from_raw`] is it too, since a parameter
    /// cell holds any float.
    ///
    /// ```
    /// use tutti_units::Bipolar;
    ///
    /// assert_eq!(Bipolar::new(0.25).map(Bipolar::get), Some(0.25));
    /// assert_eq!(Bipolar::new(1.5), None);
    /// assert_eq!(-Bipolar::MAX, Bipolar::MIN);
    /// assert_eq!(Bipolar::clamped(1.5), Bipolar::MAX);
    /// ```
    #[repr(transparent)]
    #[derive(Default)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "f64", into = "f64"))]
    Bipolar(f64), BipolarOutOfRange, "within -1..=1",
    |raw| raw >= -1.0 && raw <= 1.0
}

// NOT `Additive` and NOT `Scalable`: a sum or a scaled amount can leave
// `-1..=1`. Do the arithmetic on the raw number and call `new` (or
// `clamped`, where saturating is meant).
impl Bipolar {
    /// The negative end.
    pub const MIN: Bipolar = Bipolar(-1.0);
    /// The middle.
    pub const ZERO: Bipolar = Bipolar(0.0);
    /// The positive end.
    pub const MAX: Bipolar = Bipolar(1.0);

    /// `raw` constrained into `-1..=1`; NaN is [`ZERO`](Self::ZERO).
    #[inline]
    #[must_use]
    pub const fn clamped(raw: f64) -> Bipolar {
        if raw.is_nan() {
            Bipolar::ZERO
        } else if raw < -1.0 {
            Bipolar::MIN
        } else if raw > 1.0 {
            Bipolar::MAX
        } else {
            Bipolar(raw)
        }
    }

    /// Returns the smaller of the two, in this unit.
    #[inline]
    #[must_use]
    pub fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    /// Returns the larger of the two, in this unit.
    #[inline]
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }

    /// Clamps into `lo..=hi`, in this unit.
    ///
    /// # Panics
    ///
    /// Panics if `lo > hi`, matching `f64::clamp`.
    #[inline]
    #[must_use]
    pub fn clamp(self, lo: Self, hi: Self) -> Self {
        Self(self.0.clamp(lo.0, hi.0))
    }
}

impl PartialOrd for Bipolar {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl core::ops::Neg for Bipolar {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

impl core::fmt::Display for Bipolar {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// `from_raw` saturates ([`Bipolar::clamped`]): a parameter cell holds any
/// float, and a unit's `from_raw` cannot refuse.
impl Unit for Bipolar {
    type Raw = f64;
    #[inline]
    fn from_raw(v: f64) -> Self {
        Self::clamped(v)
    }
    #[inline]
    fn to_raw(self) -> f64 {
        self.0
    }
}

unit! {
    /// Waveshaper drive — how hard a signal is pushed into a nonlinearity.
    ///
    /// Distinct from [`Amplitude`] despite sharing its `[0, inf)` range,
    /// because it is **not a multiplier on the signal**: it selects or
    /// parameterizes a shaping curve. Typing it as a gain would license
    /// `sample * drive`, which is meaningless for a curve selector.
    pub struct Drive(f64): Unit, Ordered, Bounded;
}
// NOT `Scalable`: "twice the drive" is not twice anything — the curve's
// response is nonlinear by construction.
// NOT `Additive`: two drives do not sum.

impl Drive {
    /// No overdrive — the shaper passes the signal through.
    pub const UNITY: Drive = Drive(1.0);
}

unit! {
    /// Spatial diffusion, `0..1`: how widely a point source is smeared across
    /// a speaker array. 0 is a point, 1 is fully diffuse.
    ///
    /// Not a [`Mix`] — it blends no pair of signals; it is a geometric
    /// property of the panning solution, and the VBAP panner consumes it as
    /// such. Sharing `Mix`'s range is not sharing its meaning.
    pub struct Spread(f64): Unit, Ordered, Bounded;
}

impl Spread {
    /// A point source — no diffusion.
    pub const POINT: Spread = Spread(0.0);
    /// Fully diffuse across the array.
    pub const DIFFUSE: Spread = Spread(1.0);

    /// Constrain into `0..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f64) -> Spread {
        Spread(v).clamp(Self::POINT, Self::DIFFUSE)
    }
}

unit! {
    /// Mid/side stereo width. `0` collapses to mono, `1` leaves the image
    /// unchanged, and above `1` widens it past the source.
    ///
    /// Not an [`Amplitude`] despite the matching range: it scales the *side*
    /// component against the mid, so it reshapes the stereo image rather than
    /// making the signal louder. Typing it as a gain would invite it into
    /// signal multiplications where it does not belong.
    pub struct StereoWidth(f64): Unit, Ordered, Bounded;
}

impl StereoWidth {
    /// Collapsed to mono.
    pub const MONO: StereoWidth = StereoWidth(0.0);
    /// The source image, unchanged.
    pub const NATURAL: StereoWidth = StereoWidth(1.0);

    /// Constrain to non-negative. Deliberately no upper bound — widening past
    /// the source is a legitimate effect. NaN stays NaN.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f64) -> StereoWidth {
        StereoWidth(if v < 0.0 { 0.0 } else { v })
    }
}

unit! {
    /// A word length in bits, `1..=32`: the resolution a quantizer keeps. A depth of
    /// `b` bits has the step `1 / 2^(b-1)` over a signal of peak 1 (`2^b + 1` levels
    /// across `-1..=1`).
    ///
    /// Fractional depths are values: the step is `2^-(b-1)` for any real `b`, so
    /// `3.5` bits is a step between those of 3 and 4 bits, and a depth moves
    /// smoothly. A value outside the range, or not finite, is not a depth:
    /// [`new_clamped`](Self::new_clamped) is the way in from a raw number.
    ///
    /// Not an [`Amplitude`] or a [`Drive`]: it is a resolution, a count of bits and
    /// not a factor on a signal.
    pub struct BitDepth(f64): Unit, Ordered, Bounded;
}
// NOT `Additive`: two word lengths do not sum into a third; the bits a quantizer
// keeps are chosen, not accumulated.
// NOT `Scalable`: twice the bits is not twice anything audible (each bit is
// about 6 dB of signal-to-noise ratio, so the scale is logarithmic).

impl BitDepth {
    /// One bit: three levels, `-1`, `0` and `1`.
    pub const MIN: BitDepth = BitDepth(1.0);
    /// Thirty-two bits: finer than an `f32` signal's own resolution.
    pub const MAX: BitDepth = BitDepth(32.0);

    /// Constrain into `1..=32`. NaN stays NaN.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f64) -> BitDepth {
        BitDepth(v).clamp(Self::MIN, Self::MAX)
    }
}

#[cfg(test)]
mod tests;
