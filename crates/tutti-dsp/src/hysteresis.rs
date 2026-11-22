//! [`Hysteresis`]: the Jiles-Atherton model of magnetic hysteresis, discretized by the
//! trapezoidal rule with a capped Newton solve per sample.
//!
//! # The model
//!
//! A ferromagnet's magnetization `M` follows the applied field `H` along a loop that depends
//! on where it has been (Jiles and Atherton 1986, "Theory of ferromagnetic hysteresis"). The
//! effective field is `H_e = H + alpha M`, the field without loss is Langevin's
//! `M_an = M_s L(H_e / a)` with `L(x) = coth x - 1 / x`, and `M` is pulled toward `M_an` by
//! pinning of strength `k`, `c` of the pull being reversible. The slope the discretization of
//! Holters and Zoelzer (2016, "Circuit simulation with inductors and transformers based on the
//! Jiles-Atherton model of magnetization") integrates is
//!
//! ```text
//! dM/dH_e = (1 - c) max(0, delta (M_an - M)) / k + c dM_an/dH_e,    dM/dH = (dM/dH_e) / (1 - alpha dM/dH_e)
//! ```
//!
//! with `delta = sign(dH)`. The `max(0, ..)` is the model's `delta_M`: magnetization never
//! moves away from the curve it is pulled toward.
//!
//! # The dimensionless form
//!
//! With `h = H / a`, `m = M / M_s`, `kappa = k / a` and `alpha' = alpha M_s / a` the model is
//! `x_e = h + alpha' m`, `m_an = L(x_e)`,
//!
//! ```text
//! chi = (1 - c) max(0, delta (L(x_e) - m)) / kappa + c L'(x_e),     dm/dh = chi / (1 - alpha' chi)
//! ```
//!
//! so the model is three numbers, `alpha'`, `kappa` and `c`; `M_s` is the unit of the output
//! (`m` reaches 1 at saturation) and `a` the unit of the input. The physical set is the one
//! of the Jiles-Atherton literature for a steel (`M_s` 1.6e6 A/m, `a` 1100 A/m, `k` 400 A/m,
//! `alpha` 1.6e-3, `c` 0.2); the figures are the named constants below. The set is not
//! checked against the 1986 paper's tables: tests compare the discretization with this
//! equation, not the set's published loop.
//!
//! The user's controls map onto the set as follows. The signal is the field in units of
//! `a`: `h = amplitude(drive) x`, so a sample of 1 at 0 dB drive is `H = a`. Saturation is
//! `c` itself, the share of the magnetization that is reversible: 0 is the widest loop the
//! pinning allows and 1 a memoryless saturating curve. The coercivity position `w` in
//! `0..=1` sets `kappa = kappa_ref 16^(2 w - 1)`: `w = 0.5` is the set's `k = 400 A/m`, and
//! each half of the scale is a factor of 16. `alpha'` stays the set's (2.327): `alpha' / 3 < 1`,
//! so the material has no spontaneous magnetization, and along every solution tested
//! `alpha' chi <= alpha' / 3`, so the slope's denominator stays above 0.22.
//!
//! # The discretization
//!
//! The model is rate independent: the loop depends on the path of `h`, not on how fast it is
//! walked. One step takes `h` from `h_p` to `h` and `m` from `m_p` to the root of
//!
//! ```text
//! f(m) = m - m_p - (dh / 2) (S(h_p, m_p) + S(h, m)) = 0,        S = dm/dh at delta = sign(dh)
//! ```
//!
//! (the trapezoidal rule), solved by `HYSTERESIS_CAP` Newton steps from the better of the
//! explicit Euler step `m_p + dh S(h_p, m_p)` and `m_p`. No closed form exists for the root,
//! so the seed is the predictor, whose distance from the root is `e_0 <= (dh^2 / 2) |m''|`.
//! A step covers at most `STEP` in `h`: a larger move is split into equal substeps (at most
//! `MAX_SUBSTEPS`, the most a sample of magnitude 2 at the highest drive makes), so the
//! Newton recurrence `e' <= (K / 2) e^2`, `K = sup|f''| / inf|f'|`, starts from a seed within
//! `STEP^2 / 2 * |m''|` for every input in range. `f' = 1 - (dh / 2) S_m >= 1` where the pull
//! is active, and `S_m` is small where it is not. `f''` is not bounded in closed form to a
//! useful figure (the slope's denominator and the pull's corner both enter it), so the cap is
//! the measured one: see `HYSTERESIS_CAP`.
//!
//! The Langevin function and its derivatives below `SERIES_BELOW` are evaluated from the
//! series `x / 3 - x^3 / 45 + 2 x^5 / 945 - ..` (eight terms, truncation under `1e-19` at the
//! threshold): the closed forms lose `eps / x`, `eps / x^2` and `eps / x^3` there to
//! cancellation between terms of size `1 / x^n`.

use crate::BlockMismatch;
use crate::block::expect_widths;
use crate::newton::capped_newton_steps;
use crate::{GAIN_RAMP, Lerp, Ramp, Steps};
use tutti_buffers::{ChannelLayout, Float, InputOutput};
use tutti_units::{Db, Normalized, SampleRate};

/// The saturation magnetization `M_s` of the set, amperes per metre.
const SATURATION_MAGNETIZATION: f64 = 1.6e6;
/// The field scale `a` of the set, amperes per metre: a sample of 1 at 0 dB is this field.
const FIELD_SCALE: f64 = 1_100.0;
/// The pinning `k` of the set, amperes per metre.
const PINNING: f64 = 400.0;
/// The mean-field coupling `alpha` of the set.
const COUPLING: f64 = 1.6e-3;
/// `alpha' = alpha M_s / a`.
const ALPHA: f64 = COUPLING * SATURATION_MAGNETIZATION / FIELD_SCALE;
/// `kappa = k / a` of the set: the coercivity position 0.5.
const KAPPA_REF: f64 = PINNING / FIELD_SCALE;
/// The factor `kappa` moves over each half of the coercivity scale.
const KAPPA_SPAN: f64 = 16.0;

/// The Newton steps of every substep. The seed is the Euler predictor, within 0.09 of the root
/// at a `STEP` of 0.25 (the largest measured over the range of the controls); a step takes
/// the error `e` to about `(K / 2) e^2`, and four steps from there leave a residual of `4e-12`
/// at the worst corner (the default material), under the test's `1e-10`. Three leave `2e-6`.
pub(crate) const HYSTERESIS_CAP: usize = 4;

/// The widest move of `h` one Newton solve covers.
pub(crate) const STEP: f64 = 0.25;
/// The most substeps a sample takes: the move of an input of magnitude 2 at the highest
/// drive (`+18 dB`, `h` of 7.9) is 31.6, and 128 of `STEP` cover 32. A larger move takes 128
/// wider substeps.
pub(crate) const MAX_SUBSTEPS: usize = 128;

/// Below this `|x|` the Langevin function and its derivatives come from their series.
pub(crate) const SERIES_BELOW: f64 = 0.25;

/// The series coefficients `c_n` of `L(x) = sum c_n x^(2n - 1)`: `2^(2n) B_(2n) / (2n)!`.
const LANGEVIN_SERIES: [f64; 8] = [
    1.0 / 3.0,
    -1.0 / 45.0,
    2.0 / 945.0,
    -1.0 / 4725.0,
    2.0 / 93_555.0,
    -1_382.0 / 638_512_875.0,
    4.0 / 18_243_225.0,
    -3_617.0 / 162_820_783_125.0,
];

tutti_units::ranged! {
    /// The field into a [`Hysteresis`] in decibels over one `a` (the model's field scale) per
    /// unit of signal: from [`MIN`](Self::MIN) (-24 dB) to [`MAX`](Self::MAX) (+18 dB), the
    /// range the solver's residual is stated over.
    HysteresisDrive(Db), HysteresisDriveError, "a drive from -24 dB to +18 dB",
    |raw| raw.get() >= -24.0 && raw.get() <= 18.0
}

impl HysteresisDrive {
    /// The lowest drive: -24 dB, a field of 0.063 `a` at full scale, where the loop is thin.
    pub const MIN: Self = Self::of(Db::new(-24.0));
    /// The highest drive: +18 dB, a field of 7.9 `a` at full scale, deep in saturation.
    pub const MAX: Self = Self::of(Db::new(18.0));
    /// +12 dB, a field of 4 `a`: the [`HysteresisControls::DEFAULT`] drive.
    pub const DEFAULT: Self = Self::of(Db::new(12.0));

    /// The drive a live control value reads as: `raw` held within [`MIN`](Self::MIN) and
    /// [`MAX`](Self::MAX), and [`DEFAULT`](Self::DEFAULT) for a NaN. A control signal arrives
    /// every frame with no caller to refuse it to, so this is the one place a value out of
    /// range is changed; a configured drive is refused by [`new`](Self::new) instead.
    #[must_use]
    pub fn held(raw: Db) -> Self {
        Self::new(raw).unwrap_or_else(|| {
            if raw.get().is_nan() {
                Self::DEFAULT
            } else if raw.get() < 0.0 {
                Self::MIN
            } else {
                Self::MAX
            }
        })
    }
}

/// The live controls of a [`Hysteresis`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HysteresisControls {
    /// The field into the material; in range by its type.
    pub drive: HysteresisDrive,
}

impl HysteresisControls {
    /// +12 dB.
    pub const DEFAULT: Self = Self {
        drive: HysteresisDrive::DEFAULT,
    };
}

/// The Langevin function `L(x) = coth x - 1 / x` with its first and second derivatives, from
/// the series below `SERIES_BELOW` and from the closed forms above.
#[inline]
#[must_use]
pub(crate) fn langevin<F: Float>(x: F) -> (F, F, F) {
    if x.abs() < F::from_design(SERIES_BELOW) {
        langevin_series(x)
    } else {
        langevin_closed(x)
    }
}

/// `L`, `L'` and `L''` from the series `sum c_n x^(2n - 1)`, by Horner's rule in `x^2`.
#[inline]
#[must_use]
pub(crate) fn langevin_series<F: Float>(x: F) -> (F, F, F) {
    let t = x * x;
    let (mut l, mut l1, mut l2) = (F::ZERO, F::ZERO, F::ZERO);
    for j in (0..LANGEVIN_SERIES.len()).rev() {
        let c = F::from_design(LANGEVIN_SERIES[j]);
        let odd = F::from_design((2 * j + 1) as f64);
        l = l * t + c;
        l1 = l1 * t + c * odd;
        if j >= 1 {
            l2 = l2 * t + c * odd * F::from_design((2 * j) as f64);
        }
    }
    (x * l, l1, x * l2)
}

/// `L`, `L'` and `L''` from the closed forms, with `e = exp(-2 |x|)` so nothing overflows:
/// `coth = (1 + e) / (1 - e)`, `1 / sinh^2 = 4 e / (1 - e)^2` and
/// `L'' = 2 (coth / sinh^2 - 1 / x^3)`.
#[inline]
#[must_use]
pub(crate) fn langevin_closed<F: Float>(x: F) -> (F, F, F) {
    let a = x.abs();
    let e = (F::lit(-2.0) * a).exp();
    let inv = F::ONE / (F::ONE - e);
    let coth = (F::ONE + e) * inv;
    let csch2 = F::lit(4.0) * e * inv * inv;
    let inv_a = F::ONE / a;
    let l = coth - inv_a;
    let l1 = inv_a * inv_a - csch2;
    let l2 = F::lit(2.0) * (coth * csch2 - inv_a * inv_a * inv_a);
    (x.signum() * l, l1, x.signum() * l2)
}

/// The model's three numbers in the float `F`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Model<F: Float> {
    alpha: F,
    inv_kappa: F,
    c: F,
    one_minus_c: F,
}

impl<F: Float> Model<F> {
    /// The model at saturation `c` and coercivity position `w` (see the module).
    pub(crate) fn new(saturation: Normalized, coercivity: Normalized) -> Self {
        let c = saturation.get();
        let kappa = KAPPA_REF * libm::pow(KAPPA_SPAN, 2.0 * coercivity.get() - 1.0);
        Self::from_parts(ALPHA, kappa, c)
    }

    pub(crate) fn from_parts(alpha: f64, kappa: f64, c: f64) -> Self {
        Self {
            alpha: F::from_design(alpha),
            inv_kappa: F::from_design(1.0 / kappa),
            c: F::from_design(c),
            one_minus_c: F::from_design(1.0 - c),
        }
    }

    /// `dm/dh` and `d(dm/dh)/dm` at the field `h`, magnetization `m` and direction `delta`
    /// (`+-1`).
    #[inline]
    pub(crate) fn slope(&self, h: F, m: F, delta: F) -> (F, F) {
        let (l, l1, l2) = langevin(h + self.alpha * m);
        let pull = delta * (l - m);
        let active = pull > F::ZERO;
        let irreversible = if active {
            pull * self.inv_kappa
        } else {
            F::ZERO
        };
        let chi = self.one_minus_c * irreversible + self.c * l1;
        let den = F::ONE - self.alpha * chi;
        let dpull = if active {
            self.one_minus_c * delta * (self.alpha * l1 - F::ONE) * self.inv_kappa
        } else {
            F::ZERO
        };
        let dchi = dpull + self.c * self.alpha * l2;
        (chi / den, dchi / (den * den))
    }

    /// The trapezoidal step of `m` from `(h_p, m_p)` to the field `h`, the root of the module's
    /// `f` after `HYSTERESIS_CAP` Newton steps.
    #[inline]
    pub(crate) fn step(&self, h_p: F, m_p: F, h: F) -> F {
        self.step_capped::<HYSTERESIS_CAP>(h_p, m_p, h)
    }

    #[inline]
    pub(crate) fn step_capped<const CAP: usize>(&self, h_p: F, m_p: F, h: F) -> F {
        let dh = h - h_p;
        let delta = dh.signum();
        let (s_p, _) = self.slope(h_p, m_p, delta);
        let half = F::lit(0.5) * dh;
        capped_newton_steps::<F, CAP>(
            |m| {
                let (s, ds) = self.slope(h, m, delta);
                (m - m_p - half * (s_p + s), F::ONE - half * ds)
            },
            m_p + dh * s_p,
            m_p,
        )
    }

    /// The magnetization after the move from `(h_p, m_p)` to `h`, in at most
    /// `MAX_SUBSTEPS` substeps of at most `STEP`.
    #[inline]
    fn advance(&self, h_p: F, m_p: F, h: F) -> F {
        let dh = h - h_p;
        let wanted = libm::ceil(Into::<f64>::into(dh.abs()) / STEP) as usize;
        let n = wanted.clamp(1, MAX_SUBSTEPS);
        let (mut at, mut m) = (h_p, m_p);
        for k in 1..=n {
            let next = if k == n {
                h
            } else {
                h_p + dh * F::from_design(k as f64 / n as f64)
            };
            m = self.step(at, m, next);
            at = next;
        }
        m
    }
}

/// A hysteresis stage of any width in the float `F`: every channel a piece of magnetic
/// material driven by the signal as its field, the output its magnetization in units of the
/// saturation (`-1..1`). At most [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) channels wide.
///
/// The material keeps its remanence: when the input stops at 0 the output holds the
/// magnetization it was left at, a constant that never decays (a node declares an unbounded
/// tail for it, and a graph blocks it with a DC blocker where it is unwanted). The nonlinearity
/// creates harmonics that an oversampled node ([`Oversampler`](crate::Oversampler)) filters
/// out. Built on the control side; nothing it holds grows.
#[derive(Clone, Debug)]
pub struct Hysteresis<F: Float> {
    model: Model<F>,
    /// The field's gain, ramping over [`GAIN_RAMP`].
    drive: Ramp<F>,
    steps: u32,
    /// The gain held for the frames of the current chunk.
    gain: F,
    /// The frames into the current chunk, `0..CHUNK`.
    into: usize,
    /// Each channel's last field and magnetization.
    state: Vec<(F, F)>,
}

/// The frames a gain holds for: a control grid counted from the last reset, so the render does
/// not depend on where blocks fall.
const CHUNK: usize = 16;

impl<F: Float + Lerp> Hysteresis<F> {
    /// A stage of `width` channels running at `rate`, of the given saturation and coercivity
    /// positions (see the module), at [`HysteresisControls::DEFAULT`].
    #[must_use]
    pub fn new(
        width: ChannelLayout,
        rate: SampleRate,
        saturation: Normalized,
        coercivity: Normalized,
    ) -> Self {
        let steps = u32::try_from(GAIN_RAMP.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        let drive = F::from_design(HysteresisControls::DEFAULT.drive.get().to_amplitude().get());
        Self {
            model: Model::new(saturation, coercivity),
            drive: Ramp::new(drive),
            steps,
            gain: drive,
            into: 0,
            state: vec![(F::ZERO, F::ZERO); usize::from(width.count())],
        }
    }

    /// Aims at `controls`: the drive ramps over [`GAIN_RAMP`]. No allocation.
    pub fn retarget(&mut self, controls: &HysteresisControls) {
        let drive = F::from_design(controls.drive.get().to_amplitude().get());
        self.drive.retarget_by(drive, Steps::Fixed(self.steps));
    }

    /// Lands the drive on its target now.
    pub fn jump_to_target(&mut self) {
        self.drive.jump_to_target();
        self.gain = self.drive.at(0);
        self.into = 0;
    }

    /// Back to rest: demagnetized and the drive on its target.
    pub fn reset(&mut self) {
        self.state.fill((F::ZERO, F::ZERO));
        self.jump_to_target();
    }

    /// Runs `input` into `output`, every channel through the material. No allocation, at most
    /// `MAX_SUBSTEPS` solves a sample.
    ///
    /// The gain is designed every 16 frames from the first frame after a reset, so the render
    /// is the same whatever the blocks.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's channels are not the stage's width.
    pub fn process(&mut self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        let width = self.state.len();
        expect_widths(block, width, width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let first = self.drive.steps_taken().saturating_add(1);
        let mut start = 0;
        while start < frames {
            if self.into == 0 {
                let k = first.saturating_add(u32::try_from(start).unwrap_or(u32::MAX));
                self.gain = self.drive.at(k);
            }
            let end = (start + CHUNK - self.into).min(frames);
            let gain = self.gain;
            let model = self.model;
            for (c, state) in self.state.iter_mut().enumerate() {
                let x = &input.plane(c).samples()[start..end];
                let mut plane = output.plane_mut(c);
                let y = &mut plane.samples_mut()[start..end];
                for (y, &x) in y.iter_mut().zip(x) {
                    let (h_p, m_p) = *state;
                    let h = gain * x;
                    let m = model.advance(h_p, m_p, h);
                    *state = (h, m);
                    *y = m;
                }
            }
            self.into = (self.into + end - start) % CHUNK;
            start = end;
        }
        self.drive
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
