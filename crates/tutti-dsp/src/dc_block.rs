//! [`DcBlocker`]: a first-order high-pass that removes a constant offset.
//!
//! `y[n] = x[n] - x[n-1] + R y[n-1]`, a zero at DC and a pole at `R` just inside the unit
//! circle. A new pole is reached along a line over [`COEFF_INTERVAL`] frames (the grid a
//! node designs on), so the output moves by at most the input's own step plus
//! `(1 - R) |y|` per frame whatever the pole did before.
//!
//! The pole is set by the corner as `R = exp(-2 pi fc / fs)` ([`dc_pole`]): a constant
//! input decays as `R^n`, and the gain at `omega = 2 pi f / fs` is
//! `|1 - e^(-j omega)| / |1 - R e^(-j omega)|`, which is `1/sqrt(2)` close to the corner.

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, InputOutput};
use tutti_units::{Hz, SampleRate, Samples};

use crate::onepole::{cutoff_pole, ring_frames};
use crate::{COEFF_INTERVAL, Lerp, Ramp, Steps};

/// The default corner: 10 Hz, well under the audible band's low end and a few time
/// constants (`1 / 2 pi fc`, 16 ms) from settling a step.
pub const DC_CORNER: Hz = Hz::new(10.0);

/// The lowest corner a blocker takes: 1 Hz. A lower one reads as this, which bounds the
/// ring-out ([`DcBlockerControls::tail`]) at about a second.
pub const MIN_DC_CORNER: Hz = Hz::new(1.0);

/// The pole `R = exp(-2 pi fc / fs)` of a blocker with `corner` at `rate`: the corner read
/// as at least [`MIN_DC_CORNER`] and at most the rate's Nyquist, and a corner that is not
/// finite as [`DC_CORNER`].
#[must_use]
pub(crate) fn dc_pole(corner: Hz, rate: SampleRate) -> f64 {
    cutoff_pole(held_corner(corner, rate), rate)
}

/// The corner as a blocker reads it: finite, within [`MIN_DC_CORNER`] and the Nyquist.
fn held_corner(corner: Hz, rate: SampleRate) -> Hz {
    corner
        .finite_or(DC_CORNER)
        .clamp(MIN_DC_CORNER, rate.nyquist())
}

/// The control of a [`DcBlocker`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DcBlockerControls {
    /// The corner frequency, read within [`MIN_DC_CORNER`] and the Nyquist.
    pub corner: Hz,
}

impl DcBlockerControls {
    /// The [`DC_CORNER`].
    pub const DEFAULT: Self = Self { corner: DC_CORNER };

    /// The frames, from the end of an input, until the output is under
    /// [`TAIL_FLOOR`](crate::TAIL_FLOOR) of the input's peak, for a blocker with these controls,
    /// erring long: past its first tap the impulse response is `-(1 - R) R^(k-1)`, all one
    /// sign, so after a stop `|y| <= A R^(n-1)` for an input of peak `A`; the frames are those
    /// of `R^n` under the floor's amplitude, plus one.
    #[must_use]
    pub fn tail(&self, rate: SampleRate) -> Samples {
        let r = dc_pole(self.corner, rate);
        Samples::new((ring_frames(r, crate::TAIL_FLOOR).ceil() as usize).saturating_add(1))
    }
}

/// The complement `1 - R` of the pole of a blocker with `corner` at `rate`: the small term the
/// recursion runs on, solved in `f64`.
fn dc_complement(corner: Hz, rate: SampleRate) -> f64 {
    -libm::expm1(-held_corner(corner, rate).angle_per_frame(rate).get())
}

/// A DC blocker of any width in the float `F`: one `(x[n-1], y[n-1])` pair per channel, one
/// pole for all.
///
/// The pole `R` is within `1 - R` of 1 at a low corner, so the blocker holds `c = 1 - R`, designed
/// in `f64` and narrowed to `F` once per `retarget`, and runs `y = y1 + ((x - x1) - c y1)`. Built
/// on the control side for one rate; nothing it holds grows.
#[derive(Clone, Debug)]
pub struct DcBlocker<F: Float> {
    rate: SampleRate,
    /// The complement `1 - R` of the pole, ramping over [`COEFF_INTERVAL`] frames.
    complement: Ramp<F>,
    /// `(x[n-1], y[n-1])` of each channel: per-sample values in `F`.
    state: Vec<(F, F)>,
}

impl<F: Float + Lerp> DcBlocker<F> {
    /// A blocker of `width` channels at `rate`, at the [`DC_CORNER`], silent.
    #[must_use]
    pub fn new(width: ChannelLayout, rate: SampleRate) -> Self {
        Self {
            rate,
            complement: Ramp::new(F::from_design(dc_complement(DC_CORNER, rate))),
            state: vec![(F::ZERO, F::ZERO); usize::from(width.count())],
        }
    }

    /// Aims at the pole for `controls`: the pole moves in a line from the one in force over
    /// the next [`COEFF_INTERVAL`] frames. The state is kept. No allocation.
    pub fn retarget(&mut self, controls: &DcBlockerControls) {
        let to = F::from_design(dc_complement(controls.corner, self.rate));
        self.complement
            .retarget_by(to, Steps::Fixed(COEFF_INTERVAL as u32));
    }

    /// Jumps to the target pole now.
    pub fn jump_to_target(&mut self) {
        self.complement.jump_to_target();
    }

    /// The pole in force, `1 - c` formed in `f64`: the set one once the ramp has run.
    #[must_use]
    pub fn pole(&self) -> F {
        let c: f64 = self.complement.now().into();
        F::from_design(1.0 - c)
    }

    /// Back to silence, the pole at rest.
    pub fn reset(&mut self) {
        self.state.fill((F::ZERO, F::ZERO));
        self.jump_to_target();
    }

    /// Runs `input` into `output`, channel `c` through its own recursion. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process(&mut self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        let width = self.state.len();
        expect_widths(block, width, width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let first = self.complement.steps_taken().saturating_add(1);
        for (ch, state) in self.state.iter_mut().enumerate().take(width) {
            let x = &input.plane(ch).samples()[..frames];
            let mut plane = output.plane_mut(ch);
            let y = &mut plane.samples_mut()[..frames];
            let (mut x1, mut y1) = *state;
            for (i, (y, &x)) in y.iter_mut().zip(x).enumerate() {
                let comp = self.complement.at(first.saturating_add(i as u32));
                let out = y1 + ((x - x1) - comp * y1);
                (x1, y1) = (x, out);
                *y = out;
            }
            *state = (x1, y1);
        }
        self.complement
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
