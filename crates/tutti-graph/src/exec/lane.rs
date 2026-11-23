//! The arithmetic that renders a control value, a ramp or a fade into a
//! per-sample lane in the graph's float.

use tutti_buffers::Float;
use tutti_curve::{FadeLaw, Law};
use tutti_units::{Azimuth, Normalized, Samples, UnitParam};

use crate::node::param::CellValue;

/// A control value, the unit's `f64` quantity, as the lane's float: the one
/// crossing of a rendered ramp or segment into a lane, once per frame,
/// saturating ([`Real::from_unit`](tutti_units::Real::from_unit)).
///
/// The graph's quantity to lane crossing for a value whose unit the producer
/// does not name (a curve's value, a param event's).
#[inline]
pub fn control_in<F: Float>(value: f64) -> F {
    F::from_unit(CellValue(value))
}

/// `value` as `param` holds it: wrapped onto the circle for a circular param
/// ([`UnitParam::wrap`]), in the unit's `f64` and narrowed once to `F`.
#[inline]
pub(crate) fn wrap_in<F: Float>(param: UnitParam, value: F) -> F {
    if param.is_circular() {
        F::from_f64(Azimuth::new(value.to_f64()).wrap().get())
    } else {
        value
    }
}

/// The signed move from `from` to `to` for `param` ([`UnitParam::arc`]).
#[inline]
pub(crate) fn arc_in<F: Float>(param: UnitParam, from: F, to: F) -> F {
    if param.is_circular() {
        F::from_f64(
            Azimuth::new(from.to_f64())
                .shortest_arc_to(Azimuth::new(to.to_f64()))
                .get(),
        )
    } else {
        to - from
    }
}

/// A law's value at progress `t`: evaluated in `f64` and narrowed once to `F`.
#[inline]
pub(crate) fn law_in<F: Float>(law: Law, t: Normalized) -> F {
    F::from_f64(law.eval(t).get())
}

/// `Piece::value` read in `F`: `from + (to - from) * g(t)`, a circular
/// param along the short arc.
#[inline]
pub(crate) fn piece_in<F: Float>(
    param: Option<UnitParam>,
    from: F,
    to: F,
    law: Law,
    t: Normalized,
) -> F {
    let g = law_in(law, t);
    match param {
        Some(p) if p.is_circular() => wrap_in(p, from + arc_in(p, from, to) * g),
        _ => from + (to - from) * g,
    }
}

/// `Piece::ramp_at` read in `F`: frame `k` of an `n`-frame linear ramp,
/// landing exactly on `to` on its last frame.
///
/// The graph's quantity to lane crossing (a control ramp rendered in `F`).
#[inline]
pub fn ramp_in<F: Float>(param: Option<UnitParam>, from: F, to: F, k: Samples, n: Samples) -> F {
    let (k, n) = (k.get(), n.get());
    if k + 1 >= n {
        to
    } else {
        piece_in(
            param,
            from,
            to,
            Law::Linear,
            Normalized::progress(Samples::new(k + 1), Samples::new(n)),
        )
    }
}

/// `(out, in)`: the gains of frame `j` of a `len`-frame fade, in `F`. The law
/// is the curve crate's and its gains are `Amplitude`s; they narrow once here,
/// where the fade is rendered into a lane.
///
/// The graph's quantity to lane crossing (a fade law rendered in `F`).
#[inline]
pub fn fade_gains<F: Float>(law: FadeLaw, j: Samples, len: Samples) -> (F, F) {
    let (out, inn) = law.gains(j, len);
    (F::from_f64(out.get()), F::from_f64(inn.get()))
}

/// `start` moved to `goal` at frame `j` of a `len`-frame fade of `law`, and
/// exactly `goal` from `len` on.
///
/// The graph's quantity to lane crossing (a fade between two weights in `F`).
#[inline]
pub fn fade_in<F: Float>(law: FadeLaw, start: F, goal: F, j: Samples, len: Samples) -> F {
    if j >= len {
        return goal;
    }
    let (out, inn) = fade_gains::<F>(law, j, len);
    start * out + goal * inn
}
