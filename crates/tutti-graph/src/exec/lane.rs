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

/// The progress of one declick fade of [`FadeLaw::EqualAmplitude`]: how many
/// of its `len` frames have run. Every fade that declicks (an edge's weight,
/// a unit's mode weights, a param port's source change) counts its frames
/// here and reads its gains from here; what it fades between stays its own.
///
/// A fade is *running* from [`restart`](Self::restart) or
/// [`started`](Self::started) until the block that runs its last frame (or
/// [`cut`](Self::cut)). A running fade's progress may exceed its length (one
/// carried to a shorter length by [`relength`](Self::relength)): its next
/// block is its last, at the goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Declick {
    len: Samples,
    /// The frames run, while the fade runs; `None` when it does not.
    at: Option<Samples>,
}

impl Declick {
    /// The fade law every declick runs.
    const LAW: FadeLaw = FadeLaw::EqualAmplitude;

    /// A fade of `len` frames that is not running.
    pub(crate) const fn idle(len: Samples) -> Self {
        Self { len, at: None }
    }

    /// A fade of `len` frames about to run its first frame.
    pub(crate) const fn started(len: Samples) -> Self {
        Self {
            len,
            at: Some(Samples::ZERO),
        }
    }

    /// Run again from the first frame.
    pub(crate) fn restart(&mut self) {
        self.at = Some(Samples::ZERO);
    }

    /// End the fade where it is: it no longer runs.
    pub(crate) fn cut(&mut self) {
        self.at = None;
    }

    /// The same fade over `len` frames, keeping its progress.
    pub(crate) fn relength(self, len: Samples) -> Self {
        Self { len, ..self }
    }

    /// Whether the fade has a block left to run.
    pub(crate) fn running(self) -> bool {
        self.at.is_some()
    }

    /// The fade's length.
    pub(crate) fn len(self) -> Samples {
        self.len
    }

    /// The frames run so far: the fade's frame the next block starts at. The
    /// whole length when the fade does not run.
    pub(crate) fn done(self) -> Samples {
        self.at.unwrap_or(self.len)
    }

    /// The frames left to run.
    pub(crate) fn remaining(self) -> Samples {
        self.len.remaining_after(self.done())
    }

    /// `frames` more have run; the fade stops running once it has run its
    /// length.
    pub(crate) fn advance(&mut self, frames: Samples) {
        if let Some(at) = &mut self.at {
            *at += frames;
            if *at >= self.len {
                self.at = None;
            }
        }
    }

    /// The last frame run: the one before [`done`](Self::done).
    pub(crate) fn last_frame(self) -> Samples {
        self.done().remaining_after(Samples::new(1))
    }

    /// `(out, in)`: the gains of fade frame `j`.
    #[inline]
    pub(crate) fn gains<F: Float>(self, j: Samples) -> (F, F) {
        fade_gains(Self::LAW, j, self.len)
    }

    /// `start` moved to `goal` at fade frame `j`, and exactly `goal` from the
    /// fade's length on.
    #[inline]
    pub(crate) fn between<F: Float>(self, start: F, goal: F, j: Samples) -> F {
        fade_in(Self::LAW, start, goal, j, self.len)
    }
}
