//! Memoryless waveshaping curves ([`ShapeKind`]): `Tanh`, `Atan`, `Softsign`,
//! `HardClip`, `TriangleFold`, `Crush` and `SoftCrush`, each a few lines of arithmetic, and the
//! [`WaveshaperBank`] that runs one over a block.
//!
//! A bank's drive is a [`Ramp`] over [`GAIN_RAMP`], or a control signal read per frame where a port drives
//! it; a frame whose control signal value is not finite reads the drive in force, the ramp's value at that
//! frame. Frame `i` reads the ramp at step `taken + 1 + i`, then the block's steps are taken.
//!
//! [`Antialiasing`] replaces the curve at each frame by its mean over the segment from the
//! previous input to this one, which removes most of what the curve folds back past the Nyquist
//! frequency. The bank then keeps each channel's last two inputs.

use crate::BlockMismatch;
use crate::block::expect_same_width;
use tutti_buffers::MAX_CHANNELS;
use tutti_buffers::{Float, InputOutput, Mono};
use tutti_units::{Drive, SampleRate, Samples};

use crate::{GAIN_RAMP, Lerp, OversamplingFactor, Ramp, Steps};

/// The frames a pass over a block takes at a time: the hardness of a stretch is held in a
/// stack array.
const CHUNK: usize = 64;

/// Selects which waveshaping curve a [`WaveshaperBank`] applies.
///
/// There is no default curve: the curve is what the waveshaper is, so a waveshaper names it.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
#[non_exhaustive]
pub enum ShapeKind {
    /// `tanh` saturation — smooth, tube-like.
    Tanh,
    /// `atan` saturation — slightly harder knee than tanh.
    Atan,
    /// `softsign` (`x / (1 + |x|)`) — gentle.
    Softsign,
    /// Hard clip to ±1.
    HardClip,
    /// The triangle fold (a wavefolder): the input, at the drive, reflected back into `-1..=1`
    /// at each edge instead of stopping there, so it is the identity inside `-1..=1`, `2 - x`
    /// from 1 to 3, `x - 4` from 3 to 5, and so on with a period of 4. Unlike the clip it
    /// stays in range by turning, so a driven signal gains harmonics as it folds.
    TriangleFold,
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
            ShapeKind::TriangleFold => {
                const $k: ShapeKind = ShapeKind::TriangleFold;
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

use adaa::Guard;

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
        self.curve(self.hardness(F::from_unit(drive)), x)
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
        let (half, one) = (F::lit(0.5), F::ONE);
        match self {
            Self::Tanh => (x * hardness).tanh(),
            // Rescaled to saturate at ±1 while keeping unit slope at the origin.
            Self::Atan => (x * (hardness * F::PI * half)).atan() * (F::lit(2.0) / F::PI),
            Self::Softsign => {
                let y = x * hardness;
                y / (one + y.abs())
            }
            Self::HardClip => (x * hardness).clamp(-one, one),
            Self::TriangleFold => triangle_fold(x * hardness),
            Self::Crush => (x * hardness).round() / hardness,
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
    fn shape_held<F: Float>(self, hardness: F, input: &[F], output: &mut [F]) {
        with_kind!(self, |K| {
            for (o, &x) in output.iter_mut().zip(input) {
                *o = K.curve(hardness, x);
            }
        })
    }

    /// `input` through the curve at a hardness per frame, into `output`.
    #[inline(always)]
    fn shape_each<F: Float>(self, hardness: &[F], input: &[F], output: &mut [F]) {
        with_kind!(self, |K| {
            for ((o, &x), &h) in output.iter_mut().zip(input).zip(hardness) {
                *o = K.curve(h, x);
            }
        })
    }
}

/// `u` reduced by whole periods of the triangle fold to `-2..=2`: `u - 4 round(u / 4)`, exact
/// (each step is a power-of-two scaling, a rounding to a whole number, or a subtraction of two
/// floats within a factor of two). A `u` past the float's range reads as its end, so a finite
/// input never makes a non-finite fold.
#[inline(always)]
fn fold_phase<F: Float>(u: F) -> F {
    let u = u.clamp(-F::MAX * F::lit(0.25), F::MAX * F::lit(0.25));
    u - F::lit(4.0) * (u * F::lit(0.25)).round()
}

/// The triangle fold of `u`: the triangle wave of period 4 and amplitude 1 that is `u` on
/// `-1..=1`, `2 - u` on `1..=3`, and odd (`2 / pi asin(sin(pi u / 2))`).
#[inline(always)]
fn triangle_fold<F: Float>(u: F) -> F {
    let (w, one) = (fold_phase(u), F::ONE);
    if w > one {
        F::lit(2.0) - w
    } else if w < -one {
        F::lit(-2.0) - w
    } else {
        w
    }
}

/// The ninth-order smoothstep: `0 → 0`, `1 → 1`, with the first four
/// derivatives zero at both ends, so each [`ShapeKind::SoftCrush`] step is a
/// smooth S rather than a jump. The coefficients are exact in both widths.
#[inline]
fn smooth9<F: Float>(x: F) -> F {
    let x2 = x * x;
    ((((F::lit(70.0) * x - F::lit(315.0)) * x + F::lit(540.0)) * x - F::lit(420.0)) * x
        + F::lit(126.0))
        * x2
        * x2
        * x
}

/// The order of antiderivative antialiasing (ADAA) a [`WaveshaperBank`] shapes with.
///
/// With `F1` the antiderivative of the curve `g` and `F2` that of `F1`, **first-order** ADAA
/// outputs, from the input `x` and the one before it `x'`, `(F1(x) - F1(x')) / (x - x')`: the
/// mean of `g` over the segment between them, and `g(x)` where they are the same.
///
/// **Second-order** ADAA, from `x`, `x'` and `x''`, with `D(p, q) = (F2(p) - F2(q)) / (p - q)`,
/// outputs `2 (D(x, x') - D(x', x'')) / (x - x'')`.
///
/// The triangle fold's quotient is the two linear pieces' mean where the segment holds a kink
/// (at an odd integer) and is under 1 long, and the periodic antiderivative's difference over a
/// gap of at least 1, so it needs no guard.
///
/// Each quotient is written as differences that do not cancel (`x - x'`, `|x| - |x'|`,
/// `clamp(x) - clamp(x')`, `ln_1p` of a ratio of differences, the hard clip's second order as
/// divided differences of the truncated cube), so it is within a few `eps` of the closed form
/// at any gap, `eps` the bank's float epsilon: measured at most `1.2 eps` at first order
/// for `Tanh`, `Atan`, `Softsign`, `HardClip` and `Crush` and `0.6 eps` at the hard clip's
/// second, at `f32` (`1.4e-7`). Two quotients have no such form and take a guard, `(36
/// eps)^(1/3)` of the larger of 1 and the inputs' magnitudes (0.016 at `f32`, 2.0e-5 at
/// `f64`):
///
/// - `SoftCrush`'s first order, where `|x - x'|` under the guard outputs `g((x + x') / 2)`;
/// - `Softsign`'s second order, where `|x - x''|` under it, with `m = (x + x'') / 2`, outputs the
///   limit `2 (F1(m) - D(m, x')) / (m - x')`, and `g((x + x' + x'') / 3)`, the Taylor form to
///   `O(d^2)`, where `|m - x'|` is under it too.
///
/// There the nested quotient's error is `3 eps` over the span and the limit's is the span's
/// square over 12: at `f32` the floor is 2e-5 (-94 dB) at the guard, measured at most 1.7e-5
/// (-95 dB) over every window (2.2e-6 inside the guard).
///
/// The argument of the curve (`x` times the hardness) is held to `sqrt(MAX) / 4` so a square of
/// it is finite: an infinite input shapes as the curve's end value.
///
/// The history is the input before the drive, and the drive of the frame scales all of it, so a
/// drive change makes no step. The history is kept in the bank's float. A non-finite
/// input enters the history as 0, so the frame after it (the two after, at second order) reads
/// its segment from 0.
///
/// Each order delays the signal: half a frame at first order, a whole frame at second
/// ([`delay`](Self::delay)). Every curve has the first order and [`SecondOrderShape`] the second
/// ([`Waveshape`]); `Off` is the memoryless curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[non_exhaustive]
pub enum Antialiasing {
    /// The curve at each sample.
    #[default]
    Off,
    /// The mean of the curve over the segment between two inputs.
    FirstOrder,
    /// The second-order form over three inputs, of the curves of [`SecondOrderShape`].
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
        Samples::new((self.depth() + factor) / (2 * factor))
    }

    /// The frames the output rings after the input stops, at a rate `factor` times lower than
    /// the one the bank shapes at, rounded up: the output reads back as many inputs as the order.
    #[must_use]
    pub fn ring_out(self, factor: Option<OversamplingFactor>) -> Samples {
        let factor = factor.map_or(1, OversamplingFactor::get);
        Samples::new(self.depth().div_ceil(factor))
    }
}

/// The curves whose antiderivative has an elementary second antiderivative: the two a
/// [`Waveshape`] takes at the [second order](Antialiasing::SecondOrder)
/// ([`Waveshape::second_order`]). The others' antiderivatives are elementary to the first
/// order only: `Tanh` (`ln cosh`), `Atan`, `Crush` and `SoftCrush` (a polynomial per step).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecondOrderShape {
    /// [`ShapeKind::HardClip`].
    HardClip,
    /// [`ShapeKind::Softsign`].
    Softsign,
}

impl SecondOrderShape {
    /// The curve.
    #[must_use]
    pub const fn kind(self) -> ShapeKind {
        match self {
            Self::HardClip => ShapeKind::HardClip,
            Self::Softsign => ShapeKind::Softsign,
        }
    }
}

/// The curve and order of a [`Waveshape`]: the second order holds only a curve that has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Form {
    Memoryless(ShapeKind),
    FirstOrder(ShapeKind),
    SecondOrder(SecondOrderShape),
}

/// A waveshaper's curve and the order of antialiasing it is shaped with: every curve has
/// the first order, and [`SecondOrderShape`] has the second, so a pair that does not exist
/// is not writable.
///
/// A curve without an elementary second antiderivative has no second order:
///
/// ```compile_fail,E0308
/// use tutti_dsp::{ShapeKind, Waveshape};
/// let _ = Waveshape::second_order(ShapeKind::Tanh);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Waveshape {
    form: Form,
}

impl Waveshape {
    /// The curve `kind` with no antialiasing ([`Antialiasing::Off`]): the memoryless curve.
    #[must_use]
    pub const fn new(kind: ShapeKind) -> Self {
        Self {
            form: Form::Memoryless(kind),
        }
    }

    /// The curve shaped with [`Antialiasing::FirstOrder`], which every curve has.
    #[must_use]
    pub const fn first_order(self) -> Self {
        Self {
            form: Form::FirstOrder(self.kind()),
        }
    }

    /// The curve `shape` shaped with [`Antialiasing::SecondOrder`].
    #[must_use]
    pub const fn second_order(shape: SecondOrderShape) -> Self {
        Self {
            form: Form::SecondOrder(shape),
        }
    }

    /// The curve.
    #[must_use]
    pub const fn kind(&self) -> ShapeKind {
        match self.form {
            Form::Memoryless(kind) | Form::FirstOrder(kind) => kind,
            Form::SecondOrder(shape) => shape.kind(),
        }
    }

    /// The order it is shaped with.
    #[must_use]
    pub const fn antialiasing_order(&self) -> Antialiasing {
        match self.form {
            Form::Memoryless(_) => Antialiasing::Off,
            Form::FirstOrder(_) => Antialiasing::FirstOrder,
            Form::SecondOrder(_) => Antialiasing::SecondOrder,
        }
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

/// A waveshaper of any width in the float `F`: one [`Waveshape`] at one drive shared by
/// every channel, each channel shaped alone. Memoryless unless the shape's
/// [`Antialiasing`] is on, when each channel keeps its last two inputs. At most
/// [`MAX_CHANNELS`] channels wide.
///
/// Built on the control side for one rate; nothing it holds grows.
#[derive(Clone, Copy, Debug)]
pub struct WaveshaperBank<F: Float> {
    shape: Waveshape,
    /// The drive, ramping over [`GAIN_RAMP`].
    drive: Ramp<F>,
    /// The steps a drive ramp takes: [`GAIN_RAMP`] at the rate.
    steps: u32,
    /// Each channel's last two inputs before the drive, newest first: per-sample values in `F`.
    history: [[F; 2]; MAX_CHANNELS],
    /// The closeness at which a quotient with no conditioned form takes its limit.
    guard: Guard<F>,
}

impl<F: Float + Lerp> WaveshaperBank<F> {
    /// A waveshaper of `shape` at `rate`, at unity drive, to be set.
    #[must_use]
    pub fn new(shape: Waveshape, rate: SampleRate) -> Self {
        // 5 ms is a few hundred frames at any real rate; an absurd rate saturates rather
        // than panic, since a prepare must not.
        let steps = u32::try_from(GAIN_RAMP.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        Self {
            shape,
            drive: Ramp::new(F::from_unit(WaveshaperControls::DEFAULT.drive)),
            steps,
            history: [[F::ZERO; 2]; MAX_CHANNELS],
            guard: Guard::new(),
        }
    }

    /// The curve and the order the bank shapes with.
    #[must_use]
    pub fn shape(&self) -> Waveshape {
        self.shape
    }

    /// Back to rest: the drive jumps to its target ([`jump_to_target`](Self::jump_to_target)) and
    /// every channel's history clears.
    pub fn reset(&mut self) {
        self.jump_to_target();
        self.history = [[F::ZERO; 2]; MAX_CHANNELS];
    }

    /// Lands the drive on its target now: back to rest.
    pub fn jump_to_target(&mut self) {
        self.drive.jump_to_target();
    }

    /// Aims at `controls`: the drive ramps over [`GAIN_RAMP`]. No allocation.
    pub fn retarget(&mut self, controls: &WaveshaperControls) {
        self.drive
            .retarget_by(F::from_unit(controls.drive), Steps::Fixed(self.steps));
    }

    /// Runs `input` into `output`, each channel through the curve at the drive.
    ///
    /// `signal` is the drive per frame where a port drives it: a frame past its end, or one
    /// that is not finite, reads the drive in force: the ramp's value at that frame. Channel `c`
    /// of `input` feeds channel `c` of `output`. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the output has other channels than the input.
    pub fn process(
        &mut self,
        signal: Option<Mono<'_, F>>,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let width = expect_same_width(block)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        debug_assert!(width <= MAX_CHANNELS, "a bank is at most MAX_CHANNELS wide");
        let kind = self.shape.kind();
        let blended = self.drive.blended_frames(frames);
        let taken = self.drive.steps_taken();
        let form = self.shape.form;
        if form == Form::Memoryless(kind) && signal.is_none() && blended == 0 {
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
                    let drive = signal
                        .and_then(|l| l.samples().get(i))
                        .copied()
                        .filter(|d| d.is_finite())
                        .unwrap_or_else(|| {
                            let step = taken.saturating_add(1).saturating_add(i as u32);
                            self.drive.at(step)
                        });
                    *h = kind.hardness(drive);
                }
                for (c, history) in self.history[..width].iter_mut().enumerate() {
                    let mut plane = output.plane_mut(c);
                    let (hardness, input) = (&hardness[..end - start], input.plane(c));
                    let (input, output) = (&input.samples()[start..end], plane.samples_mut());
                    let output = &mut output[start..end];
                    match form {
                        Form::Memoryless(_) => kind.shape_each(hardness, input, output),
                        Form::FirstOrder(_) => {
                            kind.first_each(self.guard, hardness, input, output, history);
                        }
                        Form::SecondOrder(shape) => {
                            shape.second_each(self.guard, hardness, input, output, history);
                        }
                    }
                }
                start = end;
            }
        }
        self.drive
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
