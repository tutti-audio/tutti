//! Compiler assembly: the passes' results as one [`Plan`].

use crate::compile::coarsen::Tasks;
use crate::compile::colour::Coloured;
use crate::compile::emit::Emitted;
use crate::compile::ids::Table;
use crate::compile::ids::{Id, PlanUnitId};
use crate::compile::lower::Lowered;
use crate::compile::plan::{Csr, FeedbackSpec, NodeTables, Plan, PlanUnit};
use crate::compile::shapes::Units;
use crate::compile::timing::Timing;
use crate::model::spec::{EventInputPort, EventOutputPort, ValidGraph};
use crate::node::Prepare;

/// The results of every pass before assembly.
pub(super) struct Passes<'a> {
    pub(super) graph: &'a ValidGraph,
    pub(super) prepare: &'a Prepare,
    pub(super) checked: &'a Units,
    pub(super) order: &'a [PlanUnitId],
    pub(super) timing: Timing,
    pub(super) emitted: Emitted,
    pub(super) coloured: Coloured,
    pub(super) lowered: Lowered,
    pub(super) tasks: Tasks,
    pub(super) units: Table<PlanUnitId, PlanUnit>,
    pub(super) store_len: u32,
}

/// Builds the [`Plan`] from the passes' results, each field once.
///
/// Takes the [`Passes`]; returns the plan, which holds what the passes agreed
/// on: its ops index the slot lists, its tables are the ones its ops name,
/// and its units are the placed ones.
pub(super) fn assemble(passes: Passes<'_>) -> Plan {
    let Passes {
        graph,
        prepare,
        checked,
        order,
        timing,
        emitted,
        coloured,
        lowered,
        tasks,
        units,
        store_len,
    } = passes;
    let nodes = NodeTables::lower(
        lowered.ops.as_slice(),
        &lowered.audio_list,
        &lowered.event_list,
        &units,
    );
    Plan {
        prepare: *prepare,
        ops: lowered.ops,
        audio_list: lowered.audio_list,
        event_list: lowered.event_list,
        op_succ: Csr::from_ids(&coloured.succ),
        tasks: tasks.tasks,
        task_ops: tasks.task_ops,
        task_succ: tasks.succ,
        task_activation: tasks.activation,
        audio_slots: coloured.audio_slots.get(),
        event_slots: coloured.event_slots.get(),
        event_slot_capacity: coloured.event_slot_capacity,
        audio_feedback: emitted
            .audio_feedback
            .iter_enumerated()
            .map(|(f, &key)| FeedbackSpec {
                key,
                slot: f.slot().get(),
            })
            .collect(),
        event_feedback: emitted
            .event_feedback
            .iter_enumerated()
            .map(|(f, &key)| FeedbackSpec {
                key,
                slot: f.slot().get(),
            })
            .collect(),
        delays: emitted.delays,
        taps: emitted.taps,
        event_taps: emitted.event_taps,
        meters: emitted.meters,
        event_edges: {
            let mut v: Vec<(EventOutputPort, EventInputPort)> = graph
                .events()
                .iter()
                .flat_map(|(&at, edges)| edges.iter().map(move |e| (e.from(), at)))
                .collect();
            v.sort();
            v
        },
        units,
        store_len,
        order: order.iter().map(|&n| checked.keys[n]).collect(),
        main_inputs: graph.topology().inputs.count(),
        compensation: timing.compensation,
        main_output_id: crate::MainOutputId::GENESIS,
        main_output_bank: graph.main_output_bank(),
        rendered_outputs: timing.rendered_outputs,
        total_latency: timing.total,
        bank_total: timing.bank_total,
        audio_values: lowered.audio_values,
        event_values: lowered.event_values,
        value_readers: lowered.value_readers,
        param_ports: lowered.param_ports,
        gain_ports: lowered.gain_ports,
        sum_lanes: lowered.sum_lanes,
        nodes,
    }
}
