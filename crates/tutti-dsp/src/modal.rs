//! [`ModalBank`]: a bank of damped two-pole resonators, one per mode of a struck body.
//!
//! Mode `k` of frequency `f`, decay (the time to fall 60 dB) `T60` and gain `g` is the
//! impulse-invariant resonator whose impulse response is the sampled damped sinusoid
//! `h[n] = g r^n sin(w n)`, `w = 2 pi f / fs` and `r = exp(-ln(1000) / (T60 fs))`:
//!
//! ```text
//! H(z) = g r sin(w) z^-1 / (1 - 2 r cos(w) z^-1 + r^2 z^-2)
//! y[n] = 2 r cos(w) y[n-1] - r^2 y[n-2] + g r sin(w) x[n-1]
//! ```
//!
//! so a unit impulse in rings the bank with `sum over k of g_k e^(-t ln(1000) / T60_k)
//! sin(2 pi f_k t)` at `t = n / fs`, and the output has no direct term. The input is the
//! excitation, and the output is the sum of the modes. A mode at or above the Nyquist
//! frequency is silent.
//!
//! The live controls move the whole bank: [`ModalControls`] names the first mode's frequency
//! and decay now, and every mode keeps its configured ratio to the first's.

use core::num::NonZeroU16;
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_units::{Amplitude, Db, Gain, Hz, SampleRate, Samples, Seconds, Tail};

use crate::TAIL_FLOOR;
use crate::onepole::ring_frames;

/// The most modes a bank holds.
pub const MAX_MODES: usize = 64;

tutti_units::ranged! {
    /// How many modes a [`ModalBank`] holds: `1..=`[`MAX_MODES`].
    ModeCount(usize), ModeCountError, "a mode count from 1 to 64",
    |raw| raw >= 1 && raw <= MAX_MODES
}

/// One mode of a body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mode {
    /// The frequency it rings at.
    pub frequency: Hz,
    /// The time its amplitude takes to fall 60 dB. One that is not above zero is a mode that
    /// does not ring.
    pub decay: Seconds,
    /// The amplitude of its impulse response's sinusoid.
    pub gain: Gain,
}

/// The modes of a [`ModalBank`]: `1..=`[`MAX_MODES`] of them, the first the one the live controls
/// name.
#[derive(Clone, Debug, PartialEq)]
pub struct Modes(Vec<Mode>);

impl Modes {
    /// The modes, in order.
    #[must_use]
    pub fn as_slice(&self) -> &[Mode] {
        &self.0
    }

    /// The first mode, which the controls name.
    fn first(&self) -> Mode {
        self.0[0]
    }
}

impl TryFrom<&[Mode]> for Modes {
    type Error = ModeCountError;

    /// The modes of `modes`; allocates.
    ///
    /// # Errors
    ///
    /// [`ModeCountError`] for no modes, or more than [`MAX_MODES`].
    fn try_from(modes: &[Mode]) -> Result<Self, ModeCountError> {
        ModeCount::try_from(modes.len())?;
        Ok(Self(modes.to_vec()))
    }
}

/// The controls of a [`ModalBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModalControls {
    /// The first mode's frequency now; the other modes keep their ratio to it. One that is
    /// not finite and above zero leaves the bank as it is.
    pub frequency: Hz,
    /// The first mode's decay now; the other modes keep their ratio to it. One that is not
    /// finite and above zero leaves the bank as it is.
    pub decay: Seconds,
}

impl ModalControls {
    /// The first mode of `modes`.
    #[must_use]
    pub fn of_first(modes: &Modes) -> Self {
        let first = modes.first();
        Self {
            frequency: first.frequency,
            decay: first.decay,
        }
    }
}

/// One mode's coefficients in the data's float.
#[derive(Clone, Copy, Debug)]
struct Resonator<F> {
    /// `2 r cos(w)`.
    a1: F,
    /// `-r^2`.
    a2: F,
    /// `g r sin(w)`.
    b: F,
}

/// The state of one mode on one channel.
#[derive(Clone, Copy, Debug)]
struct Memory<F> {
    y1: F,
    y2: F,
}

/// A bank of modes over the float `F`, the same on every channel.
///
/// Built on the control side, where it allocates; nothing after that does.
#[derive(Clone, Debug)]
pub struct ModalBank<F: Float> {
    rate: SampleRate,
    modes: Modes,
    resonators: Vec<Resonator<F>>,
    /// Per channel, `modes.len()` memories then the channel's last input.
    memory: Vec<Memory<F>>,
    last_input: Vec<F>,
}

/// The decay `r` per frame of a mode of `t60` at `rate`: the time constant is `T60 / ln 1000`.
fn per_frame(t60: Seconds, rate: SampleRate) -> f64 {
    // 60 dB is an amplitude of 1/1000: `-ln(1/1000)` the time constants.
    let span = -libm::log(Db::new(-60.0).to_amplitude().get());
    Seconds::new(t60.get() / span).one_pole_leave(rate)
}

impl<F: Float> ModalBank<F> {
    /// A silent bank of `modes` on `width` channels at `rate`. Allocates.
    #[must_use]
    pub fn new(modes: &Modes, width: NonZeroU16, rate: SampleRate) -> Self {
        let count = modes.as_slice().len();
        let width = usize::from(width.get());
        let mut bank = Self {
            rate,
            modes: modes.clone(),
            resonators: vec![
                Resonator {
                    a1: F::ZERO,
                    a2: F::ZERO,
                    b: F::ZERO
                };
                count
            ],
            memory: vec![
                Memory {
                    y1: F::ZERO,
                    y2: F::ZERO
                };
                count * width
            ],
            last_input: vec![F::ZERO; width],
        };
        bank.retarget(&ModalControls::of_first(modes));
        bank
    }

    /// The modes as configured.
    #[must_use]
    pub fn modes(&self) -> &Modes {
        &self.modes
    }

    /// Designs every mode for `controls`; the state is kept. No allocation.
    pub fn retarget(&mut self, controls: &ModalControls) {
        let first = self.modes.first();
        let ratio = |now: f64, base: f64| {
            let r = now / base;
            if now.is_finite() && now > 0.0 && r.is_finite() && r > 0.0 {
                r
            } else {
                1.0
            }
        };
        let f_ratio = ratio(controls.frequency.get(), first.frequency.get());
        let t_ratio = ratio(controls.decay.get(), first.decay.get());
        let nyquist = self.rate.nyquist();
        for (mode, res) in self.modes.as_slice().iter().zip(&mut self.resonators) {
            let f = Hz::new(mode.frequency.get() * f_ratio);
            if !(f.get() > 0.0 && f < nyquist) || !mode.gain.get().is_finite() {
                *res = Resonator {
                    a1: F::ZERO,
                    a2: F::ZERO,
                    b: F::ZERO,
                };
                continue;
            }
            let r = per_frame(Seconds::new(mode.decay.get() * t_ratio), self.rate);
            let w = f.angle_per_frame(self.rate);
            *res = Resonator {
                a1: F::from_design(2.0 * r * w.cos()),
                a2: F::from_design(-r * r),
                b: F::from_design(mode.gain.get() * r * w.sin()),
            };
        }
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.memory.fill(Memory {
            y1: F::ZERO,
            y2: F::ZERO,
        });
        self.last_input.fill(F::ZERO);
    }

    /// How long it rings after its input stops, to [`TAIL_FLOOR`], erring long, at
    /// `controls`: the response is under `sum |g| r^n` for the slowest mode's `r`.
    #[must_use]
    pub fn tail(&self, controls: &ModalControls) -> Tail {
        let first = self.modes.first();
        let t_ratio = controls.decay.get() / first.decay.get();
        let t_ratio = if t_ratio.is_finite() && t_ratio > 0.0 {
            t_ratio
        } else {
            1.0
        };
        let slowest = self
            .modes
            .as_slice()
            .iter()
            .map(|m| per_frame(Seconds::new(m.decay.get() * t_ratio), self.rate))
            .fold(0.0, f64::max);
        let total: f64 = self
            .modes
            .as_slice()
            .iter()
            .map(|m| m.gain.get().abs())
            .sum();
        let floor = if total > 1.0 {
            TAIL_FLOOR - Db::from_amplitude(Amplitude::new(total))
        } else {
            TAIL_FLOOR
        };
        let frames = ring_frames(slowest, floor);
        if frames.is_finite() {
            Tail::Finite(Samples::new(libm::ceil(frames) as usize + 2))
        } else {
            Tail::Unbounded
        }
    }

    /// Runs `input` into `output`, channel by channel: the fewer of the two views' channels
    /// and frames. No allocation.
    pub fn process(&mut self, input: Planes<'_, F>, mut output: PlanesMut<'_, '_, F>) {
        let frames = input.frames().get().min(output.frames().get());
        let count = self.resonators.len();
        let width = input
            .channels()
            .min(output.channels())
            .min(self.last_input.len());
        for c in 0..width {
            let memory = &mut self.memory[c * count..(c + 1) * count];
            let mut x1 = self.last_input[c];
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            for (y, &x) in plane.samples_mut()[..frames].iter_mut().zip(x) {
                let mut sum = F::ZERO;
                for (res, m) in self.resonators.iter().zip(memory.iter_mut()) {
                    let next = res.a1 * m.y1 + res.a2 * m.y2 + res.b * x1;
                    m.y2 = m.y1;
                    m.y1 = next;
                    sum += next;
                }
                x1 = x;
                *y = sum;
            }
            self.last_input[c] = x1;
        }
    }
}

#[cfg(test)]
mod tests;
