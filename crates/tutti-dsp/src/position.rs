//! The interpolation and the ramp of a read position.

use tutti_units::ReadPosition;

use crate::{Lerp, Ramp, Steps};

impl Lerp for ReadPosition {
    /// `from + (to - from) * k / n` in integers, truncated toward `from` to the quantum.
    #[inline]
    fn lerp(&self, to: &ReadPosition, k: u32, n: u32) -> ReadPosition {
        let from = i128::from(self.get());
        let diff = i128::from(to.get()) - from;
        let at = from + diff * i128::from(k) / i128::from(n.max(1));
        // Between the ends for any `k`: a `k` past `n` reads the far end, never a wrap.
        let (lo, hi) = (
            from.min(i128::from(to.get())),
            from.max(i128::from(to.get())),
        );
        ReadPosition::from_count(at.clamp(lo, hi) as u64)
    }
}

impl Ramp<ReadPosition> {
    /// Aims at `to` in `steps`: a count of steps, or no more than a rate in frames a step.
    /// A target already aimed at leaves the ramp as it is.
    pub fn retarget_position(&mut self, to: ReadPosition, steps: Steps) {
        if to == self.to {
            return;
        }
        let n = match steps {
            Steps::Fixed(n) => n,
            Steps::Rate(rate) => {
                let now = self.now().get();
                let frames = to.get().abs_diff(now) as f64 / ReadPosition::COUNTS_PER_FRAME;
                // `as` saturates at `u32::MAX`, so a rate of zero is the slowest ramp.
                (frames / rate).ceil() as u32
            }
        };
        self.retarget_in(to, n);
    }
}

#[cfg(test)]
mod tests;
