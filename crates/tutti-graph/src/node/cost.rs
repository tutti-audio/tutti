//! The cost model.
//!
//! The compiler estimates every op's work per block from what the node
//! declared ([`Shape::cost`](crate::Shape::cost)) and its channel count, as
//! an affine function of the block length ([`Work`]). It uses them for
//! **coarsening** (`compile/coarsen.rs`): sibling tasks (one predecessor
//! set) cheaper than [`GRAIN`] are packed together until a task is worth
//! dispatching, where that does not lengthen the costliest path through the
//! task DAG.
//!
//! The estimates never decide whether a block spreads: with a pool, every
//! block of a plan with more than one task runs on all of its participants.
//!
//! # Why declared, not measured
//!
//! The estimate is built from a coarse declared class per node times its
//! channels times the block length. Two alternatives were weighed:
//!
//! - **Channels × frames alone** cannot tell a gain from a filter from a
//!   plugin: they differ by two to three orders of magnitude at the same
//!   width, and the one decision that matters (is a sibling worth a task of
//!   its own?) turns on exactly that difference.
//! - **Measuring** each task on the audio thread would know real costs, but
//!   needs a clock read around every task (tens of nanoseconds, the size of
//!   the tiny tasks it is meant to judge), makes the plan's shape depend on
//!   timing noise (so a test cannot pin it), and would have to restructure
//!   tasks on the audio thread or round-trip to the compiler to coarsen.
//!
//! A declared class is pure and deterministic, costs nothing per block, and
//! errs safely: the default, [`Cost::Heavy`], is the cost of a node that
//! declares nothing (a plugin, a host's node), so any two independent such
//! nodes are each worth a task of its own. Only a node that says it is
//! cheap is packed; copy and event ops are priced cheap, so sibling ones
//! pack where that does not lengthen the critical path. A wrong `Light`
//! costs at most the parallel speed-up of a graph its author said was
//! trivial.
//!
//! # The constants
//!
//! Work is counted in **units of about a nanosecond of one core** on the
//! machine the constants were calibrated on. Only their
//! ratios matter to packing: a faster core shrinks node work and
//! dispatch alike. They are a speed decision only — a render is bit for bit
//! the same at any participant count, so a wrong estimate never changes a
//! sample.

use crate::compile::ids::{PlanUnitId, TableSlice};
use crate::compile::plan::Op;
use crate::node::shape::Shape;
use tutti_units::Samples;

/// What a node declares about its processing cost, per sample and channel:
/// an order of magnitude the compiler's cost model works from (see
/// [`Shape::cost`](crate::Shape::cost)).
///
/// A speed hint only. Whatever a node declares, the render is bit for bit the
/// same at any participant count; a wrong class can only make a block slower.
///
/// `#[non_exhaustive]`: a finer class may come later without a breaking
/// change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cost {
    /// A few arithmetic operations per sample and channel: a gain, a pan, a
    /// sum, a meter. Siblings this cheap are packed into one task.
    Light,
    /// Tens of operations per sample and channel: a biquad or
    /// state-variable filter, an oscillator, a dynamics detector.
    Moderate,
    /// Expensive, or unknown: a hosted plugin (whose call crosses a process
    /// boundary), a convolution, a synth. **The default**, so a node that
    /// declares nothing is assumed worth a task of its own at any block
    /// length.
    #[default]
    Heavy,
}

impl Cost {
    /// `(fixed, per sample per channel)` work, in the model's units.
    const fn weights(self) -> (u64, u64) {
        match self {
            // A gain's multiply plus its loop; the call itself.
            Cost::Light => (100, 1),
            // `SvfFilterNode<f64>`: about 8 ns per frame on the bench box.
            Cost::Moderate => (200, 8),
            // A plugin: an out-of-process call is tens of microseconds
            // before any DSP. Large enough that two independent ones are
            // never packed into one task.
            Cost::Heavy => (20_000, 256),
        }
    }
}

/// An estimate of work per block, affine in the block length:
/// `fixed + per_frame × frames`, in the cost model's units (about a
/// nanosecond of one core; see the module docs of `cost.rs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Work {
    /// Work per block whatever its length.
    pub(crate) fixed: u64,
    /// Work per frame.
    pub(crate) per_frame: u64,
}

impl Work {
    /// No work.
    pub(crate) const ZERO: Self = Self {
        fixed: 0,
        per_frame: 0,
    };

    /// The estimate for a block of `frames`.
    #[must_use]
    pub(crate) const fn at(self, frames: Samples) -> u64 {
        self.fixed
            .saturating_add(self.per_frame.saturating_mul(frames.get() as u64))
    }

    /// The sum of two estimates.
    #[must_use]
    pub(crate) const fn plus(self, other: Self) -> Self {
        Self {
            fixed: self.fixed.saturating_add(other.fixed),
            per_frame: self.per_frame.saturating_add(other.per_frame),
        }
    }
}

/// Charged to every task on top of its ops: taking it off the ready list,
/// its activation counters, its claims, and the cache lines its inputs
/// cross between cores on.
pub(crate) const TASK_DISPATCH: u64 = 400;

/// A task estimated below this at the plan's maximum block is packed with
/// its independent siblings (`compile/coarsen.rs`): five dispatches' worth.
pub(crate) const GRAIN: u64 = 5 * TASK_DISPATCH;

/// A copy of one channel (a main input, an output, a PDC delay, a
/// feedback capture).
const COPY: Work = Work {
    fixed: 50,
    per_frame: 1,
};

/// A level meter's op over `lanes` channels: per lane the scan reads every
/// frame (an absolute value, a maximum and a square-and-add, about twice a
/// copy's per-frame work) and folds the block into its cell (a fence and a few
/// stores, about a copy's fixed cost).
const fn meter_work(lanes: u64) -> Work {
    Work {
        fixed: COPY.fixed * lanes,
        per_frame: 2 * COPY.per_frame * lanes,
    }
}

/// A sum of `addends`, `weighted` of them lanes with a gain: a copy's fixed
/// cost, and one unit per addend per frame, two for a weighted one (a
/// multiply and an add). An estimate; `graph_render` measures it.
const fn sum_work(addends: u64, weighted: u64) -> Work {
    Work {
        fixed: COPY.fixed,
        per_frame: COPY.per_frame * (addends + weighted),
    }
}

/// An event op (a merge, an event delay, an event capture, an event tap).
const EVENTS: Work = Work {
    fixed: 100,
    per_frame: 0,
};

/// The estimated work of `op`, whose node ops run units of `shapes` (by
/// plan unit index).
pub(crate) fn op_work<A, E>(
    op: &Op<A, E>,
    shapes: &TableSlice<PlanUnitId, Shape>,
    sum_lanes: &[Option<crate::EdgeId>],
) -> Work {
    match *op {
        Op::MainInput { .. }
        | Op::Delay { .. }
        | Op::Output { .. }
        | Op::Capture { .. }
        | Op::Tap { .. } => COPY,
        Op::Meter { srcs, .. } => meter_work(u64::from(srcs.len)),
        Op::Sum { srcs, .. } => sum_work(u64::from(srcs.len), 0),
        // A driven lane also runs the param step per frame (one more unit
        // each for its values and its clamp), over the weight of a plain one.
        Op::WeightedSum {
            srcs, lanes, gains, ..
        } => sum_work(
            u64::from(srcs.len),
            sum_lanes[lanes.range()].iter().flatten().count() as u64 + u64::from(gains.len),
        ),
        Op::EventDelay { .. }
        | Op::EventMerge { .. }
        | Op::EventCapture { .. }
        | Op::EventTap { .. } => EVENTS,
        Op::Node { unit, .. } => node_work(&shapes.as_slice()[unit as usize]),
    }
}

/// A node's estimated work: its declared class, per channel of its wider
/// side (at least one: an event-only node still runs per frame).
pub(crate) fn node_work(shape: &Shape) -> Work {
    let (fixed, per) = shape.cost.weights();
    let channels = shape
        .audio_in()
        .count()
        .max(shape.audio_out().count())
        .max(1) as u64;
    Work {
        fixed,
        per_frame: per * channels,
    }
}
