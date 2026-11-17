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

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::{ChannelLayout, Hz, SampleRate, Samples};

use crate::COEFF_INTERVAL;

/// The default corner: 10 Hz, well under the audible band's low end and a few time
/// constants (`1 / 2 pi fc`, 16 ms) from settling a step.
pub const DC_CORNER: Hz = Hz(10.0);

/// The lowest corner a blocker takes: 1 Hz. A lower one reads as this, which bounds the
/// ring-out ([`DcBlocker::tail`]) at about a second.
pub const MIN_DC_CORNER: Hz = Hz(1.0);

/// The pole `R = exp(-2 pi fc / fs)` of a blocker with `corner` at `rate`: the corner read
/// as at least [`MIN_DC_CORNER`] and at most the rate's Nyquist, and a corner that is not
/// finite as [`DC_CORNER`].
#[must_use]
pub fn dc_pole(corner: Hz, rate: SampleRate) -> f64 {
    let corner = if corner.get().is_finite() {
        corner.get()
    } else {
        DC_CORNER.get()
    };
    let corner = f64::from(corner).clamp(f64::from(MIN_DC_CORNER.get()), rate.get() / 2.0);
    (-core::f64::consts::TAU * corner / rate.get()).exp()
}

/// The pole `taken` frames along the line from `from` to `to`.
fn glide(from: f64, to: f64, taken: usize) -> f64 {
    if taken >= COEFF_INTERVAL {
        to
    } else {
        from + (to - from) * taken as f64 / COEFF_INTERVAL as f64
    }
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
}

/// A DC blocker of any width: one `(x[n-1], y[n-1])` pair per channel, one pole for all.
///
/// The state is `f64` whatever the sample format. Built on the control side for one rate;
/// nothing it holds grows.
#[derive(Clone, Debug)]
pub struct DcBlocker {
    rate: SampleRate,
    /// The pole the glide left from, the one it arrives at, and the steps taken of it.
    from: f64,
    to: f64,
    taken: usize,
    /// `(x[n-1], y[n-1])` of each channel.
    state: Vec<(f64, f64)>,
}

impl DcBlocker {
    /// A blocker of `width` channels at `rate`, at the [`DC_CORNER`], silent.
    #[must_use]
    pub fn new(width: ChannelLayout, rate: SampleRate) -> Self {
        Self {
            rate,
            from: dc_pole(DC_CORNER, rate),
            to: dc_pole(DC_CORNER, rate),
            taken: COEFF_INTERVAL,
            state: vec![(0.0, 0.0); usize::from(width.count())],
        }
    }

    /// Aims at the pole for `controls`: the pole moves in a line from the one in force over
    /// the next [`COEFF_INTERVAL`] frames. The state is kept. No allocation.
    pub fn aim(&mut self, controls: &DcBlockerControls) {
        let to = dc_pole(controls.corner, self.rate);
        if to != self.to {
            self.from = self.pole();
            self.to = to;
            self.taken = 0;
        }
    }

    /// Lands on the aimed pole now.
    pub fn settle(&mut self) {
        self.from = self.to;
        self.taken = COEFF_INTERVAL;
    }

    /// The pole in force: the aimed one once the glide has run.
    #[must_use]
    pub fn pole(&self) -> f64 {
        glide(self.from, self.to, self.taken)
    }

    /// Back to silence, the pole landed.
    pub fn reset(&mut self) {
        self.state.fill((0.0, 0.0));
        self.settle();
    }

    /// The frames, from the end of an input, until the output is under
    /// [`TAIL_FLOOR`](crate::TAIL_FLOOR) of the input's peak, for a blocker with `controls`,
    /// erring long: past its first tap the impulse response is `-(1 - R) R^(k-1)`, all one
    /// sign, so after a stop `|y| <= A R^(n-1)` for an input of peak `A`; the frames are those
    /// of `R^n` under the floor's amplitude, plus one.
    #[must_use]
    pub fn tail(controls: &DcBlockerControls, rate: SampleRate) -> Samples {
        let r = dc_pole(controls.corner, rate);
        let floor = 10f64.powf(f64::from(crate::TAIL_FLOOR.get()) / 20.0);
        let frames = (floor.ln() / r.ln()).ceil() + 1.0;
        Samples(frames as usize)
    }

    /// Runs `input` into `output`, channel `c` through its own recursion. The channels
    /// rendered are the fewest of the two views' and the blocker's, the frames the fewer of
    /// the two views'. No allocation.
    pub fn process<S: Float>(&mut self, input: Planes<'_, S>, mut output: PlanesMut<'_, '_, S>) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input
            .channels()
            .min(output.channels())
            .min(self.state.len());
        let (from, to, start) = (self.from, self.to, self.taken);
        for (c, state) in self.state.iter_mut().enumerate().take(width) {
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            let y = &mut plane.samples_mut()[..frames];
            let (mut x1, mut y1) = *state;
            for (i, (y, &x)) in y.iter_mut().zip(x).enumerate() {
                let r = glide(from, to, start + i + 1);
                let x = x.to_f64();
                let out = x - x1 + r * y1;
                (x1, y1) = (x, out);
                *y = S::from_f64(out);
            }
            *state = (x1, y1);
        }
        self.taken = (start + frames).min(COEFF_INTERVAL);
    }
}

#[cfg(test)]
mod tests;
