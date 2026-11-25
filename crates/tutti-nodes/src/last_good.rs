//! A control a node reads once per block, or per frame, without a non-finite value reaching its
//! state: [`LastGood`].
//!
//! A node reads every [`Param`](tutti_rt::Param) once per `process` call (a per-sample load
//! would be several atomics a channel a sample), and a control signal that drives it replaces
//! the cell's value at its frames. A cell is a raw `AtomicF64` any thread may write, so
//! each read, from the cell or from the signal, goes through its [`LastGood`].

use tutti_buffers::Float;
use tutti_units::Amplitude;

/// The last finite value a control cell held — what a non-finite write reads
/// as.
///
/// A raw `Arc<AtomicF64>` cell accepts anything, and a NaN or ±∞ that reaches
/// a recursive node's state (a filter integrator, a delay line, an LFO phase,
/// an envelope follower) stays there for good: every later sample is computed
/// from it. So each per-block read goes through one of these, and a
/// non-finite value reads as "unchanged" — the last good value — until a
/// finite one is written.
///
/// Clamping cannot stand in for this: `f32::clamp` passes NaN through.
///
/// `F` is the width of the control's value: `f32` for a control the graph
/// carries as a control signal, `f64` for one held in an `f64` unit (an angle).
#[derive(Clone, Copy, Debug)]
pub(crate) struct LastGood<F: Float = f32>(F);

impl<F: Float> LastGood<F> {
    /// Starts at `initial`, or at `fallback` if `initial` is not finite.
    ///
    /// `fallback` is the control's own default, chosen by the caller: a
    /// control whose range excludes zero (a ratio, a Q) must not start at
    /// `0.0`, which is outside it.
    pub(crate) fn new(initial: F, fallback: F) -> Self {
        Self(if initial.is_finite() {
            initial
        } else {
            fallback
        })
    }

    /// Starts at `initial` if `valid`, else at `fallback`: a control that accepts more
    /// than the finite values says so (`ControlSet::valid`).
    pub(crate) fn new_if(initial: F, fallback: F, valid: bool) -> Self {
        Self(if valid { initial } else { fallback })
    }

    /// Starts at a finite constant.
    pub(crate) fn fixed(initial: F) -> Self {
        Self(initial)
    }

    /// `value` if finite (and remembered), otherwise the last finite value.
    #[inline]
    pub(crate) fn read(&mut self, value: F) -> F {
        self.read_if(value, value.is_finite())
    }

    /// `value` if `valid` (and remembered), otherwise the last valid value: for a
    /// control whose valid values are not the finite ones (a ceiling accepts
    /// `+-inf`, `ControlSet::valid`).
    #[inline]
    pub(crate) fn read_if(&mut self, value: F, valid: bool) -> F {
        if valid {
            self.0 = value;
        }
        self.0
    }
}

impl LastGood<f64> {
    /// A block's value of a control the graph may drive: the signal's `driven` value (its last
    /// frame), else the `cell`'s, held at the last finite one.
    #[inline]
    pub(crate) fn block(&mut self, driven: Option<f64>, cell: f64) -> f64 {
        self.read(driven.unwrap_or(cell))
    }

    /// A level the cell holds, as the output's float: held at the last finite one.
    #[inline]
    pub(crate) fn level<F: Float>(&mut self, cell: f64) -> F {
        F::from_unit(Amplitude::new(self.read(cell)))
    }

    /// A control signal's level at a frame: a finite value is the level in `F` as it is, and
    /// a non-finite one reads as the last finite level.
    #[inline]
    pub(crate) fn driven_level<F: Float>(&mut self, driven: F) -> F {
        // The read runs on every frame: a finite `driven` becomes the held value.
        let held = self.read(driven.into());
        if driven.is_finite() {
            driven
        } else {
            F::from_unit(Amplitude::new(held))
        }
    }
}
