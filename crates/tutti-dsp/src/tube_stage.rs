//! [`TubeStage`]: a triode common-cathode gain stage, solved per sample by a capped Newton
//! iteration from a closed-form seed.
//!
//! # The circuit
//!
//! A triode with its grid driven, a plate load `R_p` to a supply `B+`, and a coupling
//! capacitor at the output. The plate current follows Koren's model (Koren 1996, "Improved
//! vacuum tube models for SPICE simulations"), with `V_gk` the grid-to-cathode and `V_pk` the
//! plate-to-cathode voltage:
//!
//! ```text
//! E1 = (V_pk / Kp) ln(1 + exp(Kp (1 / mu + V_gk / sqrt(KVB + V_pk^2))))
//! I_p = (E1^Ex / KG1) (1 + sgn E1)
//! ```
//!
//! The parameters are the 12AX7 set of Koren's paper (`mu` 100, `Ex` 1.4, `KG1` 1060, `Kp` 600,
//! `KVB` 300); the supply 250 V, the plate load 100 kOhm and the cathode resistor 1.5 kOhm are
//! the usual values of a 12AX7 gain stage and named constants here (`tutti_units` has no volt
//! or ohm unit, and the user sets a drive in decibels, not a circuit). The plate voltage is the
//! root of the load line
//!
//! ```text
//! f(V_pk) = V_pk - B+ + R_p I_p(V_pk, V_gk) = 0
//! ```
//!
//! # The stage
//!
//! The cathode resistor is bypassed by a capacitor large enough that it carries the DC current
//! alone: the cathode sits at the rest voltage `V_k = I_p0 R_k` and the grid sees
//! `V_gk = -V_k + g x`, with `x` the input and `g` the drive's gain in volts per unit of
//! signal. The plate is memoryless in `V_gk` (the tube's capacitances are far above the audio
//! band), and a coupling capacitor of corner [`DC_CORNER`] takes it to the output: a
//! [`DcBlocker`](crate::DcBlocker)'s first-order high-pass. The rest point `(V_gk, V_pk)` is
//! solved when the stage is built, by bisection on the self-bias condition `I_p R_k = -V_gk`
//! and on the load line.
//!
//! **Normalization.** The output is `(V_p0 - V_pk) / (|A| 1 V)`, with `A = -g_m (r_p || R_p)`
//! the small-signal gain at the rest point: a small signal at drive 0 dB has gain 1 (the
//! stage's inversion is undone), `A = -R_p I_g / (1 + R_p I_v)` for `I_g = dI_p / dV_gk` and
//! `I_v = dI_p / dV_pk`.
//!
//! # The solve
//!
//! `I_p` rises with `V_pk` (`dI_p / dV_pk >= 0`; the module's tests sweep it), so `f` is
//! increasing with slope `f' = 1 + R_p I_v >= 1` and has one root, in `0 < V_pk <= B+`, and a
//! Newton iterate's distance from it is at most `|f|`. Newton from `V_pk = 0` or below, where the
//! tube is cut off and `f' = 1`, overshoots to `B+` and oscillates, so the start is the
//! closed-form **seed** of the saturated tube: positive where the tube conducts, and `B+` where it
//! does not.
//!
//! With `z0 = Kp (1 / mu + V_gk / sqrt KVB)` (the exponent at `V_pk` small against `sqrt KVB`),
//! `a = softplus(z0) / Kp` and `c = R_p (2 / KG1) a^Ex`, the load line reads
//! `V + c V^Ex = B+`. Its first approximation is `V = (B+ / c)^(1 / Ex)` (the term `c V^Ex`
//! carries the load), refined by two steps of `V <- ((B+ - V) / c)^(1 / Ex)`, a contraction of
//! ratio `V / (Ex (B+ - V))`. A seed past `B+` (the tube cut off) is `B+`.
//!
//! The solver then takes [`TUBE_NEWTON_CAP`] steps from the better of the seed and the
//! previous sample's solution. The cap is 4. The error is measured by the module's tests,
//! sweeping `V_gk` from -35 V to +35 V in 0.05 V steps: from the seed alone it is at worst
//! `0.5` V after two steps, `4e-4` V after three and `2.8e-10` V after four. A warm start the
//! solver prefers to the seed (the smaller `|f|`) is the plate of the previous sample: within a
//! sample of a 1 kHz signal at the highest drive it leaves under `1e-11` V, and after the worst
//! jump of the grid (from +35 V to -0.2 V in one sample, the plate from 0.6 V to 80 V) `3.5e-4` V.
//! The error in the solution bounds the residual `f` by `f'` times it, and `f' >= 1` bounds the
//! error by the residual.
//!
//! The grid voltage is not bounded by a grid current (the tube's grid diode is not
//! modelled): a drive's peak reaches `V_gk` of `+/- 32` V, over which the Koren expression
//! stays smooth, and the plate current at the saturated seed's `V_pk` is the model's.

use crate::BlockMismatch;
use crate::block::expect_widths;
use crate::dc_block::{DC_CORNER, dc_complement};
use crate::newton::capped_newton_steps;
use crate::{GAIN_RAMP, Lerp, Ramp, Steps};
use tutti_buffers::{ChannelLayout, Float, InputOutput};
use tutti_units::{Db, SampleRate, Samples};

/// The frames a coefficient set (the drive's gain) holds for: a control grid of the
/// oversampled stream, counted from the last reset, so the render does not depend on where
/// blocks fall.
const CHUNK: usize = 16;

/// Koren's amplification factor `mu` of the 12AX7.
const MU: f64 = 100.0;
/// Koren's exponent `Ex`.
const EX: f64 = 1.4;
/// Koren's current coefficient `KG1`.
const KG1: f64 = 1060.0;
/// Koren's knee `Kp`.
const KP: f64 = 600.0;
/// Koren's `KVB`, the plate voltage's contribution to the knee, volts squared.
const KVB: f64 = 300.0;
/// The supply `B+`, volts.
const SUPPLY: f64 = 250.0;
/// The plate load `R_p`, ohms.
const PLATE_LOAD: f64 = 100_000.0;
/// The cathode resistor `R_k`, ohms.
const CATHODE_RESISTOR: f64 = 1_500.0;
/// The volts one unit of signal is at 0 dB drive.
const FULL_SCALE: f64 = 1.0;

/// The Newton steps of every solve, 4: the iteration from the seed leaves `2.8e-10` V at this
/// cap, against `4e-4` V at three (the module's measured sweep).
pub(crate) const TUBE_NEWTON_CAP: usize = 4;

tutti_units::ranged! {
    /// The gain into a [`TubeStage`], in decibels over 1 V per unit of signal: from
    /// [`MIN`](Self::MIN) (-60 dB) to [`MAX`](Self::MAX) (+30 dB), the range the solver's
    /// residual is stated over (a grid swing of 31.6 V).
    TubeDrive(Db), TubeDriveError, "a drive from -60 dB to +30 dB",
    |raw| raw.get() >= -60.0 && raw.get() <= 30.0
}

impl TubeDrive {
    /// The lowest drive: -60 dB.
    pub const MIN: Self = Self::of(Db::new(-60.0));
    /// The highest drive: +30 dB, a grid swing of 31.6 V.
    pub const MAX: Self = Self::of(Db::new(30.0));
    /// +6 dB, the [`TubeStageControls::DEFAULT`] drive: a 2 V grid swing against the rest
    /// bias of about -1.2 V.
    pub const DEFAULT: Self = Self::of(Db::new(6.0));

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

/// The controls of a [`TubeStage`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TubeStageControls {
    /// The gain into the tube; in range by its type.
    pub drive: TubeDrive,
}

impl TubeStageControls {
    /// The drive [`TubeDrive::DEFAULT`].
    pub const DEFAULT: Self = Self {
        drive: TubeDrive::DEFAULT,
    };

    /// The frames the coupling capacitor rings after the input stops, to
    /// [`TAIL_FLOOR`](crate::TAIL_FLOOR), at a stage running at `rate`: the plate is
    /// memoryless, so only the high-pass of corner [`DC_CORNER`] carries state.
    #[must_use]
    pub fn tail(&self, rate: SampleRate) -> Samples {
        crate::DcBlockerControls::DEFAULT.tail(rate)
    }
}

/// The 12AX7 stage's constants in the float `F`: the plate current of Koren's model and the
/// load line it sits on.
#[derive(Clone, Copy, Debug)]
struct Triode<F: Float> {
    inv_mu: F,
    ex: F,
    inv_ex: F,
    /// `2 / KG1`: the current scale with the `1 + sgn` factor of a conducting tube.
    scale: F,
    kp: F,
    inv_kp: F,
    kvb: F,
    sqrt_kvb: F,
    supply: F,
    load: F,
}

impl<F: Float> Triode<F> {
    fn new() -> Self {
        Self {
            inv_mu: F::from_design(1.0 / MU),
            ex: F::from_design(EX),
            inv_ex: F::from_design(1.0 / EX),
            scale: F::from_design(2.0 / KG1),
            kp: F::from_design(KP),
            inv_kp: F::from_design(1.0 / KP),
            kvb: F::from_design(KVB),
            sqrt_kvb: F::from_design(KVB.sqrt()),
            supply: F::from_design(SUPPLY),
            load: F::from_design(PLATE_LOAD),
        }
    }

    /// `softplus(z) = ln(1 + e^z)` and its slope, the logistic function, from one `exp(-|z|)`
    /// (at most 1, so neither form overflows).
    #[inline]
    fn softplus(z: F) -> (F, F) {
        let e = (-z.abs()).exp();
        let up = z >= F::ZERO;
        let sp = (if up { z } else { F::ZERO }) + e.ln_1p();
        let logistic = (if up { F::ONE } else { e }) / (F::ONE + e);
        (sp, logistic)
    }

    /// The plate current `I_p` in amperes and `dI_p / dV_pk`, at plate voltage `v` and grid
    /// voltage `vg`. `E1 > 0` exactly when `v > 0` (the logarithm is positive), and the
    /// `1 + sgn E1` factor is 2 there and 0 for `v <= 0`.
    #[inline]
    fn current(&self, v: F, vg: F) -> (F, F) {
        let s2 = self.kvb + v * v;
        let s = s2.sqrt();
        let (sp, logistic) = Self::softplus(self.kp * (self.inv_mu + vg / s));
        let e1 = v * self.inv_kp * sp;
        if e1 > F::ZERO {
            let de1 = sp * self.inv_kp - logistic * vg * v * v / (s2 * s);
            let root = e1.powf(self.ex - F::ONE);
            (self.scale * root * e1, self.scale * self.ex * root * de1)
        } else {
            (F::ZERO, F::ZERO)
        }
    }

    /// The load line `f(v) = v - B+ + R_p I_p` and its slope, at grid voltage `vg`.
    #[inline]
    fn residual(&self, vg: F, v: F) -> (F, F) {
        let (i, di) = self.current(v, vg);
        (v - self.supply + self.load * i, F::ONE + self.load * di)
    }

    /// `dI_p / dV_gk` at `(v, vg)` (`v > 0`).
    fn transconductance(&self, v: F, vg: F) -> F {
        let s = (self.kvb + v * v).sqrt();
        let (sp, logistic) = Self::softplus(self.kp * (self.inv_mu + vg / s));
        let e1 = v * self.inv_kp * sp;
        self.scale * self.ex * e1.powf(self.ex - F::ONE) * v * logistic / s
    }

    /// The saturated-tube seed for the grid voltage `vg`: module docs, "The solve". At most
    /// `B+`.
    #[inline]
    fn seed(&self, vg: F) -> F {
        let (sp, _) = Self::softplus(self.kp * (self.inv_mu + vg / self.sqrt_kvb));
        let c = self.load * self.scale * (sp * self.inv_kp).powf(self.ex);
        let first = (self.supply / c).powf(self.inv_ex);
        if first >= self.supply || first.is_nan() {
            self.supply
        } else {
            let mut v = first;
            for _ in 0..2 {
                v = ((self.supply - v) / c).powf(self.inv_ex);
            }
            v.min(self.supply)
        }
    }

    /// The plate voltage at grid voltage `vg`, from the seed and the previous solution `warm`
    /// by [`TUBE_NEWTON_CAP`] Newton steps.
    #[inline]
    fn solve(&self, vg: F, warm: F) -> F {
        capped_newton_steps::<F, TUBE_NEWTON_CAP>(|v| self.residual(vg, v), self.seed(vg), warm)
    }
}

/// The rest point and the gain normalization of the stage, solved in `f64` when it is built.
#[derive(Clone, Copy, Debug)]
struct Operating {
    /// `V_gk` at rest, volts: `-I_p0 R_k`.
    grid: f64,
    /// `V_pk` at rest, volts.
    plate: f64,
    /// `1 / (|A| FULL_SCALE)`: the output per plate volt.
    per_volt: f64,
}

impl Operating {
    /// The plate voltage at grid voltage `vg` by bisection of the load line over `0..=B+`, to
    /// the width of `f64`.
    fn load_line(triode: &Triode<f64>, vg: f64) -> f64 {
        let (mut lo, mut hi) = (0.0, SUPPLY);
        for _ in 0..64 {
            let mid = 0.5 * (lo + hi);
            if triode.residual(vg, mid).0 > 0.0 {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        0.5 * (lo + hi)
    }

    fn solve() -> Self {
        let triode = Triode::<f64>::new();
        // `I_p R_k - V_k` falls as the bias `V_k` rises: bisect its zero over 0..=5 V.
        let (mut lo, mut hi) = (0.0, 5.0);
        for _ in 0..64 {
            let vk = 0.5 * (lo + hi);
            let plate = Self::load_line(&triode, -vk);
            if triode.current(plate, -vk).0 * CATHODE_RESISTOR > vk {
                lo = vk;
            } else {
                hi = vk;
            }
        }
        let grid = -0.5 * (lo + hi);
        let plate = Self::load_line(&triode, grid);
        let (_, di) = triode.current(plate, grid);
        let gain = triode.load * triode.transconductance(plate, grid) / (1.0 + triode.load * di);
        Self {
            grid,
            plate,
            per_volt: 1.0 / (gain * FULL_SCALE),
        }
    }
}

/// A tube stage of any width in the float `F`: every channel a 12AX7 common-cathode stage
/// with a plate load and a coupling capacitor, at one drive. At most
/// [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) channels wide.
///
/// It runs at the rate it is built for, which is a multiple of the signal's: the
/// nonlinearity creates harmonics that an oversampled node ([`Oversampler`](crate::Oversampler))
/// filters out. Built on the control side; nothing it holds grows.
#[derive(Clone, Debug)]
pub struct TubeStage<F: Float> {
    triode: Triode<F>,
    /// The rest grid voltage and plate voltage.
    rest: (F, F),
    /// The output per plate volt.
    per_volt: F,
    /// The coupling capacitor's `1 - R`, the complement of the high-pass pole.
    complement: F,
    /// The drive's amplitude, ramping over [`GAIN_RAMP`].
    drive: Ramp<F>,
    steps: u32,
    /// The grid volts per unit of signal, held for the frames of the current chunk.
    gain: F,
    /// The frames into the current chunk, `0..CHUNK`.
    into: usize,
    /// Each channel's last plate voltage and the high-pass's `(x[n-1], y[n-1])`.
    state: Vec<(F, F, F)>,
}

impl<F: Float + Lerp> TubeStage<F> {
    /// A stage of `width` channels running at `rate`, at [`TubeStageControls::DEFAULT`].
    #[must_use]
    pub fn new(width: ChannelLayout, rate: SampleRate) -> Self {
        let steps = u32::try_from(GAIN_RAMP.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        let operating = Operating::solve();
        let rest = (
            F::from_design(operating.grid),
            F::from_design(operating.plate),
        );
        let drive = F::from_design(TubeStageControls::DEFAULT.drive.get().to_amplitude().get());
        let mut stage = Self {
            triode: Triode::new(),
            rest,
            per_volt: F::from_design(operating.per_volt),
            complement: F::from_design(dc_complement(DC_CORNER, rate)),
            drive: Ramp::new(drive),
            steps,
            gain: F::ONE,
            into: 0,
            state: vec![(rest.1, F::ZERO, F::ZERO); usize::from(width.count())],
        };
        stage.gain = stage.design(0);
        stage
    }

    /// Aims at `controls`: the drive ramps over [`GAIN_RAMP`]. No allocation.
    pub fn retarget(&mut self, controls: &TubeStageControls) {
        let drive = F::from_design(controls.drive.get().to_amplitude().get());
        self.drive.retarget_by(drive, Steps::Fixed(self.steps));
    }

    /// Lands the drive on its target now.
    pub fn jump_to_target(&mut self) {
        self.drive.jump_to_target();
        self.gain = self.design(0);
        self.into = 0;
    }

    /// Back to rest: the tube at its rest point, the coupling capacitor discharged and the
    /// drive on its target.
    pub fn reset(&mut self) {
        self.state.fill((self.rest.1, F::ZERO, F::ZERO));
        self.jump_to_target();
    }

    /// The grid volts per unit of signal at ramp step `k`.
    fn design(&self, k: u32) -> F {
        self.drive.at(k) * F::from_design(FULL_SCALE)
    }

    /// Runs `input` into `output`, every channel through the stage; at rest the output is 0 and
    /// a small signal at drive 0 dB has gain 1 (module docs, "Normalization"). No allocation.
    ///
    /// The drive is designed every 16 frames from the first frame after a reset, so the render
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
        let (grid, plate) = self.rest;
        let mut start = 0;
        while start < frames {
            if self.into == 0 {
                let k = first.saturating_add(u32::try_from(start).unwrap_or(u32::MAX));
                self.gain = self.design(k);
            }
            let end = (start + CHUNK - self.into).min(frames);
            let (gain, triode, per_volt, complement) =
                (self.gain, self.triode, self.per_volt, self.complement);
            for (c, state) in self.state.iter_mut().enumerate() {
                let x = &input.plane(c).samples()[start..end];
                let mut plane = output.plane_mut(c);
                let y = &mut plane.samples_mut()[start..end];
                for (y, &x) in y.iter_mut().zip(x) {
                    let (warm, x1, y1) = *state;
                    let v = triode.solve(grid + gain * x, warm);
                    let shaped = (plate - v) * per_volt;
                    let out = y1 + ((shaped - x1) - complement * y1);
                    *state = (v, shaped, out);
                    *y = out;
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
