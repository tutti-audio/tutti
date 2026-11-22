//! The timer's schedule: when each block is due, and what to do at a given
//! time. Pure: it reads no clock and sleeps no thread.

use tutti_units::SampleRate;

/// How many blocks late a wake-up may be before the schedule is given up and
/// begun again: a block is owed back to back up to this lag, and past it the
/// owed blocks are dropped.
pub(crate) const MAX_LAG_BLOCKS: i64 = 4;

/// What the loop does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Due {
    /// The next block is not due: sleep until this time.
    Wait(i64),
    /// Render the next block, dated by this deadline.
    Render(i64),
    /// The thread fell too far behind: the schedule began again from now. Poll
    /// again; the next block is due at once.
    Reanchored,
}

/// The schedule of a block of `frames` frames at `rate`: block `n` is due at
/// `start + n * block - block`, where `block` is `frames / rate` in
/// nanoseconds and `start` is when block 0 plays.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pacer {
    start_ns: i64,
    frames: f64,
    rate: f64,
    block_ns: i64,
    next: u64,
}

impl Pacer {
    /// A schedule whose block 0 is due at `now_ns`, so it plays one block
    /// after.
    pub(crate) fn new(now_ns: i64, frames: usize, rate: SampleRate) -> Self {
        let (frames, rate) = (frames as f64, rate.get());
        let block_ns = (frames * 1e9 / rate).round() as i64;
        Self {
            start_ns: now_ns.saturating_add(block_ns),
            frames,
            rate,
            block_ns,
            next: 0,
        }
    }

    /// The time block `n` is due, in nanoseconds: computed from the start, so
    /// the rounding of one block is not added to the next.
    pub(crate) fn deadline_ns(&self, n: u64) -> i64 {
        let from_start = (n as f64 - 1.0) * self.frames * 1e9 / self.rate;
        self.start_ns.saturating_add(from_start.round() as i64)
    }

    /// What to do at `now_ns`: wait for the next block, render it, or begin
    /// the schedule again when it is more than [`MAX_LAG_BLOCKS`] late.
    ///
    /// A block rendered advances the schedule; a wait or a re-anchor does not
    /// consume one.
    pub(crate) fn poll(&mut self, now_ns: i64) -> Due {
        let deadline = self.deadline_ns(self.next);
        if now_ns < deadline {
            return Due::Wait(deadline);
        }
        if now_ns - deadline > MAX_LAG_BLOCKS * self.block_ns {
            // Block 0 of the new schedule is due now. Only the schedule
            // restarts: the engine's frame count continues, so the graph's
            // frames stay continuous across the jump in OS time.
            self.start_ns = now_ns.saturating_add(self.block_ns);
            self.next = 0;
            return Due::Reanchored;
        }
        self.next += 1;
        Due::Render(deadline)
    }
}
