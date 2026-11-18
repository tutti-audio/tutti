//! [`Tremolo`] and [`AutoPan`]: a low-frequency oscillator scales the level, or places the
//! signal between left and right.
//!
//! Both read an [`Oscillator`] of a chosen [`OscillatorWave`] at the controlled rate, its
//! phase starting at 0 and kept across blocks, so the render is the same whatever the blocks.
//! The wave `u` is read within `-1..=1`. The LFO is the band-limited oscillator, so a square or
//! saw edge is about 1 to 2 samples wide at sub-audio rates, and the clamp trims the
//! polynomial overshoot of its edges.
//!
//! A [`Depth`] `d` in `-1..=1` is the amount. Tremolo's gain is
//! `g = 1 - (|d| - d u) / 2`: depth 0 is unity, and depth 1 swings from unity (at the wave's
//! peak) to silence (at its trough); a negative depth swings the other way, `g = 1 - |d| (1 + u) / 2`.
//! Auto-pan's position is `d u` as a [`Pan`], so `d = 1` sweeps the whole way across and
//! a negative depth sweeps the opposite way.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_units::{Depth, Hz, Pan, Real, SampleRate};

use crate::{Oscillator, OscillatorWave, PanLaw, pan_gains};

/// The controls of a [`Tremolo`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TremoloControls {
    /// The LFO's rate. A rate at or below 0 holds the phase and one past the Nyquist runs at
    /// it; one that is not finite holds the phase.
    pub rate: Hz,
    /// How far the gain swings below unity: 0 none, 1 down to silence.
    pub depth: Depth,
}

impl TremoloControls {
    /// 5 Hz at half depth.
    pub const DEFAULT: Self = Self {
        rate: Hz(5.0),
        depth: Depth::of(0.5),
    };
}

/// The controls of an [`AutoPan`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoPanControls {
    /// The LFO's rate, read as [`TremoloControls::rate`] is.
    pub rate: Hz,
    /// How far the position travels either side of the centre: 0 none, 1 hard left to hard
    /// right.
    pub depth: Depth,
}

impl AutoPanControls {
    /// 1 Hz across the whole field.
    pub const DEFAULT: Self = Self {
        rate: Hz(1.0),
        depth: Depth::FULL,
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

    /// The wave at the current phase, then the phase one frame on.
    fn next(&mut self, rate: Hz) -> f32 {
        self.osc.step(rate, self.sample_rate).clamp(-1.0, 1.0)
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
    pub fn aim(&mut self, controls: &TremoloControls) {
        self.controls = *controls;
    }

    /// Back to phase 0.
    pub fn reset(&mut self) {
        self.lfo.osc.reset();
    }

    /// Runs `input` into `output`. The channels rendered are the fewer of the two views',
    /// the frames the fewer of the two views'. No allocation.
    pub fn process<S: Float>(&mut self, input: Planes<'_, S>, mut output: PlanesMut<'_, '_, S>) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input.channels().min(output.channels());
        let TremoloControls { rate, depth } = self.controls;
        // The depth crosses into the oscillator's `f32` lane once per block.
        let d = f32::from_unit(depth);
        let start = self.lfo;
        let mut end = start;
        for c in 0..width {
            let mut lfo = start;
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            let y = &mut plane.samples_mut()[..frames];
            for (y, &x) in y.iter_mut().zip(x) {
                let g = 1.0 - (d.abs() - d * lfo.next(rate)) * 0.5;
                *y = x * S::from_f32(g);
            }
            end = lfo;
        }
        // Every channel walked the same frames from the same phase.
        self.lfo = end;
    }
}

/// An auto-pan of a left/right pair: the left scaled by the left gain of [`pan_gains`] and
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
    pub fn aim(&mut self, controls: &AutoPanControls) {
        self.controls = *controls;
    }

    /// Back to phase 0.
    pub fn reset(&mut self) {
        self.lfo.osc.reset();
    }

    /// Runs the first two channels of `input` into the first two of `output`; nothing is
    /// written when either view has fewer than two. The frames are the fewer of the two
    /// views'. No allocation.
    pub fn process<S: Float>(&mut self, input: Planes<'_, S>, mut output: PlanesMut<'_, '_, S>) {
        if input.channels() < 2 || output.channels() < 2 {
            return;
        }
        let frames = input.frames().get().min(output.frames().get());
        let (l, r) = (input.plane(0), input.plane(1));
        let (l, r) = (&l.samples()[..frames], &r.samples()[..frames]);
        let mut planes = output.iter_mut();
        let (Some(mut left), Some(mut right)) = (planes.next(), planes.next()) else {
            return;
        };
        let (out_l, out_r) = (
            &mut left.samples_mut()[..frames],
            &mut right.samples_mut()[..frames],
        );
        let AutoPanControls { rate, depth } = self.controls;
        // The depth crosses into the oscillator's `f32` lane once per block.
        let depth = f32::from_unit(depth);
        for (((&l, &r), o_l), o_r) in l.iter().zip(r).zip(out_l).zip(out_r) {
            let pan = Pan(depth * self.lfo.next(rate));
            let (gl, gr) = pan_gains(pan, self.law);
            *o_l = l * S::from_f64(gl.get());
            *o_r = r * S::from_f64(gr.get());
        }
    }
}

#[cfg(test)]
mod tests;
