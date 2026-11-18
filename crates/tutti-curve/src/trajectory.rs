use alloc::vec;
use alloc::vec::Vec;
use tutti_units::{Beat, Unit};

use crate::{Law, Piece};

/// A breakpoint of a [`Trajectory`]: a value in the trajectory's unit at a beat,
/// and the [`Law`] of the piece that starts there and runs to the next
/// breakpoint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Breakpoint<U: Unit<Raw = f32>> {
    /// Where the breakpoint is.
    pub time: Beat,
    /// The value there.
    pub value: U,
    /// How the value moves from here to the next breakpoint.
    pub law: Law,
}

impl<U: Unit<Raw = f32>> Breakpoint<U> {
    /// A breakpoint whose piece is [`Law::Linear`].
    #[must_use]
    pub const fn new(time: Beat, value: U) -> Self {
        Self::with_law(time, value, Law::Linear)
    }

    /// A breakpoint whose piece follows `law`.
    #[must_use]
    pub const fn with_law(time: Beat, value: U, law: Law) -> Self {
        Self { time, value, law }
    }
}

/// Breakpoints in time order, read at a beat: the least a lane or a recorder
/// needs of automation.
///
/// Before the first breakpoint and from the last on, the value is that breakpoint's.
/// Between two breakpoints it is the first's value moving to the second's along the
/// first's [`Law`]. A trajectory may carry a range its values are clamped to.
/// It has no value (`None`) while it has no breakpoint.
///
/// It is typed over the unit `U` of the quantity it automates, so a breakpoint,
/// its range and the value it reads are all in that unit.
#[derive(Clone, Debug, PartialEq)]
pub struct Trajectory<U: Unit<Raw = f32>> {
    breakpoints: Vec<Breakpoint<U>>,
    range: Option<(U, U)>,
}

impl<U: Unit<Raw = f32>> Default for Trajectory<U> {
    fn default() -> Self {
        Self {
            breakpoints: Vec::new(),
            range: None,
        }
    }
}

impl<U: Unit<Raw = f32>> Trajectory<U> {
    /// An empty trajectory with no range.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same trajectory with every value it reads clamped to `[min, max]`.
    #[must_use]
    pub fn with_range(mut self, min: U, max: U) -> Self {
        self.range = Some((min, max));
        self
    }

    /// The range values are clamped to, if there is one.
    #[must_use]
    pub fn range(&self) -> Option<(U, U)> {
        self.range
    }

    /// The breakpoints, in time order.
    #[must_use]
    pub fn breakpoints(&self) -> &[Breakpoint<U>] {
        &self.breakpoints
    }

    /// How many breakpoints there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.breakpoints.len()
    }

    /// Whether there is no breakpoint.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.breakpoints.is_empty()
    }

    /// Inserts `breakpoint` in time order; it replaces a breakpoint at the same time.
    pub fn add_breakpoint(&mut self, breakpoint: Breakpoint<U>) -> &mut Self {
        let at = breakpoint.time.get();
        match self
            .breakpoints
            .binary_search_by(|p| p.time.get().total_cmp(&at))
        {
            Ok(i) => self.breakpoints[i] = breakpoint,
            Err(i) => self.breakpoints.insert(i, breakpoint),
        }
        self
    }

    /// The value at `beat`, or `None` for an empty trajectory. Allocation-free:
    /// a lane reads it on the audio thread.
    #[must_use]
    #[inline]
    pub fn value_at(&self, beat: Beat) -> Option<U> {
        let (first, last) = (self.breakpoints.first()?, self.breakpoints.last()?);
        let time = beat.get();
        if time <= first.time.get() {
            return Some(self.clamped(first.value.to_raw()));
        }
        if time >= last.time.get() {
            return Some(self.clamped(last.value.to_raw()));
        }
        // `first.time < time < last.time`: the piece is the one before the
        // first breakpoint after `time`; a breakpoint exactly at `time` starts its own.
        let next = self.breakpoints.partition_point(|p| p.time.get() <= time);
        let (p, q) = (self.breakpoints[next - 1], self.breakpoints[next]);
        let t = ((time - p.time.get()) / (q.time.get() - p.time.get())) as f32;
        Some(self.clamped(Piece::new(p.value.to_raw(), q.value.to_raw(), p.law).value(None, t)))
    }

    /// Drops every breakpoint whose value its neighbours' piece already gives to
    /// within `tolerance`, a distance in the unit's raw value; the first and last stay.
    pub fn simplify(&mut self, tolerance: f32) -> &mut Self {
        if self.breakpoints.len() <= 2 {
            return self;
        }
        let last = self.breakpoints.len() - 1;
        let mut kept = vec![self.breakpoints[0]];
        for i in 1..last {
            let (prev, curr, next) = (
                self.breakpoints[i - 1],
                self.breakpoints[i],
                self.breakpoints[i + 1],
            );
            let span = next.time.get() - prev.time.get();
            let t = ((curr.time.get() - prev.time.get()) / span) as f32;
            if (curr.value.to_raw()
                - Piece::new(prev.value.to_raw(), next.value.to_raw(), prev.law).value(None, t))
            .abs()
                > tolerance
            {
                kept.push(curr);
            }
        }
        kept.push(self.breakpoints[last]);
        self.breakpoints = kept;
        self
    }

    fn clamped(&self, value: f32) -> U {
        U::from_raw(match self.range {
            Some((min, max)) => value.max(min.to_raw()).min(max.to_raw()),
            None => value,
        })
    }
}

#[cfg(test)]
mod tests;
