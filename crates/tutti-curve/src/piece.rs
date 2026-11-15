use tutti_types::UnitParam;

use crate::Law;

/// One piece of a trajectory: a value moving from `from` to `to` along `law`,
/// read at a progress `t` in `[0, 1]`. The pure part of a segment, without an
/// address or a position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Piece {
    /// The value at progress 0.
    pub from: f32,
    /// The value at progress 1.
    pub to: f32,
    /// How the value moves between them.
    pub law: Law,
}

impl Piece {
    /// A piece from `from` to `to` along `law`.
    #[must_use]
    pub const fn new(from: f32, to: f32, law: Law) -> Self {
        Self { from, to, law }
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
    pub fn value(self, param: Option<UnitParam>, t: f32) -> f32 {
        match param {
            Some(p) if p.is_circular() => {
                p.wrap(self.from + p.arc(self.from, self.to) * self.law.eval(t))
            }
            _ => self.from + (self.to - self.from) * self.law.eval(t),
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
    pub fn ramp_at(param: Option<UnitParam>, from: f32, to: f32, k: usize, n: usize) -> f32 {
        if k + 1 >= n {
            to
        } else {
            Self::new(from, to, Law::Linear).value(param, (k + 1) as f32 / n as f32)
        }
    }
}

#[cfg(test)]
mod tests;
