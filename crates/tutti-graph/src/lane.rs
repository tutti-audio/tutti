//! The arithmetic that renders a control value, a ramp or a fade into a
//! per-sample lane in the graph's float.

use tutti_buffers::Float;
use tutti_curve::{FadeLaw, Law};
use tutti_units::{Azimuth, UnitParam};

/// `value` as `param` holds it: wrapped onto the circle for a circular param
/// ([`UnitParam::wrap`]), in the unit's `f64` and narrowed once to `F`.
#[inline]
pub(crate) fn wrap_in<F: Float>(param: UnitParam, value: F) -> F {
    if param.is_circular() {
        F::from_f64(Azimuth(value.to_f64()).wrap().get())
    } else {
        value
    }
}

/// The signed move from `from` to `to` for `param` ([`UnitParam::arc`]).
#[inline]
pub(crate) fn arc_in<F: Float>(param: UnitParam, from: F, to: F) -> F {
    if param.is_circular() {
        F::from_f64(
            Azimuth(from.to_f64())
                .shortest_arc_to(Azimuth(to.to_f64()))
                .get(),
        )
    } else {
        to - from
    }
}

/// A law's value at progress `t`, in `F`. A line is exact in `F`; a shaped
/// law is evaluated at its own `f32` width (its exponent and curvature words
/// are `f32`).
#[inline]
pub(crate) fn law_in<F: Float>(law: Law, t: F) -> F {
    match law {
        Law::Linear => {
            if t >= F::ZERO {
                t.min(F::ONE)
            } else {
                F::ZERO
            }
        }
        shaped => F::from_f32(shaped.eval(t.to_f32())),
    }
}

/// `Piece::value` read in `F`: `from + (to - from) * g(t)`, a circular
/// param along the short arc.
#[inline]
pub(crate) fn piece_in<F: Float>(param: Option<UnitParam>, from: F, to: F, law: Law, t: F) -> F {
    let g = law_in(law, t);
    match param {
        Some(p) if p.is_circular() => wrap_in(p, from + arc_in(p, from, to) * g),
        _ => from + (to - from) * g,
    }
}

/// `Piece::ramp_at` read in `F`: frame `k` of an `n`-frame linear ramp,
/// landing exactly on `to` on its last frame.
#[inline]
pub fn ramp_in<F: Float>(param: Option<UnitParam>, from: F, to: F, k: usize, n: usize) -> F {
    if k + 1 >= n {
        to
    } else {
        let count = |i: usize| F::from_f64(i as f64);
        piece_in(param, from, to, Law::Linear, count(k + 1) / count(n))
    }
}

/// `(out, in)`: the gains of frame `j` of a `len`-frame fade, in `F`. The law
/// is the curve crate's and its gains are `Amplitude`s; they narrow once here,
/// where the fade is rendered into a lane.
#[inline]
pub fn fade_gains<F: Float>(law: FadeLaw, j: usize, len: usize) -> (F, F) {
    let (out, inn) = law.gains(j, len);
    (F::from_f64(out.get()), F::from_f64(inn.get()))
}

/// `start` moved to `goal` at frame `j` of a `len`-frame fade of `law`, and
/// exactly `goal` from `len` on.
#[inline]
pub fn fade_in<F: Float>(law: FadeLaw, start: F, goal: F, j: usize, len: usize) -> F {
    if j >= len {
        return goal;
    }
    let (out, inn) = fade_gains::<F>(law, j, len);
    start * out + goal * inn
}

/// A control value (a param cell's, an edge weight's, a mix's) as the lane's
/// float: the one place a node-side control is read into a lane. It widens
/// exactly and narrows once.
#[inline]
pub fn control_in<F: Float>(value: impl Into<f64>) -> F {
    F::from_f64(value.into())
}
