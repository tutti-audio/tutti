//! [`Tremolo`] and [`AutoPan`]: a low-frequency oscillator scales the level, or places the
//! signal between left and right.
//!
//! Both read an [`Oscillator`] of a chosen [`OscillatorWave`] at the controlled rate, its
//! phase starting at 0 and kept across blocks, so the render is the same whatever the blocks.
//! The wave `u` is read within `-1..=1`. The LFO is the band-limited oscillator, so a square or
//! saw edge is about 1 to 2 samples wide at sub-audio rates, and the clamp trims the
//! polynomial overshoot of its edges.
//!
//! A [`Bipolar`] `d` in `-1..=1` is the amount. Tremolo's gain is
//! `g = 1 - (|d| - d u) / 2`: depth 0 is unity, and depth 1 swings from unity (at the wave's
//! peak) to silence (at its trough); a negative depth swings the other way,
//! `g = 1 - |d| (1 + u) / 2`. Auto-pan's position is `d u`, a [`Pan`](tutti_units::Pan), so
//! `d = 1` sweeps the whole way across and a negative depth sweeps the opposite way.

use crate::BlockMismatch;
use crate::block::{expect_same_width, expect_widths};
use tutti_buffers::{Float, InputOutput};
use tutti_units::{Bipolar, Hz, SampleRate};

use crate::pan::pan_gains_in;
use crate::{Oscillator, OscillatorStep, OscillatorWave, PanLaw};

/// The controls of a [`Tremolo`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TremoloControls {
    /// The LFO's rate. A rate at or below 0 holds the phase and one past the Nyquist runs at
    /// it; one that is not finite holds the phase.
    pub rate: Hz,
    /// How far the gain swings below unity: 0 none, 1 down to silence.
    pub depth: Bipolar,
}

impl TremoloControls {
    /// 5 Hz at half depth.
    pub const DEFAULT: Self = Self {
        rate: Hz::new(5.0),
        depth: Bipolar::of(0.5),
    };
}

/// The controls of an [`AutoPan`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoPanControls {
    /// The LFO's rate, read as [`TremoloControls::rate`] is.
    pub rate: Hz,
    /// How far the position travels either side of the centre: 0 none, 1 hard left to hard
    /// right.
    pub depth: Bipolar,
}

impl AutoPanControls {
    /// 1 Hz across the whole field.
    pub const DEFAULT: Self = Self {
        rate: Hz::new(1.0),
        depth: Bipolar::MAX,
    };
}

/// The LFO both effects read: an oscillator at a rate, its wave cut to `-1..=1`.
#[derive(Clone, Copy, Debug)]
struct Lfo {
    osc: Oscillator,
    sample_rate: SampleRate,
}

impl Lfo {
    fn new(wave: OscillatorWave, sample_rate: SampleRate) -> Self {
        Self {
            osc: Oscillator::new(wave),
            sample_rate,
        }
    }

    /// The wave at the current phase in the float `F`, then the phase one frame on.
    fn next<F: Float>(&mut self, step: OscillatorStep<F>) -> F {
        self.osc.step(step).clamp(-F::ONE, F::ONE)
    }

    /// The step of `rate` at the stream's rate, crossed once per block.
    fn step<F: Float>(&self, rate: Hz) -> OscillatorStep<F> {
        OscillatorStep::new(rate, self.sample_rate)
    }
}

/// A tremolo of any width: every channel scaled by one gain, `g = 1 - (|d| - d u) / 2` for
/// the depth `d` and the wave `u` at each frame.
///
/// Built on the control side for one rate; nothing it holds grows.
#[derive(Clone, Copy, Debug)]
pub struct Tremolo {
    lfo: Lfo,
    controls: TremoloControls,
}

impl Tremolo {
    /// A tremolo reading `wave` at `rate`, at the default controls, the wave at phase 0.
    #[must_use]
    pub fn new(wave: OscillatorWave, rate: SampleRate) -> Self {
        Self {
            lfo: Lfo::new(wave, rate),
            controls: TremoloControls::DEFAULT,
        }
    }

    /// Takes `controls`; the phase is kept. No allocation.
    pub fn retarget(&mut self, controls: &TremoloControls) {
        self.controls = *controls;
    }

    /// Back to phase 0.
    pub fn reset(&mut self) {
        self.lfo.osc.reset();
    }

    /// Runs `input` into `output`, every channel scaled by the one gain. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the output has other channels than the input.
    pub fn process<F: Float>(
        &mut self,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let width = expect_same_width(block)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let TremoloControls { rate, depth } = self.controls;
        // The depth crosses into the data's float once per block.
        let d = F::from_unit(depth);
        let half = F::lit(0.5);
        let step = self.lfo.step::<F>(rate);
        let start = self.lfo;
        let mut end = start;
        for c in 0..width {
            let mut lfo = start;
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            let y = &mut plane.samples_mut()[..frames];
            for (y, &x) in y.iter_mut().zip(x) {
                let g = F::ONE - (d.abs() - d * lfo.next(step)) * half;
                *y = x * g;
            }
            end = lfo;
        }
        // Every channel walked the same frames from the same phase.
        self.lfo = end;
        Ok(())
    }
}

/// An auto-pan of a left/right pair: the left scaled by the left gain of
/// [`pan_gains`](crate::pan_gains) and
/// the right by its right gain, at the position `d u` of the depth `d` and the wave `u`.
///
/// Under [`PanLaw::SquareRoot`] and [`PanLaw::Rp036`] the two gains' squares sum to 1 at every
/// position, so a source of one channel keeps its power across the sweep; at the centre each
/// channel is the law's -3 dB.
#[derive(Clone, Copy, Debug)]
pub struct AutoPan {
    lfo: Lfo,
    law: PanLaw,
    controls: AutoPanControls,
}

impl AutoPan {
    /// An auto-pan reading `wave` at `rate`, placing by `law`, at the default controls, the
    /// wave at phase 0.
    #[must_use]
    pub fn new(wave: OscillatorWave, law: PanLaw, rate: SampleRate) -> Self {
        Self {
            lfo: Lfo::new(wave, rate),
            law,
            controls: AutoPanControls::DEFAULT,
        }
    }

    /// Takes `controls`; the phase is kept. No allocation.
    pub fn retarget(&mut self, controls: &AutoPanControls) {
        self.controls = *controls;
    }

    /// Back to phase 0.
    pub fn reset(&mut self) {
        self.lfo.osc.reset();
    }

    /// Runs the two channels of `input` into the two of `output`. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the input or the output is not two channels.
    pub fn process<F: Float>(
        &mut self,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        expect_widths(block, 2, 2)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let (l, r) = (input.plane(0), input.plane(1));
        let (l, r) = (&l.samples()[..frames], &r.samples()[..frames]);
        let mut planes = output.iter_mut();
        let (Some(mut left), Some(mut right)) = (planes.next(), planes.next()) else {
            // Not reached: the width check above left two output channels.
            return Ok(());
        };
        let (out_l, out_r) = (
            &mut left.samples_mut()[..frames],
            &mut right.samples_mut()[..frames],
        );
        let AutoPanControls { rate, depth } = self.controls;
        // The depth crosses into the data's float once per block.
        let depth = F::from_unit(depth);
        let step = self.lfo.step::<F>(rate);
        for (((&l, &r), o_l), o_r) in l.iter().zip(r).zip(out_l).zip(out_r) {
            let (gl, gr) = pan_gains_in(depth * self.lfo.next(step), self.law);
            *o_l = l * gl;
            *o_r = r * gr;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
