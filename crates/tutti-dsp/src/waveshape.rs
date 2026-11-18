//! Memoryless waveshaping curves ([`ShapeKind`]): `Tanh`, `Atan`, `Softsign`,
//! `HardClip`, `Crush` and `SoftCrush`, each one line of arithmetic, and the
//! [`WaveshaperBank`] that runs one over a block.
//!
//! A bank's drive is a [`Glide`] over [`GAIN_GLIDE`], or a lane read per frame where a port drives
//! it; a frame whose lane value is not finite reads the drive in force, the glide's value at that
//! frame. Frame `i` reads the glide at step `taken + 1 + i`, then the block's steps are taken.
//!
//! [`Antialiasing`] replaces the curve at each frame by its mean over the segment from the
//! previous input to this one, which removes most of what the curve folds back past the Nyquist
//! frequency. The bank then keeps each channel's last two inputs.

use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_types::MAX_CHANNELS;
use tutti_units::{Drive, SampleRate, Samples};

use crate::{GAIN_GLIDE, Glide, Lerp, OversamplingFactor, Steps};

/// The frames a pass over a block takes at a time: the hardness of a stretch is held in a
/// stack array.
const CHUNK: usize = 64;

/// Selects which waveshaping curve a [`WaveshaperBank`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq, Default)]
#[non_exhaustive]
pub enum ShapeKind {
    /// `tanh` saturation — smooth, tube-like.
    #[default]
    Tanh,
    /// `atan` saturation — slightly harder knee than tanh.
    Atan,
    /// `softsign` (`x / (1 + |x|)`) — gentle.
    Softsign,
    /// Hard clip to ±1.
    HardClip,
    /// Bitcrush-style staircase quantization.
    Crush,
    /// Smoothed staircase quantization.
    SoftCrush,
}

/// Runs `$body` with `$k` bound to `$kind` as a **constant**, one arm per
/// kind, so each arm's loop is compiled for its own curve: the `match` in
/// [`ShapeKind::curve`] folds away instead of running per sample.
macro_rules! with_kind {
    ($kind:expr, |$k:ident| $body:expr) => {
        match $kind {
            ShapeKind::Tanh => {
                const $k: ShapeKind = ShapeKind::Tanh;
                $body
            }
            ShapeKind::Atan => {
                const $k: ShapeKind = ShapeKind::Atan;
                $body
            }
            ShapeKind::Softsign => {
                const $k: ShapeKind = ShapeKind::Softsign;
                $body
            }
            ShapeKind::HardClip => {
                const $k: ShapeKind = ShapeKind::HardClip;
                $body
            }
            ShapeKind::Crush => {
                const $k: ShapeKind = ShapeKind::Crush;
                $body
            }
            ShapeKind::SoftCrush => {
                const $k: ShapeKind = ShapeKind::SoftCrush;
                $body
            }
        }
    };
}

mod adaa;

impl ShapeKind {
    /// Shapes one sample: `x` through this curve at `drive`, in the float `F`.
    ///
    /// `drive` is input gain into the curve, floored at 0. For the two
    /// staircases it is instead the number of **levels per unit**, floored at 1
    /// (fewer than one level per unit is not a staircase).
    ///
    /// Memoryless and total: every curve maps every finite `x` to a finite
    /// output, and none keeps state between calls.
    #[inline]
    #[must_use]
    pub fn apply<F: Float>(self, x: F, drive: Drive) -> F {
        self.curve(self.hardness(F::from_f32(drive.get())), x)
    }

    /// The hardness the curve runs at for `drive`, floored the way the kind needs.
    #[inline(always)]
    fn hardness<F: Float>(self, drive: F) -> F {
        let drive = drive.max(F::ZERO);
        match self {
            Self::Crush | Self::SoftCrush => drive.max(F::ONE),
            _ => drive,
        }
    }

    /// The curve at a hardness from [`hardness`](Self::hardness).
    ///
    /// The operation order within each arm is fixed: the pinned values in the
    /// tests depend on it bit for bit.
    #[inline(always)]
    fn curve<F: Float>(self, hardness: F, x: F) -> F {
        let (half, one) = (F::from_f32(0.5), F::ONE);
        match self {
            Self::Tanh => (x * hardness).tanh(),
            // Rescaled to saturate at ±1 while keeping unit slope at the origin.
            Self::Atan => {
                // `atan2(y, 1)` is `atan(y)`.
                (x * (hardness * F::PI * half)).atan2(one) * (F::from_f32(2.0) / F::PI)
            }
            Self::Softsign => {
                let y = x * hardness;
                y / (one + y.abs())
            }
            Self::HardClip => (x * hardness).clamp(-one, one),
            Self::Crush => round_half_away(x * hardness) / hardness,
            Self::SoftCrush => {
                let y = x * hardness;
                let step = y.floor();
                (step + smooth9(y - step)) / hardness
            }
        }
    }

    /// `input` through the curve at one `hardness`, into `output`. The loop is compiled
    /// for the kind, so the curve inlines.
    #[inline(always)]
    fn shape_held<F: Float, S: Float>(self, hardness: F, input: &[S], output: &mut [S]) {
        with_kind!(self, |K| {
            for (o, x) in output.iter_mut().zip(input) {
                *o = S::from_f64(K.curve(hardness, F::from_f64(x.to_f64())).to_f64());
            }
        })
    }

    /// `input` through the curve at a hardness per frame, into `output`.
    #[inline(always)]
    fn shape_each<F: Float, S: Float>(self, hardness: &[F], input: &[S], output: &mut [S]) {
        with_kind!(self, |K| {
            for ((o, x), &h) in output.iter_mut().zip(input).zip(hardness) {
                *o = S::from_f64(K.curve(h, F::from_f64(x.to_f64())).to_f64());
            }
        })
    }
}

/// `x` rounded to the nearest whole number, halves away from zero, in the float `F`. NaN
/// stays NaN and the sign of zero is kept.
#[inline(always)]
fn round_half_away<F: Float>(x: F) -> F {
    let magnitude = x.abs();
    let whole = magnitude.floor();
    let up = if magnitude - whole >= F::from_f32(0.5) {
        whole + F::ONE
    } else {
        whole
    };
    up * x.signum()
}

/// The ninth-order smoothstep: `0 → 0`, `1 → 1`, with the first four
/// derivatives zero at both ends, so each [`ShapeKind::SoftCrush`] step is a
/// smooth S rather than a jump. The coefficients are exact in both widths.
#[inline]
fn smooth9<F: Float>(x: F) -> F {
    let c = F::from_f32;
    let x2 = x * x;
    ((((c(70.0) * x - c(315.0)) * x + c(540.0)) * x - c(420.0)) * x + c(126.0)) * x2 * x2 * x
}

/// The order of antiderivative antialiasing (ADAA) a [`WaveshaperBank`] shapes with.
///
/// With `F1` the antiderivative of the curve `g` and `F2` that of `F1`, **first-order** ADAA
/// outputs, from the input `x` and the one before it `x'`, `(F1(x) - F1(x')) / (x - x')`: the
/// mean of `g` over the segment between them. Where `|x - x'|` is under 1e-4 (in the units of
/// the curve's argument) it outputs `g((x + x') / 2)`.
///
/// **Second-order** ADAA, from `x`, `x'` and `x''`, with `D(p, q) = (F2(p) - F2(q)) / (p - q)`,
/// outputs `2 (D(x, x') - D(x', x'')) / (x - x'')`. Where `|p - q|` is under 1e-4, `D(p, q)` is
/// `F1((p + q) / 2)`. Where `|x - x''|` is under 1e-4, with `m = (x + x'') / 2`, it outputs the
/// limit `2 (F1(m) - D(m, x')) / (m - x')`, and `g(m)` where `|m - x'|` is under 1e-4 too.
///
/// The history is the input before the drive, and the drive of the frame scales all of it, so a
/// drive change makes no step. The quotients are formed in `f64`, where the cancellation of the
/// antiderivatives is conditioned, and the history is kept in the bank's float. A non-finite input enters the history as 0,
/// so the frame after it (the two after, at second order) reads its segment from 0.
///
/// Each order delays the signal: half a frame at first order, a whole frame at second
/// ([`delay`](Self::delay)). A kind whose antiderivatives are not elementary to the order asked
/// for shapes with the highest order it has ([`ShapeKind::max_antialiasing`]); `Off` is the
/// memoryless curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[non_exhaustive]
pub enum Antialiasing {
    /// The curve at each sample.
    #[default]
    Off,
    /// The mean of the curve over the segment between two inputs.
    FirstOrder,
    /// The second-order form over three inputs: `HardClip` and `Softsign`; the other kinds
    /// shape at first order.
    SecondOrder,
}

impl Antialiasing {
    /// The frames of input an output reads back: 0, 1 or 2, which is also its delay in half
    /// frames.
    const fn depth(self) -> usize {
        match self {
            Self::Off => 0,
            Self::FirstOrder => 1,
            Self::SecondOrder => 2,
        }
    }

    /// The delay in whole frames of a rate `factor` times lower than the one the bank shapes
    /// at, rounded to the nearest frame, a half up: a first-order bank at the rate itself
    /// delays by half a frame, which a whole-frame latency declares as one.
    #[must_use]
    pub fn delay(self, factor: Option<OversamplingFactor>) -> Samples {
        let factor = factor.map_or(1, OversamplingFactor::get);
        Samples((self.depth() + factor) / (2 * factor))
    }

    /// The frames the output rings after the input stops, at a rate `factor` times lower than
    /// the one the bank shapes at, rounded up: the output reads back as many inputs as the order.
    #[must_use]
    pub fn ring_out(self, factor: Option<OversamplingFactor>) -> Samples {
        let factor = factor.map_or(1, OversamplingFactor::get);
        Samples(self.depth().div_ceil(factor))
    }
}

/// The controls of a [`WaveshaperBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveshaperControls {
    /// Input gain into the curve, or the levels per unit of a staircase; the bank floors it
    /// as [`ShapeKind::apply`] does.
    pub drive: Drive,
}

impl WaveshaperControls {
    /// Unity drive: what a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        drive: Drive::UNITY,
    };
}

/// A waveshaper of any width in the float `F`: one curve ([`ShapeKind`]) at one drive shared by every
/// channel, each channel shaped alone. Memoryless unless [`antialiasing`](Self::antialiasing)
/// is on, when each channel keeps its last two inputs. At most [`MAX_CHANNELS`] channels wide.
///
/// Built on the control side for one rate; nothing it holds grows.
#[derive(Clone, Copy, Debug)]
pub struct WaveshaperBank<F: Float> {
    kind: ShapeKind,
    /// The drive, gliding over [`GAIN_GLIDE`].
    drive: Glide<F>,
    /// The steps a drive glide takes: [`GAIN_GLIDE`] at the rate.
    steps: u32,
    /// The order the bank shapes with: the one asked for, within what the kind has.
    antialiasing: Antialiasing,
    /// Each channel's last two inputs before the drive, newest first: per-sample values in `F`.
    history: [[F; 2]; MAX_CHANNELS],
}

impl<F: Float + Lerp> WaveshaperBank<F> {
    /// A waveshaper of `kind` at `rate`, at unity drive, to be aimed.
    #[must_use]
    pub fn new(kind: ShapeKind, rate: SampleRate) -> Self {
        // 5 ms is a few hundred frames at any real rate; an absurd rate saturates rather
        // than panic, since a prepare must not.
        let steps = u32::try_from(GAIN_GLIDE.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        Self {
            kind,
            drive: Glide::landed(F::from_f32(WaveshaperControls::DEFAULT.drive.get())),
            steps,
            antialiasing: Antialiasing::Off,
            history: [[F::ZERO; 2]; MAX_CHANNELS],
        }
    }

    /// Shapes with `order` of antialiasing, up to what the kind has
    /// ([`ShapeKind::max_antialiasing`]); the default is [`Antialiasing::Off`].
    #[must_use]
    pub fn antialiasing(mut self, order: Antialiasing) -> Self {
        self.antialiasing = order.min(self.kind.max_antialiasing());
        self
    }

    /// The order the bank shapes with: the one asked for, within what the kind has.
    #[must_use]
    pub fn antialiasing_order(&self) -> Antialiasing {
        self.antialiasing
    }

    /// Back to rest: [`settle`](Self::settle)s the drive on its target and clears every
    /// channel's history.
    pub fn reset(&mut self) {
        self.settle();
        self.history = [[F::ZERO; 2]; MAX_CHANNELS];
    }

    /// Lands the drive on its target now: back to rest.
    pub fn settle(&mut self) {
        self.drive.settle();
    }

    /// Aims at `controls`: the drive glides over [`GAIN_GLIDE`]. No allocation.
    pub fn aim(&mut self, controls: &WaveshaperControls) {
        self.drive
            .aim_by(F::from_f32(controls.drive.get()), Steps::Fixed(self.steps));
    }

    /// Runs `input` into `output`, each channel through the curve at the drive.
    ///
    /// `lane` is the drive per frame where a port drives it: a frame past its end, or one
    /// that is not finite, reads the drive in force: the glide's value at that frame. Channel `c`
    /// of `input` feeds channel `c` of `output`; a channel past the narrower of the two views is
    /// not touched, and the frames rendered are the fewer of the two
    /// views'. No allocation.
    pub fn process<S: Float>(
        &mut self,
        lane: Option<Mono<'_, S>>,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input.channels().min(output.channels());
        debug_assert!(width <= MAX_CHANNELS, "a bank is at most MAX_CHANNELS wide");
        let kind = self.kind;
        let blended = self.drive.blended(frames);
        let taken = self.drive.taken();
        let order = self.antialiasing;
        if order == Antialiasing::Off && lane.is_none() && blended == 0 {
            let hardness = kind.hardness(self.drive.target());
            for c in 0..width {
                let mut plane = output.plane_mut(c);
                kind.shape_held(
                    hardness,
                    &input.plane(c).samples()[..frames],
                    &mut plane.samples_mut()[..frames],
                );
            }
        } else {
            let mut hardness = [F::ZERO; CHUNK];
            let mut start = 0;
            while start < frames {
                let end = (start + CHUNK).min(frames);
                for (k, h) in hardness[..end - start].iter_mut().enumerate() {
                    let i = start + k;
                    let drive = lane
                        .and_then(|l| l.samples().get(i))
                        .map(|x| F::from_f64(x.to_f64()))
                        .filter(|d| d.is_finite())
                        .unwrap_or_else(|| {
                            let step = taken.saturating_add(1).saturating_add(i as u32);
                            self.drive.at(step)
                        });
                    *h = kind.hardness(drive);
                }
                for c in 0..width {
                    let mut plane = output.plane_mut(c);
                    let (hardness, input) = (&hardness[..end - start], input.plane(c));
                    let (input, output) = (&input.samples()[start..end], plane.samples_mut());
                    let output = &mut output[start..end];
                    if order == Antialiasing::Off {
                        kind.shape_each(hardness, input, output);
                    } else {
                        kind.adaa_each(order, hardness, input, output, &mut self.history[c]);
                    }
                }
                start = end;
            }
        }
        self.drive
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}

#[cfg(test)]
mod tests;
