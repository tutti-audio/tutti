//! The pure pieces of the phaser's swept first-order all-pass chain: the centre frequency
//! for a sweep position and the coefficient that tunes a stage to it.

use tutti_buffers::Float;
use tutti_units::{Bipolar, Hz, SampleRate};

/// The all-pass centre frequency for one LFO value: the sweep range's `min`
/// at the LFO's trough, its `max` at the peak, scaled toward `min` by `depth`.
///
/// `lfo` is the oscillator's raw value in `-1..=1`; it is mapped to `0..=1`
/// (`lfo * 0.5 + 0.5`) first, and the sweep is `min + (max - min) * lfo01 *
/// depth`. A `depth` of 0 parks the centre at `min`, 1 sweeps the whole range.
/// The result is not held to any band: [`allpass_complement`] clamps it.
#[inline]
#[must_use]
pub fn sweep_hz(min: Hz, max: Hz, lfo: Bipolar, depth: Bipolar) -> Hz {
    let lfo = lfo.get() * 0.5 + 0.5;
    let (min, max) = (min.get(), max.get());
    Hz::new(min + (max - min) * lfo * depth.get())
}

/// The lowest frequency an all-pass stage is tuned to (see [`allpass_complement`]).
const SWEEP_FLOOR: Hz = Hz::new(1.0);

/// The share of Nyquist an all-pass stage is tuned up to (see [`allpass_complement`]).
const SWEEP_CEILING: f64 = 0.98;

/// The complement `1 + c` of one first-order all-pass coefficient `c = (t - 1) / (t + 1)` for
/// a sweep position, `t` the tangent of the normalised centre: `2 t / (t + 1)`, designed in
/// `f64` and narrowed once to the chain's float `F`.
///
/// A low centre puts `c` within `2 t` of -1 (at 20 Hz and 192 kHz, 6.5e-4), where `f32` rounds
/// `c` to an absolute `2^-25`: a relative error of up to 4.6e-5 in the stage's pole offset. The
/// complement keeps full relative precision, and the stage runs on it
/// ([`PhaserChain`](crate::PhaserChain)). A phaser solves it only at the corners of a segment
/// (every 16 frames of the stream) and ramps between, rather than per sample and channel.
///
/// The sweep is held inside `SWEEP_FLOOR..=0.98 ·` Nyquist first. Outside it
/// `tan` of the normalised frequency is negative (a sweep below 0 Hz, which a
/// negative `Bipolar` reaches: a depth is held in −1..=1) or past its
/// pole (above Nyquist), `|c|` exceeds 1 and the all-pass diverges. Every
/// sweep a depth in `0..=1` makes lies in `[min_hz, max_hz]`, inside the hold
/// (`min_hz >= 20`, `max_hz <= 0.90` of Nyquist), so the hold changes nothing
/// there.
#[inline]
#[must_use]
pub fn allpass_complement<F: Float>(sweep: Hz, rate: SampleRate) -> F {
    let sweep_hz = sweep
        .max(SWEEP_FLOOR)
        .min(rate.nyquist_scaled(SWEEP_CEILING))
        .get();
    let t = libm::tan(core::f64::consts::PI * sweep_hz / rate.get());
    F::from_design(2.0 * t / (t + 1.0))
}

#[cfg(test)]
mod tests;
