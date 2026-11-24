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

/// The law every declick fade runs: an edge's weight, a unit's mode weights
/// and a param port's source change all fade in phase.
const DECLICK_LAW: FadeLaw = FadeLaw::EqualAmplitude;

/// `(out, in)`: the gains of frame `j` of a declick of `len` frames.
#[inline]
pub(crate) fn declick_gains<F: Float>(j: Samples, len: Samples) -> (F, F) {
    fade_gains(DECLICK_LAW, j, len)
}

/// `start` moved to `goal` at frame `j` of a declick of `len` frames, and
/// exactly `goal` from `len` on.
#[inline]
pub(crate) fn declick_between<F: Float>(start: F, goal: F, j: Samples, len: Samples) -> F {
    fade_in(DECLICK_LAW, start, goal, j, len)
}

/// How far one declick fade has run: the frames of its length it has run, or
/// `None` when it does not. The length is [`Prepare::declick`] and is the
/// caller's: every method that needs it takes it, so a fade reads the length
/// of the plan it runs in and no copy of it goes stale.
///
/// A fade is *running* from [`restart`](Self::restart) until the block that
/// runs its last frame (or [`cut`](Self::cut)). A running fade's progress may
/// exceed the length it is read against (one carried to a shorter length): its
/// next block is its last, at the goal.
///
/// [`Prepare::declick`]: crate::Prepare
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Progress(Option<Samples>);

impl Progress {
    /// A fade about to run its first frame over `len` frames.
    pub(crate) fn started(len: Samples) -> Self {
        let mut p = Self::default();
        p.restart(len);
        p
    }

    /// Run again from the first frame. A fade of no frames has none to run,
    /// and stays idle.
    pub(crate) fn restart(&mut self, len: Samples) {
        self.0 = (len > Samples::ZERO).then_some(Samples::ZERO);
    }

    /// End the fade where it is: it no longer runs.
    pub(crate) fn cut(&mut self) {
        self.0 = None;
    }

    /// Whether the fade has a block left to run.
    pub(crate) fn running(self) -> bool {
        self.0.is_some()
    }

    /// The frames run so far: the fade's frame the next block starts at. The
    /// whole `len` when the fade does not run.
    pub(crate) fn done(self, len: Samples) -> Samples {
        self.0.unwrap_or(len)
    }

    /// The frames of `len` left to run.
    pub(crate) fn remaining(self, len: Samples) -> Samples {
        len.remaining_after(self.done(len))
    }

    /// `frames` more have run; the fade stops running once it has run `len`.
    pub(crate) fn advance(&mut self, frames: Samples, len: Samples) {
        if let Some(at) = &mut self.0 {
            *at += frames;
            if *at >= len {
                self.0 = None;
            }
        }
    }

    /// The last frame run: the one before [`done`](Self::done).
    pub(crate) fn last_frame(self, len: Samples) -> Samples {
        self.done(len).remaining_after(Samples::new(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A declick of no frames never runs: starting or restarting it leaves it
    /// idle, so no block reads a gain over a zero length.
    #[test]
    fn a_zero_length_declick_never_runs() {
        assert!(!Progress::started(Samples::ZERO).running());
        let mut p = Progress::default();
        p.restart(Samples::ZERO);
        assert!(!p.running());
        assert!(Progress::started(Samples::new(1)).running());
    }

    /// A fade read against a shorter length than it started under keeps its
    /// progress and runs one last block, then stops.
    #[test]
    fn a_fade_past_its_length_runs_one_more_block() {
        let mut p = Progress::started(Samples::new(240));
        p.advance(Samples::new(200), Samples::new(240));
        let len = Samples::new(120);
        assert_eq!((p.running(), p.done(len)), (true, Samples::new(200)));
        p.advance(Samples::new(64), len);
        assert!(!p.running());
        assert_eq!(p.done(len), len);
    }
}
