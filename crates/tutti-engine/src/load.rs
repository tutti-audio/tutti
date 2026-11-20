//! The engine's health as two readings: [`Load`], how much of each block's
//! time budget the render used, and [`BlockCounts`], what the blocks
//! amounted to. The engine writes both once per device block through
//! `tutti_rt`'s cells; an app reads them from any thread.

use tutti_rt::{Count, Counter, CounterWriter, Gauge, GaugeWriter, Reading, counter, gauge};
use tutti_units::Seconds;

/// The time constant of [`Load`]'s exponential smoothing: a step in the load
/// is 63% through after this long, whatever the block size.
pub const LOAD_MEAN_TIME_CONSTANT: Seconds = Seconds::new(0.3);

/// The render time over the block's real-time budget (`frames / sample_rate`),
/// smoothed over [`LOAD_MEAN_TIME_CONSTANT`] of audio time and seeded with the
/// first block's load: `0.5` used half the time the device allows, and
/// anything above `1.0` missed the deadline.
///
/// It is a plain `f64` behind [`get`](Self::get): no unit in `tutti_units` is a
/// fraction of a time budget (`Mix` and `Depth` are controls with a `0..=1`
/// range, and a load has no upper bound). The smoothed value hides a single
/// late block; the blocks that missed their budget are counted exactly in
/// [`BlockCounts::over_budget`].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct Load(f64);

impl Load {
    /// The load as a ratio of the time budget: finite and not negative.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }

    /// The load of a block that took `elapsed` against `budget`. A ratio that
    /// is NaN is `0.0` and one that overflows is `f64::MAX`, so the smoothed
    /// value stays finite.
    fn of_block(elapsed: Seconds, budget: Seconds) -> Self {
        let ratio = elapsed / budget;
        Self(if ratio.is_nan() {
            0.0
        } else {
            ratio.clamp(0.0, f64::MAX)
        })
    }
}

impl Reading for Load {
    fn to_bits(self) -> u64 {
        self.0.to_bits()
    }

    fn from_bits(bits: u64) -> Self {
        Self(f64::from_bits(bits))
    }
}

/// What the engine's blocks amounted to, each tally cumulative since the
/// engine was built: nothing resets, and an app diffs two readings to see
/// what happened between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockCounts {
    /// Blocks timed: every device block the graph rendered.
    pub blocks: u64,
    /// Blocks over budget (load above `1.0`).
    pub over_budget: u64,
    /// Blocks rendered silent (an audible dropout, not timed): the graph was
    /// held on the control side when the callback came, the buffer's width has
    /// no fold, or the graph's root cannot be placed on it. The session
    /// reports the rise as `SessionEvent::BlocksSilenced`.
    pub silenced: u64,
    /// Events the graph dropped: an event output port past its capacity
    /// (`tutti_graph::DEFAULT_EVENT_CAPACITY` per block unless the node
    /// declares its own), or a full delay or feedback FIFO, or an event a node
    /// skipped because the one it pairs with was refused. Nonzero means events
    /// were lost: a port refuses the newest (a note-off last), a FIFO may evict
    /// the oldest non-note-off; the rest reached the graph. A note-off refused
    /// and sent again later counts as refused. Which port: the session's
    /// `EventsDropped`.
    pub dropped_events: u64,
}

/// One tally per count, in field order.
impl Count for BlockCounts {
    type Words = [u64; 4];

    fn to_words(self) -> [u64; 4] {
        [
            self.blocks,
            self.over_budget,
            self.silenced,
            self.dropped_events,
        ]
    }

    fn from_words([blocks, over_budget, silenced, dropped_events]: [u64; 4]) -> Self {
        Self {
            blocks,
            over_budget,
            silenced,
            dropped_events,
        }
    }
}

/// The audio thread's side of the two readings. Not `Clone`; every method
/// takes `&mut self`, so there is one writer. Allocation-free and wait-free.
#[derive(Debug)]
pub(crate) struct Writers {
    load: GaugeWriter<Load>,
    counts: CounterWriter<BlockCounts>,
    /// The smoothed load, `None` until the first block seeds it.
    mean: Option<f64>,
}

/// The writers and their readers.
pub(crate) fn link() -> (Writers, Gauge<Load>, Counter<BlockCounts>) {
    let (load, gauge_reader) = gauge(Load::default());
    let (counts, counter_reader) = counter();
    (
        Writers {
            load,
            counts,
            mean: None,
        },
        gauge_reader,
        counter_reader,
    )
}

impl Writers {
    /// Counts `n` events the graph dropped.
    pub(crate) fn dropped_events(&mut self, n: u64) {
        if n > 0 {
            self.counts.add(BlockCounts {
                dropped_events: n,
                ..BlockCounts::default()
            });
        }
    }

    /// Counts one block rendered silent, refused the graph.
    pub(crate) fn silenced(&mut self) {
        self.counts.add(BlockCounts {
            silenced: 1,
            ..BlockCounts::default()
        });
    }

    /// Records one block that took `elapsed` against a budget of `budget`. A
    /// block with no budget (no frames) records nothing.
    pub(crate) fn record(&mut self, elapsed: Seconds, budget: Seconds) {
        if budget.get() <= 0.0 {
            return;
        }
        let block = Load::of_block(elapsed, budget).get();
        let mean = match self.mean {
            None => block,
            Some(mean) => {
                // Exact exponential smoothing over audio time, so the mean's
                // response does not depend on the block size.
                let a = LOAD_MEAN_TIME_CONSTANT.one_pole_coeff_over(budget);
                mean + a * (block - mean)
            }
        };
        self.mean = Some(mean);
        self.load.set(Load(mean));
        self.counts.add(BlockCounts {
            blocks: 1,
            over_budget: u64::from(block > 1.0),
            ..BlockCounts::default()
        });
    }
}

#[cfg(test)]
mod tests;
