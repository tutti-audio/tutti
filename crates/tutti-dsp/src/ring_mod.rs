//! [`RingModulator`]: the input multiplied by a sine carrier.
//!
//! The carrier is `c[n] = sin(2 pi phi[n])` with the phase `phi` in cycles, starting at 0 and
//! advanced by `f / fs` a frame, so a change of frequency bends the phase's slope and never
//! jumps it. The output is `y = x c`, whose spectrum for a sine input at `f1` holds
//! `f1 + fc` and `f1 - fc` and nothing at `f1`. A dry/wet blend is the graph's, per node.

use crate::BlockMismatch;
use crate::block::expect_same_width;
use tutti_buffers::{Float, InputOutput};
use tutti_units::{Hz, SampleRate};

use crate::{Oscillator, OscillatorStep, OscillatorWave};

/// The controls of a [`RingModulator`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RingModControls {
    /// The carrier's frequency. At or below 0 the phase holds and past the Nyquist it runs
    /// at it; one that is not finite holds the phase.
    pub frequency: Hz,
}

impl RingModControls {
    /// A 440 Hz carrier.
    pub const DEFAULT: Self = Self {
        frequency: Hz::new(440.0),
    };
}

/// A ring modulator of any width: one carrier, in phase across the channels.
///
/// Built on the control side for one rate; nothing it holds grows.
#[derive(Clone, Copy, Debug)]
pub struct RingModulator {
    carrier: Oscillator,
    rate: SampleRate,
    controls: RingModControls,
}

impl RingModulator {
    /// A modulator at `rate` and the default controls, the carrier at phase 0.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        Self {
            carrier: Oscillator::new(OscillatorWave::Sine),
            rate,
            controls: RingModControls::DEFAULT,
        }
    }

    /// Takes `controls`; the carrier's phase is kept. No allocation.
    pub fn retarget(&mut self, controls: &RingModControls) {
        self.controls = *controls;
    }

    /// Back to carrier phase 0.
    pub fn reset(&mut self) {
        self.carrier.reset();
    }

    /// Runs `input` into `output`, every channel on the one carrier. No allocation.
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
        let RingModControls { frequency } = self.controls;
        let step = OscillatorStep::<F>::new(frequency, self.rate);
        let start = self.carrier;
        let mut end = start;
        for c in 0..width {
            let mut carrier = start;
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            let y = &mut plane.samples_mut()[..frames];
            for (y, &x) in y.iter_mut().zip(x) {
                *y = x * carrier.step(step);
            }
            end = carrier;
        }
        self.carrier = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
