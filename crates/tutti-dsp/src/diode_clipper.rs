//! [`DiodeClipper`]: the RC-loaded antiparallel diode pair, solved by the trapezoidal rule
//! with a capped Newton iteration seeded by a Wright omega closed form.
//!
//! # The circuit
//!
//! A source `v_in` drives a resistor `R` into a capacitor `C` in parallel with two diodes
//! of opposite polarity (Yeh, Abel and Smith 2007, "Simplified, physically-informed models of
//! distortion and overdrive guitar effects pedals"). With the Shockley diode current
//! `I_s (exp(v / n V_T) - 1)` the pair carries `2 I_s sinh(v / n V_T)`, and the voltage `v`
//! across the capacitor obeys
//!
//! ```text
//! C dv/dt = (v_in - v) / R - 2 I_s sinh(v / (n V_T))
//! ```
//!
//! # The dimensionless form
//!
//! With `u = v / (n V_T)`, `x = v_in / (n V_T)`, `w_c = 1 / (R C)` and
//! `beta = 2 R I_s / (n V_T)` the equation is
//!
//! ```text
//! du/dt = w_c (x - u - beta sinh u)
//! ```
//!
//! so the circuit is one dimensionless number, `beta`, and a rate. The parameters are the
//! 1N4148 fit of the paper (`I_s` 2.52 nA, `n` 1.752), the thermal voltage at 300 K
//! (25.85 mV) and `R` 2.2 kOhm, which give `beta` about `2.4e-4`. The user sets the RC
//! corner `f_c = w_c / 2 pi` ([`DiodeClipperControls::cutoff`]) and a drive
//! ([`ClipperDrive`]) that scales the signal before the circuit: one unit of
//! signal at 0 dB is 1 V, and the output is the capacitor voltage in the same
//! unit (volts). No electrical quantity is a unit type here, so these are named constants.
//!
//! **Small signal.** `sinh u` is `u` for small `u`, so the equation is a one-pole low-pass of
//! corner `(1 + beta) f_c`: the diodes add a conductance of `beta` times the resistor's.
//!
//! **Static limit.** At `w_c` far above the signal `du/dt` is small and `x = u + beta sinh u`.
//! With `sinh u ~ e^u / 2` (an error of `beta e^-u / 2` in the equation, at most `beta / 2`
//! in `u` for `u >= 0`) the solution is `u = x - omega(x + ln(beta / 2))`, with `omega` the
//! Wright omega function (`omega + ln omega = x`), after Paiva, Pakarinen, Esqueda and
//! Valimaki (2012, "Real-time audio transformer emulation") for the resistor-diode circuit.
//!
//! # The discretization
//!
//! The trapezoidal rule with the step prewarped to the corner, `g = tan(pi f_c / f_s)`
//! (`f_s` the rate the clipper runs at), so the linear circuit's corner is `f_c` exactly in the
//! analog-to-digital map `s = (2 f_s) (z - 1) / (z + 1)`. With `F(u) = x - u - beta sinh u`
//!
//! ```text
//! u[n] = u[n-1] + g (F[n-1] + F[n])        i.e.   a u + g beta sinh u = p + g x[n],
//! a = 1 + g,   p = u[n-1] + g F[n-1]
//! ```
//!
//! and the state is `p`, recomputed from each solution as `p = u + g F(u)`, so rounding
//! does not accumulate in it. The linear case is the
//! one-pole low-pass of `g`, whose gain is `1 / sqrt(1 + (tan(pi f / f_s) / g)^2)` and whose
//! impulse response is positive and sums to 1 for `g <= 1`.
//!
//! # The closed-form seed
//!
//! The step equation is `a u + g beta sinh u = q` with `q = p + g x`; with `b = g beta / 2` and
//! `sinh u ~ e^u / 2` it is `a u + b e^u = q`. Put `z = (b / a) e^u`: then `u = q / a - z` and
//! `z e^z = (b / a) e^(q / a)`, so
//!
//! ```text
//! z = omega(q / a + ln(b / a)),     u_seed = q / a - z
//! ```
//!
//! (for `z > 1` the form `u_seed = ln z + ln(a / b)` avoids the cancellation of `q / a - z`).
//! The step is odd in `q`, and the seed is taken for `|q|` (where the root is at least 0, so
//! a seed below 0 reads 0) with the sign restored. The static
//! limit above is the step with `g -> infinity`.
//!
//! The seed leaves a residual of `b e^-u` (the dropped half of `sinh`), so its error in `u` is
//! at most `e_0 = (b / a) e^-u / (1 + (b / a) e^u)`, under `beta / 2` for every `g`. Newton's
//! recurrence `e' <= (M / 2m) e^2` has `M / m = (b/a) e^u / (1 + (b/a) e^u) < 1`, so one step
//! leaves at most `(b / a)^3 / 2 < 1e-12` (at `u` near 0) and the [`NEWTON_CAP`] of two steps
//! leaves under `1e-24`: below the rounding of `f64`. The residual of the discretized equation
//! after the cap is that rounding, at every `g` and every drive in
//! [`ClipperDrive::MIN`]`..=`[`ClipperDrive::MAX`].
//!
//! The solver starts from the better of the seed and the previous sample's solution (the one
//! with the smaller residual): where the signal is slow the solution it follows is the closer.
//!
//! [`NEWTON_CAP`]: crate::newton

use crate::BlockMismatch;
use crate::block::expect_widths;
use crate::newton::capped_newton;
use crate::onepole::ring_frames;
use crate::wright_omega::wright_omega;
use crate::{GAIN_RAMP, Lerp, Ramp, Steps, TAIL_FLOOR};
use tutti_buffers::{ChannelLayout, Float, InputOutput};
use tutti_units::{Db, Hz, SampleRate, Samples};

/// The frames a coefficient set (the prewarped `g` and what follows from it) holds for: a
/// control grid of the oversampled stream, counted from the last reset, so the render does
/// not depend on where blocks fall.
const CHUNK: usize = 16;

/// Thermal voltage `kT / q` at 300 K, volts.
const THERMAL_VOLTAGE: f64 = 0.025_85;
/// The ideality factor `n` of the 1N4148 fit (Yeh, Abel and Smith 2007).
const IDEALITY: f64 = 1.752;
/// The saturation current `I_s` of the 1N4148 fit, amperes.
const SATURATION_CURRENT: f64 = 2.52e-9;
/// The resistor `R`, ohms.
const RESISTANCE: f64 = 2_200.0;
/// The volts one unit of signal is at 0 dB drive.
const FULL_SCALE: f64 = 1.0;

/// `n V_T`, volts.
const SLOPE_VOLTAGE: f64 = IDEALITY * THERMAL_VOLTAGE;
/// `beta = 2 R I_s / (n V_T)`: the diodes' conductance at 0 V over the resistor's.
const BETA: f64 = 2.0 * RESISTANCE * SATURATION_CURRENT / SLOPE_VOLTAGE;
/// `u` per unit of signal at 0 dB drive: `FULL_SCALE / (n V_T)`.
const SCALE: f64 = FULL_SCALE / SLOPE_VOLTAGE;

tutti_units::ranged! {
    /// The gain into a [`DiodeClipper`], in decibels over 1 V per unit of signal: from
    /// [`MIN`](Self::MIN) (-60 dB) to [`MAX`](Self::MAX) (+60 dB), the range the solver's
    /// residual is stated over.
    ClipperDrive(Db), ClipperDriveError, "a drive from -60 dB to +60 dB",
    |raw| raw.get() >= -60.0 && raw.get() <= 60.0
}

impl ClipperDrive {
    /// The lowest drive: -60 dB.
    pub const MIN: Self = Self::of(Db::new(-60.0));
    /// The highest drive: +60 dB, where `u` reaches about 19.
    pub const MAX: Self = Self::of(Db::new(60.0));
    /// +12 dB, the [`DiodeClipperControls::DEFAULT`] drive.
    pub const DEFAULT: Self = Self::of(Db::new(12.0));

    /// The drive a live control value reads as: `raw` held within [`MIN`](Self::MIN) and
    /// [`MAX`](Self::MAX), and [`DEFAULT`](Self::DEFAULT) for a NaN. A control
    /// signal arrives every frame with no caller to refuse it to, so this is the one place a
    /// value out of range is changed; a configured drive is refused by [`new`](Self::new)
    /// instead.
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

/// The controls of a [`DiodeClipper`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiodeClipperControls {
    /// The RC corner `1 / (2 pi R C)`: the low-pass the circuit is below the diodes' knee.
    /// Held within [`MIN_CUTOFF`](Self::MIN_CUTOFF) and 0.4 of the rate the clipper runs at
    /// (the prewarp `tan(pi f_c / f_s)` diverges at the Nyquist frequency); a value that is not
    /// finite reads as [`DEFAULT`](Self::DEFAULT)'s. The upper bound depends on the rate, which
    /// is not known where the controls are written, so it is held when the clipper is built
    /// for its rate rather than refused as the drive is.
    pub cutoff: Hz,
    /// The gain into the circuit; in range by its type.
    pub drive: ClipperDrive,
}

impl DiodeClipperControls {
    /// The lowest cutoff: 20 Hz, the bottom of the audible band; below it the circuit is a
    /// DC-coupled shunt and rings for seconds.
    pub const MIN_CUTOFF: Hz = Hz::new(20.0);
    /// 5 kHz at +12 dB.
    pub const DEFAULT: Self = Self {
        cutoff: Hz::new(5_000.0),
        drive: ClipperDrive::DEFAULT,
    };

    /// The frames it rings after its input stops, to [`TAIL_FLOOR`], at a clipper running at
    /// `rate`: the decay of the linear circuit's pole `(1 - g') / (1 + g')`, `g' = (1 + beta)
    /// tan(pi f_c / f_s)`, erring long (a driven diode pair damps faster).
    #[must_use]
    pub fn tail(&self, rate: SampleRate) -> Samples {
        let g = (1.0 + BETA)
            * libm::tan(core::f64::consts::PI * self.held_cutoff(rate).get() / rate.get());
        let pole = ((1.0 - g) / (1.0 + g)).abs();
        Samples::new((ring_frames(pole, TAIL_FLOOR).ceil() as usize).saturating_add(1))
    }

    fn held_cutoff(&self, rate: SampleRate) -> Hz {
        self.cutoff
            .finite_or(Self::DEFAULT.cutoff)
            .clamp(Self::MIN_CUTOFF, rate.nyquist_scaled(0.8))
    }
}

/// The coefficients of the step equation `a u + g beta sinh u = q` for one prewarped `g`.
#[derive(Clone, Copy, Debug)]
struct Step<F: Float> {
    g: F,
    a: F,
    inv_a: F,
    /// `g beta`.
    gb: F,
    /// `ln(a / b)`, `b = g beta / 2`.
    ln_ab: F,
    /// `u` per unit of input: the drive's amplitude times `SCALE`.
    gain: F,
}

impl<F: Float> Step<F> {
    fn new(g: F, gain: F) -> Self {
        let a = F::ONE + g;
        let gb = g * F::from_design(BETA);
        Self {
            g,
            a,
            inv_a: F::ONE / a,
            gb,
            ln_ab: (F::lit(2.0) * a / gb).ln(),
            gain,
        }
    }

    /// The closed-form seed for `q`: the module's `u_seed`, odd in `q`.
    #[inline]
    fn seed(&self, q: F) -> F {
        let qa = q.abs() * self.inv_a;
        let z = wright_omega(qa - self.ln_ab);
        let u = if z > F::ONE {
            z.ln() + self.ln_ab
        } else {
            qa - z
        };
        q.signum() * u.max(F::ZERO)
    }

    /// `sinh u` and `cosh u`, from one exponential of `|u|` (at least 1, so its inverse is
    /// safe).
    #[inline]
    fn hyperbolic(u: F) -> (F, F) {
        let e = u.abs().exp();
        let inv = F::ONE / e;
        let half = F::lit(0.5);
        (u.signum() * (e - inv) * half, (e + inv) * half)
    }

    /// `q - a u - g beta sinh u` and its slope at `u`.
    #[inline]
    fn residual(&self, q: F, u: F) -> (F, F) {
        let (sinh, cosh) = Self::hyperbolic(u);
        (q - self.a * u - self.gb * sinh, -(self.a + self.gb * cosh))
    }

    /// The trapezoid's state after the solution `u` for the input `x`: `p = u + g F(u)`.
    #[inline]
    fn state(&self, u: F, x: F) -> F {
        let (sinh, _) = Self::hyperbolic(u);
        (F::ONE - self.g) * u + self.g * x - self.gb * sinh
    }
}

/// A diode clipper of any width in the float `F`: every channel an RC circuit with an
/// antiparallel diode pair across the capacitor, at one cutoff and drive. At most
/// [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) channels wide.
///
/// It runs at the rate it is built for, which is a multiple of the signal's: the nonlinearity
/// creates harmonics that an oversampled node ([`Oversampler`](crate::Oversampler)) filters
/// out. Built on the control side; nothing it holds grows.
#[derive(Clone, Debug)]
pub struct DiodeClipper<F: Float> {
    rate: SampleRate,
    /// The cutoff in hertz, ramping over [`GAIN_RAMP`].
    cutoff: Ramp<F>,
    /// The drive's amplitude, ramping over [`GAIN_RAMP`].
    drive: Ramp<F>,
    steps: u32,
    /// The step equation's coefficients, held for the frames of the current chunk.
    step: Step<F>,
    /// The frames into the current chunk, `0..CHUNK`.
    into: usize,
    /// Each channel's last solution `u` and the trapezoid's state `p`.
    state: Vec<(F, F)>,
}

impl<F: Float + Lerp> DiodeClipper<F> {
    /// A clipper of `width` channels running at `rate`, at [`DiodeClipperControls::DEFAULT`].
    #[must_use]
    pub fn new(width: ChannelLayout, rate: SampleRate) -> Self {
        let steps = u32::try_from(GAIN_RAMP.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        let controls = DiodeClipperControls::DEFAULT;
        let cutoff = F::from_unit(controls.held_cutoff(rate));
        let drive = F::from_design(controls.drive.get().to_amplitude().get());
        let mut clipper = Self {
            rate,
            cutoff: Ramp::new(cutoff),
            drive: Ramp::new(drive),
            steps,
            step: Step::new(F::ONE, F::ONE),
            into: 0,
            state: vec![(F::ZERO, F::ZERO); usize::from(width.count())],
        };
        clipper.step = clipper.design(0);
        clipper
    }

    /// Aims at `controls`: the cutoff and the drive ramp over [`GAIN_RAMP`]. No allocation.
    pub fn retarget(&mut self, controls: &DiodeClipperControls) {
        let cutoff = F::from_unit(controls.held_cutoff(self.rate));
        let drive = F::from_design(controls.drive.get().to_amplitude().get());
        self.cutoff.retarget_by(cutoff, Steps::Fixed(self.steps));
        self.drive.retarget_by(drive, Steps::Fixed(self.steps));
    }

    /// Lands the cutoff and drive on their targets now.
    pub fn jump_to_target(&mut self) {
        self.cutoff.jump_to_target();
        self.drive.jump_to_target();
        self.step = self.design(0);
        self.into = 0;
    }

    /// Back to rest: the capacitor discharged and the controls on their targets.
    pub fn reset(&mut self) {
        self.state.fill((F::ZERO, F::ZERO));
        self.jump_to_target();
    }

    /// The coefficients at ramp step `k`.
    fn design(&self, k: u32) -> Step<F> {
        let rate = F::from_unit(self.rate);
        let g = (F::PI * self.cutoff.at(k) / rate).tan();
        Step::new(g, self.drive.at(k) * F::from_design(SCALE))
    }

    /// Runs `input` into `output`, every channel through the circuit; the output is the
    /// capacitor voltage in volts, 1 V a unit of signal. No allocation.
    ///
    /// The coefficients are designed every 16 frames from the first frame after a reset, so
    /// the render is the same whatever the blocks.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's channels are not the clipper's width.
    pub fn process(&mut self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        let width = self.state.len();
        expect_widths(block, width, width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let first = self.cutoff.steps_taken().saturating_add(1);
        let mut start = 0;
        while start < frames {
            if self.into == 0 {
                let k = first.saturating_add(u32::try_from(start).unwrap_or(u32::MAX));
                self.step = self.design(k);
            }
            let end = (start + CHUNK - self.into).min(frames);
            let step = self.step;
            let volts = F::from_design(1.0 / SCALE);
            for (c, state) in self.state.iter_mut().enumerate() {
                let x = &input.plane(c).samples()[start..end];
                let mut plane = output.plane_mut(c);
                let y = &mut plane.samples_mut()[start..end];
                for (y, &x) in y.iter_mut().zip(x) {
                    let (u, p) = *state;
                    let x = step.gain * x;
                    let q = p + step.g * x;
                    let u = capped_newton(|u| step.residual(q, u), step.seed(q), u);
                    *state = (u, step.state(u, x));
                    *y = u * volts;
                }
            }
            self.into = (self.into + end - start) % CHUNK;
            start = end;
        }
        let frames = u32::try_from(frames).unwrap_or(u32::MAX);
        self.cutoff.advance(frames);
        self.drive.advance(frames);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
