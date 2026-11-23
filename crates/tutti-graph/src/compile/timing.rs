//! Compiler pass 4: when each unit's inputs arrive and its outputs depart, and
//! the output time every main output channel is aligned to.

use crate::compile::ids::{PlanUnitId, Table};
use crate::compile::plan::Plan;
use crate::compile::shapes::Units;
use crate::model::spec::ValidGraph;
use crate::node::Prepare;
use tutti_units::{Latency, Samples};

/// The latency solve of a graph: one arrival and one departure per unit, and
/// the output times of the main output channels.
///
/// `departure = arrival + own latency` and `arrival` is the largest departure
/// among the unit's sources (event, param and gain sources included), so every
/// source of a merge point is delayed by its own gap to the arrival.
pub(super) struct Timing {
    /// When each unit's inputs are all present.
    pub(super) arrival: Table<PlanUnitId, Latency>,
    /// When each unit's outputs leave it.
    pub(super) departure: Table<PlanUnitId, Latency>,
    /// The graph's total latency: the output time of the main output.
    pub(super) total: Latency,
    /// The output time of an old main output's channels (see
    /// [`ValidGraph::main_output_bank`]); `total` when there is none.
    pub(super) bank_total: Latency,
    /// The first channel of an old main output, when one is carried.
    bank_base: Option<usize>,
    /// How many channels the plan renders, old main output's included.
    pub(super) rendered_outputs: usize,
    /// How many channels the current main output has.
    pub(super) main_output_width: usize,
    /// The delay each channel of the current main output is aligned by.
    pub(super) compensation: Vec<Samples>,
}

impl Timing {
    /// Whether `channel` is one of an old main output's, heard at `bank_total`.
    pub(super) fn in_bank(&self, channel: u16) -> bool {
        self.bank_base
            .is_some_and(|base| usize::from(channel) >= base)
    }

    /// The output time `channel` is aligned to.
    pub(super) fn output_time(&self, channel: u16) -> Latency {
        if self.in_bank(channel) {
            self.bank_total
        } else {
            self.total
        }
    }
}

/// Solves the latency of `graph` over `units`.
///
/// Takes the graph, its checked units, the `Prepare` the plan is for and the
/// plan it replaces; returns the [`Timing`] with one row per unit. An old
/// main output's channels are aligned to the output time they were heard at:
/// the previous plan's total, or, for a plan that continues the previous
/// plan's crossfade, the alignment that plan kept; with no previous plan it is
/// the new total.
pub(super) fn solve(
    graph: &ValidGraph,
    units: &Units,
    prepare: &Prepare,
    prev: Option<&Plan>,
) -> Timing {
    // One solve: `crate::delays` over the checked graph. The compiler places
    // its delays itself (event and param edges included) from the solve's
    // arrivals.
    let solved = crate::delays(graph, prepare.sample_rate());
    let arrival: Table<PlanUnitId, Latency> = units
        .keys
        .iter()
        .map(|&k| Latency::new(solved.arrival(k)))
        .collect();
    let departure: Table<PlanUnitId, Latency> = units
        .keys
        .iter()
        .map(|&k| Latency::new(solved.departure(k)))
        .collect();
    let total = Latency::new(solved.compensation().total());
    let rendered_outputs = graph.topology().outputs.len();
    let bank_total = match (graph.main_output_bank(), prev) {
        (Some(bank), Some(p)) if p.main_output_bank.is_some_and(|b| b.old == bank.old) => {
            p.bank_total
        }
        (_, Some(p)) => p.total_latency,
        (_, None) => total,
    };
    let main_output_width = graph
        .main_output_bank()
        .map_or(rendered_outputs, |b| b.base);
    Timing {
        arrival,
        departure,
        total,
        bank_total,
        bank_base: graph.main_output_bank().map(|b| b.base),
        rendered_outputs,
        main_output_width,
        compensation: (0..main_output_width)
            .map(|ch| solved.compensation().for_channel(ch))
            .collect(),
    }
}
