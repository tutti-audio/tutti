use tutti_units::{Normalized, Samples, UnitParam};

use crate::Law;

/// One piece of a trajectory: a value moving from `from` to `to` along `law`,
/// read at a progress `t` in `[0, 1]`. The pure part of a segment, without an
/// address or a position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Piece {
    /// The value at progress 0.
    pub from: f64,
    /// The value at progress 1.
    pub to: f64,
    /// How the value moves between them.
    pub law: Law,
}

impl Piece {
    /// A piece from `from` to `to` along [`Law::Linear`].
    #[must_use]
    pub const fn new(from: f64, to: f64) -> Self {
        Self {
            from,
            to,
            law: Law::Linear,
        }
    }

    /// The piece along `law`.
    #[must_use]
    pub const fn law(mut self, law: Law) -> Self {
        self.law = law;
        self
    }

    /// The value `t` of the way along the piece of `param`: `from + (to -
    /// from) * g(t)`, with `g` the law, for a line (`None`, or a param that is
    /// not [circular](UnitParam::is_circular)). A circular param takes the
    /// short arc, `wrap(from + arc(from, to) * g(t))`, so a piece from 170 to
    /// -170 passes through 180 and never through 0; the piece's own endpoints
    /// are not wrapped, only a value strictly inside it. The one evaluation of
    /// a piece the graph, the lanes and the editor share.
    #[inline]
    #[must_use]
    pub fn value(self, param: Option<UnitParam>, t: Normalized) -> f64 {
        let g = self.law.eval(t).get();
        match param {
            Some(p) if p.is_circular() => p.wrap(self.from + p.arc(self.from, self.to) * g),
            _ => self.from + (self.to - self.from) * g,
        }
    }

    /// Frame `k` of an `n`-frame ramp from `from` to `to` of `param`: the
    /// linear piece at `t = (k + 1) / n` ([`value`](Self::value)), so the last
    /// frame lands exactly on `to` and the next block starts from the exact
    /// value. A ramp is chained block after block, so it is linear (a shape
    /// flat at its ends would stall the slope at every block boundary); a
    /// one-off move between two states is a [`FadeLaw`](crate::FadeLaw).
    #[inline]
    #[must_use]
    pub fn ramp_at(param: Option<UnitParam>, from: f64, to: f64, k: Samples, n: Samples) -> f64 {
        let (k, n) = (k.get(), n.get());
        if k + 1 >= n {
            to
        } else {
            Self::new(from, to).value(
                param,
                Normalized::progress(Samples::new(k + 1), Samples::new(n)),
            )
        }
    }
}

#[cfg(test)]
mod tests;
