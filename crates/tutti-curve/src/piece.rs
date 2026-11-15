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

    /// The value `t` of the way along the piece: `from + (to - from) * g(t)`,
    /// with `g` the law. The one evaluation of a piece the graph, the lanes and
    /// the editor share.
    #[inline]
    #[must_use]
    pub fn value(self, t: f32) -> f32 {
        self.from + (self.to - self.from) * self.law.eval(t)
    }

    /// [`value`](Self::value) of a piece of `param`: a circular param
    /// ([`UnitParam::is_circular`]) takes the short arc, `wrap(from + arc(from,
    /// to) * g(t))`, so a piece from 170 to -170 passes through 180 and never
    /// through 0; any other param is [`value`](Self::value). The piece's own
    /// endpoints are not wrapped: only a value strictly inside it is.
    #[inline]
    #[must_use]
    pub fn value_of(self, param: UnitParam, t: f32) -> f32 {
        if param.is_circular() {
            param.wrap(self.from + param.arc(self.from, self.to) * self.law.eval(t))
        } else {
            self.value(t)
        }
    }

    /// [`ramp_at`](Self::ramp_at) of a piece of `param`: the short arc for a
    /// circular param.
    #[inline]
    #[must_use]
    pub fn ramp_of_at(param: UnitParam, from: f32, to: f32, k: usize, n: usize) -> f32 {
        if k + 1 >= n {
            to
        } else {
            Self::new(from, to, Law::Linear).value_of(param, (k + 1) as f32 / n as f32)
        }
    }

    /// Frame `k` of an `n`-frame ramp from `from` to `to`: the linear piece at
    /// `t = (k + 1) / n`, so the last frame lands exactly on `to` and the next
    /// block starts from the exact value. A ramp is chained block after block,
    /// so it is linear (a shape flat at its ends would stall the slope at every
    /// block boundary); a one-off move between two states is a
    /// [`FadeLaw`](crate::FadeLaw).
    #[inline]
    #[must_use]
    pub fn ramp_at(from: f32, to: f32, k: usize, n: usize) -> f32 {
        if k + 1 >= n {
            to
        } else {
            Self::new(from, to, Law::Linear).value((k + 1) as f32 / n as f32)
        }
    }
}

#[cfg(test)]
mod tests;
