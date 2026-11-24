//! The plan verifier (compiler step 8).
//!
//! It re-derives every slot access from the **ops** — what the executor will
//! actually do — and checks it against the op DAG, trusting neither the
//! colouring nor the value table it is checking. What the typed ids and
//! tables already hold (an id of one kind never names another's table, and an
//! id or span the compiler took from a table of this plan names a row or run
//! of it) is not checked again. What they cannot hold is the length of
//! tables that are separate (the per-task tables), and that is checked.
//!
//! 1. The zero / empty slots and the feedback slots are never written by an
//!    op (the executor fills feedback slots before the block), and every
//!    feedback delay is fed by exactly one capture.
//! 2. Two ops that touch the same slot, at least one of them writing, are
//!    ordered by the DAG — so no parallel schedule can run them together.
//! 3. Every read of a coloured slot is of a value whose writer happens-before
//!    the reader, with no other write to that slot possibly in between.
//! 4. An op never writes one slot twice, and never writes a slot it reads
//!    unless the op is the declared in-place form; an in-place bit on a node
//!    is only set where its input and output slot really are one.
//! 5. **Tasks** partition the ops; within a task the ops increase — and
//!    every op edge runs to a later op (checked with the DAG), so no task
//!    runs a dependency backwards, though it may hold ops the DAG does not
//!    order (packed siblings); every op edge between two tasks appears in
//!    the task successor lists; each task's activation count equals its number of
//!    distinct predecessor tasks; the task graph is acyclic.
//! 6. **Tables**: every delay index is used by exactly one op, every unit by
//!    exactly one `Node` op, and no two units share a store index. Every
//!    event slot holds what is written into it: a node output what the
//!    node's shape declares, a delay output and a feedback slot all their
//!    FIFO can hold at the source's declared rate, a merge the sum of its
//!    inputs' (so nothing past a writer can refuse or hold back an event).
//! 7. **Params**: each node op's driven params are in strict port order,
//!    each a param its unit declares, and each port's event source is a
//!    read of the op (rules 2–4 cover it like an input). A port has one
//!    source by construction: its value replaces the base.
//! 8. **Fades** ([`verify_fades`], on a plan *and the delta that installs
//!    it*): a crossfade's outgoing unit runs on the node op's own input slots
//!    and the blend writes its own output slots, so rules 1–4 already cover
//!    every slot a fade touches — its only extra buffer is the unit's own
//!    scratch, outside the arena, never shared. What a fade adds is a claim
//!    about *two* units under one op: each fade names a key the delta
//!    replaces (once), and the unit it fades from was compiled with the
//!    same ports, latency, in-place acceptance, event resolution, event
//!    capacity and declared params as the
//!    one it fades to — so the op, its PDC and its borrows are right for
//!    both, and the scratch sized from the plan fits the outgoing unit.
//!
//! `compile` runs it in every debug build, and a test calls it directly on
//! every random graph.

use crate::compile::ids::{
    AudioSlotId, DelayId, EventSlotId, OpId, PlanUnitId, Table, TableSlice, TaskId,
};
use crate::compile::plan::{
    Delta, EMPTY_SLOT, EventSlotCapacity, Op, ParamPortOp, Plan, Span, ZERO_SLOT,
};
use crate::node::MAX_PORTS;
use crate::node::io::PortKind;

use super::colour::Reach;

/// What the verifier found wrong.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct VerifyError(pub String);

#[derive(Clone, Copy, Debug)]
struct Access {
    kind: PortKind,
    /// The position of the slot in the arena of its kind.
    slot: usize,
    write: bool,
}

fn accesses(plan: &Plan, op: &Op<AudioSlotId, EventSlotId>) -> Vec<Access> {
    let a = |slot: AudioSlotId, write| Access {
        kind: PortKind::Audio,
        slot: slot.index(),
        write,
    };
    let e = |slot: EventSlotId, write| Access {
        kind: PortKind::Event,
        slot: slot.index(),
        write,
    };
    match *op {
        Op::MainInput { dst, .. } => vec![a(dst, true)],
        Op::Delay { src, dst, .. } => vec![a(src, false), a(dst, true)],
        Op::Sum { srcs, dst } => plan
            .audio_slots(srcs)
            .iter()
            .map(|&s| a(s, false))
            .chain([a(dst, true)])
            .collect(),
        Op::WeightedSum {
            srcs, dst, gains, ..
        } => plan
            .audio_slots(srcs)
            .iter()
            .map(|&s| a(s, false))
            .chain([a(dst, true)])
            // The driven lanes' event sources: reads of the op.
            .chain(
                plan.program.gain_ports[gains.range()]
                    .iter()
                    .map(|g| e(g.slot, false)),
            )
            .collect(),
        Op::EventDelay { src, dst, .. } => vec![e(src, false), e(dst, true)],
        Op::EventMerge { srcs, dst, .. } => plan
            .event_slots(srcs)
            .iter()
            .map(|&s| e(s, false))
            .chain([e(dst, true)])
            .collect(),
        Op::Node {
            audio_in,
            audio_out,
            event_in,
            event_out,
            params,
            ..
        } => plan
            .audio_slots(audio_in)
            .iter()
            .map(|&s| a(s, false))
            .chain(plan.audio_slots(audio_out).iter().map(|&s| a(s, true)))
            .chain(plan.event_slots(event_in).iter().map(|&s| e(s, false)))
            .chain(plan.event_slots(event_out).iter().map(|&s| e(s, true)))
            .chain(param_reads(plan, params).map(|s| e(s, false)))
            .collect(),
        Op::Output { src, .. } => vec![a(src, false)],
        Op::Tap { srcs, .. } => plan
            .audio_slots(srcs)
            .iter()
            .map(|&s| a(s, false))
            .collect(),
        Op::EventTap { src, .. } => vec![e(src, false)],
        Op::Meter { srcs, .. } => plan
            .audio_slots(srcs)
            .iter()
            .map(|&s| a(s, false))
            .collect(),
        // A capture feeds a delay's state, not a slot.
        Op::Capture { src, .. } => vec![a(src, false)],
        Op::EventCapture { src, .. } => vec![e(src, false)],
    }
}

/// Every event slot a node op's driven params read.
fn param_reads(
    plan: &Plan,
    params: Span<ParamPortOp<EventSlotId>>,
) -> impl Iterator<Item = EventSlotId> + '_ {
    plan.program.param_ports[params.range()]
        .iter()
        .map(|p| p.slot)
}

/// Each sum op adds at least two addends and at most [`MAX_PORTS`] (a wider
/// fan-in is a chain of ops): one is an alias the
/// compiler emits no op for, and the executor's request table holds no more.
/// A weighted sum may add one (an edge with a gain entry is never an alias),
/// and lists one lane per addend, within `sum_lanes`, with at least one of
/// them weighted: an op with none is a plain sum. That its write may alias
/// only the first read is rule 4's; that each read is of a written value,
/// rule 3's.
fn verify_sums(plan: &Plan) -> Result<(), VerifyError> {
    // Driven lanes the ops name; every entry of `gain_ports` must be one.
    let mut covered = 0usize;
    for (i, op) in plan.program.ops.iter().enumerate() {
        match *op {
            Op::Sum { srcs, .. } if !(2..=MAX_PORTS).contains(&srcs.len()) => {
                return Err(VerifyError(format!(
                    "sum op {i} adds {} addends, not 2 to {MAX_PORTS}",
                    srcs.len()
                )));
            }
            Op::WeightedSum {
                srcs, lanes, gains, ..
            } => {
                if !(1..=MAX_PORTS).contains(&srcs.len()) {
                    return Err(VerifyError(format!(
                        "weighted sum op {i} adds {} addends, not 1 to {MAX_PORTS}",
                        srcs.len()
                    )));
                }
                if lanes.len() != srcs.len() {
                    return Err(VerifyError(format!(
                        "weighted sum op {i} has {} lanes for {} addends",
                        lanes.len(),
                        srcs.len()
                    )));
                }
                let list = &plan.program.sum_lanes[lanes.range()];
                if list.iter().all(Option::is_none) {
                    return Err(VerifyError(format!(
                        "weighted sum op {i} has no weighted lane (a plain sum is `Op::Sum`)"
                    )));
                }
                let driven = &plan.program.gain_ports[gains.range()];
                for (n, g) in driven.iter().enumerate() {
                    if n > 0 && driven[n - 1].lane >= g.lane {
                        return Err(VerifyError(format!(
                            "weighted sum op {i}'s driven lanes are not in strict lane order"
                        )));
                    }
                    if list.get(usize::from(g.lane)).is_none_or(Option::is_none) {
                        return Err(VerifyError(format!(
                            "weighted sum op {i} drives lane {}, which is not a weighted lane",
                            g.lane
                        )));
                    }
                }
                covered += driven.len();
            }
            _ => {}
        }
    }
    if covered != plan.program.gain_ports.len() {
        return Err(VerifyError("gain ports no weighted sum names".into()));
    }
    Ok(())
}

/// Rule 7: each node op's driven params.
fn verify_params(plan: &Plan) -> Result<(), VerifyError> {
    let mut covered = 0usize;
    for op in &plan.program.ops {
        let Op::Node { unit, params, .. } = *op else {
            continue;
        };
        let bad = |what: String| VerifyError(format!("unit {unit}'s params {what}"));
        let ports = &plan.program.param_ports[params.range()];
        if params.range().start != covered {
            return Err(bad("are not the next run of the table".into()));
        }
        covered += ports.len();
        let declared = plan.units[unit].shape.params;
        for (i, p) in ports.iter().enumerate() {
            if i > 0 && ports[i - 1].port >= p.port {
                return Err(bad("are not in strict port order".into()));
            }
            // The node's mix is the one port nothing declares: last, at the
            // sentinel, and addressed by its reserved address.
            if p.is_mix() {
                if p.param != crate::NodeMix::ADDR {
                    return Err(bad(format!(
                        "name {:?} at the mix port, which is {:?}'s",
                        p.param,
                        crate::NodeMix::ADDR
                    )));
                }
                continue;
            }
            if declared.as_slice().get(p.port as usize) != Some(&p.param) {
                return Err(bad(format!(
                    "modulate {:?} at port {}, which the unit does not declare there",
                    p.param, p.port
                )));
            }
        }
    }
    if covered != plan.program.param_ports.len() {
        return Err(VerifyError("param ports no node op names".into()));
    }
    Ok(())
}

/// Checks `plan` (see the [module docs](self) for the rules).
pub fn verify(plan: &Plan) -> Result<(), VerifyError> {
    verify_sums(plan)?;
    for (b, row) in plan.program.deps.to_table::<OpId>().iter_enumerated() {
        if row.iter().any(|&a| a >= b) {
            return Err(VerifyError(format!(
                "op {b} has a dependency that does not precede it in the serial order"
            )));
        }
    }
    let reach = Reach::new(&plan.program.deps);
    let ordered = |a: OpId, b: OpId| a == b || reach.before(a, b) || reach.before(b, a);

    let acc: Vec<Vec<Access>> = plan
        .program
        .ops
        .iter()
        .map(|op| accesses(plan, op))
        .collect();
    let audio_fixed = 1 + plan.program.audio_feedback.len();
    let event_fixed = 1 + plan.program.event_feedback.len();

    // Rule 1 and rule 4.
    for (i, (op, list)) in plan.program.ops.iter().zip(&acc).enumerate() {
        for x in list.iter().filter(|x| x.write) {
            let reserved = match x.kind {
                PortKind::Audio => x.slot == ZERO_SLOT.index(),
                PortKind::Event => x.slot == EMPTY_SLOT.index(),
            };
            if reserved {
                return Err(VerifyError(format!(
                    "op {i} writes the {:?} null slot",
                    x.kind
                )));
            }
            let fixed = match x.kind {
                PortKind::Audio => audio_fixed,
                PortKind::Event => event_fixed,
            };
            if x.slot < fixed {
                return Err(VerifyError(format!(
                    "op {i} writes feedback slot {}",
                    x.slot
                )));
            }
            let writes = list
                .iter()
                .filter(|y| y.write && y.kind == x.kind && y.slot == x.slot)
                .count();
            if writes > 1 {
                return Err(VerifyError(format!(
                    "op {i} writes {:?} slot {} twice",
                    x.kind, x.slot
                )));
            }
            let reads_it = list
                .iter()
                .any(|y| !y.write && y.kind == x.kind && y.slot == x.slot);
            let declared = match *op {
                Op::Delay { src, dst, .. } => src == dst,
                // The in-place form: the written slot is the first addend's,
                // and no other addend's.
                Op::Sum { srcs, dst } | Op::WeightedSum { srcs, dst, .. } => {
                    let ins = plan.audio_slots(srcs);
                    ins.first() == Some(&dst) && ins.iter().filter(|&&t| t == dst).count() == 1
                }
                Op::Node {
                    audio_in,
                    audio_out,
                    in_place,
                    ..
                } if x.kind == PortKind::Audio => {
                    let ins = plan.audio_slots(audio_in);
                    let outs = plan.audio_slots(audio_out);
                    outs.iter().enumerate().any(|(c, &s)| {
                        s.index() == x.slot
                            && in_place.get(c)
                            && ins.get(c) == Some(&s)
                            && ins.iter().filter(|&&t| t == s).count() == 1
                    })
                }
                _ => false,
            };
            if reads_it && !declared {
                return Err(VerifyError(format!(
                    "op {i} reads and writes {:?} slot {} without declaring it in place",
                    x.kind, x.slot
                )));
            }
        }
    }

    // Rule 2: per slot, every pair (a writer, any toucher) is ordered.
    for kind in [PortKind::Audio, PortKind::Event] {
        let slots = match kind {
            PortKind::Audio => plan.slots(PortKind::Audio),
            PortKind::Event => plan.slots(PortKind::Event),
        } as usize;
        let mut touch: Vec<Vec<(OpId, bool)>> = vec![Vec::new(); slots];
        for (op, list) in plan.program.ops.ids().zip(&acc) {
            for x in list.iter().filter(|x| x.kind == kind) {
                touch[x.slot].push((op, x.write));
            }
        }
        for (s, t) in touch.iter().enumerate() {
            for (j, &(a, aw)) in t.iter().enumerate() {
                for &(b, bw) in &t[j + 1..] {
                    if (aw || bw) && !ordered(a, b) {
                        return Err(VerifyError(format!(
                            "ops {a} and {b} may run concurrently and share {kind:?} slot {s}"
                        )));
                    }
                }
            }
        }

        // Rule 3: every read of a coloured slot reads its intended value.
        let (values, fixed): (Vec<(OpId, Span<OpId>, usize)>, usize) = match kind {
            PortKind::Audio => (
                plan.program
                    .audio_values
                    .iter()
                    .map(|v| (v.writer, v.readers, v.slot.index()))
                    .collect(),
                audio_fixed,
            ),
            PortKind::Event => (
                plan.program
                    .event_values
                    .iter()
                    .map(|v| (v.writer, v.readers, v.slot.index()))
                    .collect(),
                event_fixed,
            ),
        };
        let mut covered: Vec<(OpId, usize)> = Vec::new();
        for &(writer, readers, slot) in &values {
            for &r in &plan.program.value_readers[readers.range()] {
                covered.push((r, slot));
                if !reach.before(writer, r) {
                    return Err(VerifyError(format!(
                        "op {r} reads {kind:?} slot {slot} before its writer op {writer} runs"
                    )));
                }
                for &(w, is_write) in &touch[slot] {
                    if is_write
                        && w != writer
                        && w != r
                        && reach.before(writer, w)
                        && reach.before(w, r)
                    {
                        return Err(VerifyError(format!(
                            "op {w} overwrites {kind:?} slot {slot} between its writer {writer} and reader {r}"
                        )));
                    }
                }
            }
        }
        for (op, list) in plan.program.ops.ids().zip(&acc) {
            for x in list
                .iter()
                .filter(|x| x.kind == kind && !x.write && x.slot >= fixed)
            {
                if !covered.contains(&(op, x.slot)) {
                    return Err(VerifyError(format!(
                        "op {op} reads {kind:?} slot {} that no value delivers to it",
                        x.slot
                    )));
                }
            }
        }
    }

    // Rule 1, second half: one capture per feedback delay.
    let mut audio = vec![0usize; plan.program.audio_feedback.len()];
    let mut events = vec![0usize; plan.program.event_feedback.len()];
    for op in &plan.program.ops {
        match *op {
            Op::Capture { feedback, .. } => audio[feedback.index()] += 1,
            Op::EventCapture { feedback, .. } => events[feedback.index()] += 1,
            _ => {}
        }
    }
    for (kind, captures) in [(PortKind::Audio, audio), (PortKind::Event, events)] {
        if let Some(f) = captures.iter().position(|&c| c != 1) {
            return Err(VerifyError(format!(
                "{kind:?} feedback {f} is captured {} times",
                captures[f]
            )));
        }
    }

    verify_params(plan)?;
    verify_in_place(plan)?;
    verify_tasks(plan)?;
    verify_tables(plan)?;
    Ok(())
}

/// Rule 4, second half: an in-place bit means one slot.
fn verify_in_place(plan: &Plan) -> Result<(), VerifyError> {
    for (i, op) in plan.program.ops.iter().enumerate() {
        if let Op::Node {
            audio_in,
            audio_out,
            in_place,
            ..
        } = *op
        {
            let ins = plan.audio_slots(audio_in);
            let outs = plan.audio_slots(audio_out);
            for c in 0..64 {
                if in_place.get(c) && (ins.get(c).is_none() || ins.get(c) != outs.get(c)) {
                    return Err(VerifyError(format!(
                        "op {i} marks channel {c} in place but its input and output slots differ"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Rule 5: the task structure a parallel executor will run.
fn verify_tasks(plan: &Plan) -> Result<(), VerifyError> {
    let n = plan.program.ops.len();
    let tasks = plan.tasks.len();
    let mut found: Table<OpId, Option<TaskId>> = Table::filled(n, None);
    for (t, span) in plan.tasks.iter_enumerated() {
        let ops = &plan.task_ops[span.range()];
        for (j, &op) in ops.iter().enumerate() {
            let slot = &mut found[op];
            if slot.is_some() {
                return Err(VerifyError(format!("op {op} is in two tasks")));
            }
            *slot = Some(t);
            if j > 0 && ops[j - 1] >= op {
                return Err(VerifyError(format!(
                    "task {t} runs op {} then op {op}, out of ascending order",
                    ops[j - 1]
                )));
            }
        }
    }
    let Some(task_of) = found
        .iter()
        .copied()
        .collect::<Option<Table<OpId, TaskId>>>()
    else {
        let op = found.iter().position(Option::is_none);
        return Err(VerifyError(format!("op {op:?} is in no task")));
    };
    // The tasks, their successors and their activation counts are three
    // tables of one length in a plan the compiler built; ids and spans cannot
    // say so, so it is checked.
    if plan.task_succ.rows() != tasks || plan.task_activation.len() != tasks {
        return Err(VerifyError("task tables have the wrong length".into()));
    }
    for b in plan.program.ops.ids() {
        for &a in plan.program.deps.of(b) {
            let (ta, tb) = (task_of[a], task_of[b]);
            if ta != tb && !plan.task_succ.of(ta).contains(&tb) {
                return Err(VerifyError(format!(
                    "op edge {a} → {b} crosses task {ta} → {tb}, which task_succ lacks"
                )));
            }
        }
    }
    let mut indeg: Table<TaskId, u32> = Table::filled(tasks, 0);
    for t in plan.tasks.ids() {
        let mut seen = std::collections::BTreeSet::new();
        for &u in plan.task_succ.of(t) {
            if !seen.insert(u) || u == t {
                return Err(VerifyError(format!("task {t} has a bad successor {u}")));
            }
            indeg[u] += 1;
        }
    }
    if indeg != plan.task_activation {
        return Err(VerifyError(
            "task activation counts differ from the distinct in-degrees".into(),
        ));
    }
    // Kahn over tasks: every task must come out, or the task graph cycles.
    let mut left = indeg.clone();
    let mut ready: Vec<TaskId> = left.ids().filter(|&t| left[t] == 0).collect();
    let mut out = 0;
    while let Some(t) = ready.pop() {
        out += 1;
        for &u in plan.task_succ.of(t) {
            left[u] -= 1;
            if left[u] == 0 {
                ready.push(u);
            }
        }
    }
    if out != tasks {
        return Err(VerifyError("the task graph has a cycle".into()));
    }
    Ok(())
}

/// Rule 6: the index tables.
fn verify_tables(plan: &Plan) -> Result<(), VerifyError> {
    let mut delay_uses: Table<DelayId, usize> = Table::filled(plan.program.delays.len(), 0);
    let mut unit_uses: Table<PlanUnitId, usize> = Table::filled(plan.units.len(), 0);
    for op in &plan.program.ops {
        let d = match *op {
            Op::Delay { delay, .. } | Op::EventDelay { delay, .. } => Some(delay),
            Op::Output { delay, .. } => delay,
            Op::Node { unit, .. } => {
                unit_uses[unit] += 1;
                None
            }
            _ => None,
        };
        if let Some(d) = d {
            delay_uses[d] += 1;
        }
    }
    if let Some((d, uses)) = delay_uses.iter_enumerated().find(|&(_, &c)| c != 1) {
        return Err(VerifyError(format!("delay {d} is used by {uses} ops")));
    }
    if let Some((u, uses)) = unit_uses.iter_enumerated().find(|&(_, &c)| c != 1) {
        return Err(VerifyError(format!("unit {u} is run by {uses} ops")));
    }
    // Every event slot holds what is written into it, or an event could be
    // refused past the writer (a note-off least of all), or delivered a
    // block late. The need is derived from the *values* (a slot can be
    // shared with a larger value, so slot capacities would overstate it): a
    // node output what its shape declares, a delay output and a feedback
    // slot all their FIFO can hold at the source's declared rate, a merge
    // the sum of its inputs.
    let block = plan.prepare.max_block().get();
    let slot_cap = |s: EventSlotId| plan.program.sizing.events[s];
    for (f, spec) in plan.program.event_feedback.iter_enumerated() {
        let crate::compile::plan::FeedbackKey::Event { from, delay, .. } = spec.key else {
            return Err(VerifyError(format!(
                "event feedback {f} is not keyed as events"
            )));
        };
        let need = EventSlotCapacity::fifo(plan.event_port_capacity(from), delay, block);
        if !slot_cap(spec.slot).holds(need) {
            return Err(VerifyError(format!(
                "event feedback slot {} holds {:?}, its FIFO {need:?}",
                spec.slot,
                slot_cap(spec.slot)
            )));
        }
    }
    let mut op_need: Table<OpId, EventSlotCapacity> =
        Table::filled(plan.program.ops.len(), EventSlotCapacity::NONE);
    for (m, op) in plan.program.ops.iter_enumerated() {
        // What op `m` reads from slot `s`, priced by its writer.
        let read_need = |s: EventSlotId, op_need: &TableSlice<OpId, EventSlotCapacity>| {
            if s == EMPTY_SLOT {
                return Ok(EventSlotCapacity::NONE);
            }
            if let Some(spec) = plan.program.event_feedback.iter().find(|f| f.slot == s) {
                return match spec.key {
                    crate::compile::plan::FeedbackKey::Event { from, delay, .. } => Ok(
                        EventSlotCapacity::fifo(plan.event_port_capacity(from), delay, block),
                    ),
                    _ => Err(VerifyError(format!("event slot {s} is not event feedback"))),
                };
            }
            plan.program
                .event_values
                .iter()
                .find(|v| v.slot == s && plan.program.value_readers[v.readers.range()].contains(&m))
                .map(|v| op_need[v.writer])
                .ok_or_else(|| VerifyError(format!("op {m} reads event slot {s} of no value")))
        };
        op_need[m] = match *op {
            Op::Node { unit, .. } => EventSlotCapacity::port(plan.units[unit].shape.event_capacity),
            Op::EventDelay { delay, .. } => {
                let d = &plan.program.delays[delay];
                let Some(from) = d.key.event_origin() else {
                    return Err(VerifyError(format!(
                        "event delay {delay} is not keyed as events"
                    )));
                };
                plan.event_tap_fifo(from, d.len, block)
            }
            Op::EventMerge { srcs, bound, .. } => {
                let mut need = EventSlotCapacity::NONE;
                for &s in plan.event_slots(srcs) {
                    need = need.plus(read_need(s, &op_need)?);
                }
                need.bounded(bound.map(|b| b.capacity))
            }
            _ => EventSlotCapacity::NONE,
        };
    }
    for v in &plan.program.event_values {
        let need = op_need[v.writer];
        if !slot_cap(v.slot).holds(need) {
            return Err(VerifyError(format!(
                "event slot {} holds {:?}, but op {} writes {need:?} into it",
                v.slot,
                slot_cap(v.slot),
                v.writer
            )));
        }
    }
    let mut idx: Vec<u32> = plan.units.iter().map(|u| u.idx.0).collect();
    idx.sort_unstable();
    if idx.windows(2).any(|w| w[0] == w[1]) {
        return Err(VerifyError("two units share a store index".into()));
    }
    Ok(())
}

/// Rule 8: check the fades `delta` carries into `plan`, from `prev` (the plan
/// running before it). See rule 8 in the module docs.
///
/// Written against the two plans' units directly — not through
/// `Editor::replace`'s check — so a delta built by hand (through
/// `Editor::package`) gets the same scrutiny as one the editor built.
pub fn verify_fades(prev: Option<&Plan>, plan: &Plan, delta: &Delta) -> Result<(), VerifyError> {
    // A cut names a unit both plans hold at one generation and index: a
    // kept unit, not one the delta places or retires.
    for c in &delta.cuts {
        let held = |p: &Plan| {
            p.units
                .iter()
                .any(|u| u.key == c.key && u.generation == c.generation && u.idx == c.idx)
        };
        if !(prev.is_some_and(held) && held(plan)) {
            return Err(VerifyError(format!(
                "node {} is cut, but is not a unit both plans keep",
                c.key.0
            )));
        }
    }
    for (i, &(key, _)) in delta.fades.iter().enumerate() {
        if delta.fades[..i].iter().any(|&(k, _)| k == key) {
            return Err(VerifyError(format!("node {} fades twice", key.0)));
        }
        let Some(&(old, new)) = delta.replace.iter().find(|(_, n)| n.key == key) else {
            return Err(VerifyError(format!(
                "node {} fades, but the delta does not replace it",
                key.0
            )));
        };
        let was = prev
            .and_then(|p| {
                p.units
                    .iter()
                    .find(|u| u.key == key && u.generation == old.generation)
            })
            .map(|u| u.shape);
        let now = plan
            .units
            .iter()
            .find(|u| u.key == key && u.generation == new.generation)
            .map(|u| u.shape);
        let (Some(was), Some(now)) = (was, now) else {
            return Err(VerifyError(format!(
                "node {} fades between units the plans do not hold",
                key.0
            )));
        };
        let same = was.audio_in() == now.audio_in()
            && was.audio_out() == now.audio_out()
            && was.event_in == now.event_in
            && was.event_out == now.event_out
            && was.latency == now.latency
            && was.in_place == now.in_place
            && was.event_resolution == now.event_resolution
            && was.event_capacity == now.event_capacity
            && was.event_input_capacity == now.event_input_capacity
            && was.params == now.params;
        if !same {
            return Err(VerifyError(format!(
                "node {} fades from {was:?} to {now:?}: only the tail may differ",
                key.0
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::ids::{AudioValueId, PlanUnitId, TaskId};
    use crate::compile::plan::Csr;

    /// Task rows with no successors, `tasks` of them.
    fn no_successors(tasks: usize) -> Csr<TaskId> {
        Csr::from_table(&Table::<TaskId, Vec<TaskId>>::filled(tasks, Vec::new()))
    }
    use crate::compile::{Shapes, compile};
    use crate::model::spec::GraphSpec;
    use crate::node::shape::Shape;
    use crate::{NodeKey, Topology};
    use crate::{NodeSpec, OutputPort, PortSource};
    use tutti_buffers::ChannelLayout;

    /// Two independent generators, each to its own output: the ops touching
    /// their values are unordered, so they must never share a slot.
    fn independent_pair() -> Plan {
        let mut t = Topology::default();
        let shapes: Shapes = [NodeKey(1), NodeKey(2)]
            .into_iter()
            .map(|k| {
                t.nodes.insert(
                    k,
                    NodeSpec::new("gen", ChannelLayout::EMPTY, ChannelLayout::MONO),
                );
                (k, Shape::source_unassigned(ChannelLayout::MONO))
            })
            .collect();
        t.set_outputs(vec![
            PortSource::Node(OutputPort::new(NodeKey(1), 0)),
            PortSource::Node(OutputPort::new(NodeKey(2), 0)),
        ]);
        let valid = GraphSpec::new(t).validate().expect("valid");
        compile(
            &valid,
            &shapes,
            &crate::node::Prepare::new(
                tutti_units::SampleRate::SR_48K,
                tutti_buffers::MaxBlock::of(tutti_units::Samples::new(64)),
            ),
            None,
        )
        .expect("compiles")
        .0
    }

    /// A driven graph: generator 1 feeds node 2 (mono in, mono out, in
    /// place, declaring `Cutoff`), whose `Cutoff` node 3's event output
    /// drives; generator 4 feeds a second output on its own and sends
    /// events nothing reads, unordered with node 2. Returns the plan and
    /// node 2's op index.
    fn modulated_plan() -> (Plan, usize) {
        use crate::model::spec::EventOutputPort;
        use crate::node::param::ParamIn;
        use tutti_units::UnitParam;
        let generator = Shape::source_unassigned(ChannelLayout::MONO);
        let sender = Shape::source_unassigned(ChannelLayout::MONO).events(0, 1);
        let thru = Shape::through_unassigned(ChannelLayout::MONO)
            .in_place()
            .params(&[UnitParam::Cutoff]);
        let mut t = Topology::default();
        let mut shapes = Shapes::new();
        for (k, s) in [(1, generator), (2, thru), (3, sender), (4, sender)] {
            t.nodes
                .insert(NodeKey(k), NodeSpec::new("n", s.audio_in(), s.audio_out()));
            shapes.insert(NodeKey(k), s);
        }
        t.edges.insert(
            crate::InputPort::new(NodeKey(2), 0),
            vec![crate::Edge::Direct(PortSource::Node(OutputPort::new(
                NodeKey(1),
                0,
            )))],
        );
        t.set_outputs(vec![
            PortSource::Node(OutputPort::new(NodeKey(2), 0)),
            PortSource::Node(OutputPort::new(NodeKey(4), 0)),
        ]);
        let mut spec = GraphSpec::new(t);
        let _ = spec.connect_param(
            ParamIn {
                node: NodeKey(2),
                param: UnitParam::Cutoff.into(),
            },
            EventOutputPort::new(NodeKey(3), 0),
        );
        let valid = spec.validate().expect("valid");
        let plan = compile(
            &valid,
            &shapes,
            &crate::node::Prepare::new(
                tutti_units::SampleRate::SR_48K,
                tutti_buffers::MaxBlock::of(tutti_units::Samples::new(64)),
            ),
            None,
        )
        .expect("compiles")
        .0;
        let u = plan
            .units
            .iter_enumerated()
            .find(|(_, u)| u.key == NodeKey(2))
            .map(|(id, _)| id)
            .unwrap();
        let op = plan
            .program
            .ops
            .iter()
            .position(|op| matches!(*op, Op::Node { unit, .. } if unit == u))
            .unwrap();
        (plan, op)
    }

    /// Rule 7, and the param reads the other rules see, each reject their
    /// own corruption of a driven plan.
    #[test]
    fn the_verifier_rejects_each_param_corruption() {
        let (good, _) = modulated_plan();
        verify(&good).expect("sound");
        let err = |p: &Plan| verify(p).expect_err("corrupt").0;

        // A port the unit does not declare there.
        let mut bad = good.clone();
        bad.program.param_ports[0].param = tutti_units::UnitParam::Q.into();
        assert!(err(&bad).contains("does not declare"), "{}", err(&bad));

        // A port no node op names.
        let mut bad = good.clone();
        for o in &mut bad.program.ops {
            if let Op::Node { params, .. } = o {
                *params = Span::default();
            }
        }
        bad.nodes
            .recs
            .iter_mut()
            .for_each(|r| r.params = Span::default());
        assert!(
            err(&bad).contains("param ports no node op names"),
            "{}",
            err(&bad)
        );

        // A param read of node 4's event slot, which nothing orders before
        // node 2: a read of a value that may not be written yet.
        let u4 = good
            .units
            .iter_enumerated()
            .find(|(_, u)| u.key == NodeKey(4))
            .map(|(id, _)| id)
            .unwrap();
        let other = good
            .program
            .ops
            .iter()
            .find_map(|o| match *o {
                Op::Node {
                    unit, event_out, ..
                } if unit == u4 => Some(good.program.event_list[event_out.range().start]),
                _ => None,
            })
            .expect("node 4 sends events");
        assert_ne!(other, good.program.param_ports[0].slot);
        let mut bad = good.clone();
        bad.program.param_ports[0].slot = other;
        assert!(verify(&bad).is_err(), "an unordered param read passed");
    }

    /// The verifier is not a rubber stamp: forcing two concurrent values into
    /// one slot — exactly what a serial-liveness colouring would do — is
    /// reported.
    #[test]
    fn the_verifier_rejects_a_slot_shared_by_concurrent_ops() {
        let good = independent_pair();
        verify(&good).expect("the compiler's own plan is sound");

        let mut bad = good.clone();
        let values: Vec<AudioValueId> = bad.program.audio_values.ids().collect();
        let a = bad.program.audio_values[values[0]].slot;
        let b = bad.program.audio_values[values[1]].slot;
        assert_ne!(a, b);
        for s in &mut bad.program.audio_list {
            if *s == b {
                *s = a;
            }
        }
        for op in &mut bad.program.ops {
            if let Op::Output { src, .. } = op
                && *src == b
            {
                *src = a;
            }
        }
        bad.program.audio_values[values[1]].slot = a;
        let err = verify(&bad).expect_err("two concurrent writers share a slot");
        assert!(err.0.contains("concurrently"), "{err}");
    }

    /// A writer that is ordered after a value's writer but may run beside one
    /// of its *readers* is reported — the overlap only a read/write check
    /// sees. `A → B → W` and `A → C`: `W` writing into `A`'s slot follows `A`
    /// but can run while `C` still reads `A`.
    #[test]
    fn the_verifier_rejects_a_write_beside_a_concurrent_read() {
        let (a, b, w, c) = (NodeKey(1), NodeKey(2), NodeKey(3), NodeKey(4));
        let mono = |i: u16| NodeSpec::new("n", ChannelLayout::from_count(i), ChannelLayout::MONO);
        let mut t = Topology::default();
        t.nodes.insert(a, mono(0));
        for k in [b, w, c] {
            t.nodes.insert(k, mono(1));
        }
        let wire = |t: &mut Topology, sink: NodeKey, from: NodeKey| {
            t.edges.insert(
                crate::InputPort::new(sink, 0),
                vec![crate::Edge::Direct(PortSource::Node(OutputPort::new(
                    from, 0,
                )))],
            );
        };
        wire(&mut t, b, a);
        wire(&mut t, w, b);
        wire(&mut t, c, a);
        t.set_outputs(vec![
            PortSource::Node(OutputPort::new(w, 0)),
            PortSource::Node(OutputPort::new(c, 0)),
        ]);
        let shapes: Shapes = t
            .nodes
            .iter()
            .map(|(&k, s)| {
                (
                    k,
                    Shape::buses(
                        crate::Buses::unassigned(s.inputs),
                        crate::Buses::unassigned(s.outputs),
                    ),
                )
            })
            .collect();
        let valid = GraphSpec::new(t).validate().expect("valid");
        let good = compile(
            &valid,
            &shapes,
            &crate::node::Prepare::new(
                tutti_units::SampleRate::SR_48K,
                tutti_buffers::MaxBlock::of(tutti_units::Samples::new(64)),
            ),
            None,
        )
        .expect("compiles")
        .0;
        verify(&good).expect("sound");

        // Move W's output into A's slot.
        let unit = |k| {
            good.units
                .iter_enumerated()
                .find(|(_, u)| u.key == k)
                .map(|(id, _)| id)
                .unwrap()
        };
        let out_slot = |p: &Plan, k| {
            p.program
                .ops
                .iter()
                .find_map(|op| match *op {
                    Op::Node {
                        unit: u, audio_out, ..
                    } if u == unit(k) => Some(audio_out.range().start),
                    _ => None,
                })
                .unwrap()
        };
        let mut bad = good.clone();
        let (ia, iw) = (out_slot(&bad, a), out_slot(&bad, w));
        let (sa, sw) = (bad.program.audio_list[ia], bad.program.audio_list[iw]);
        assert_ne!(sa, sw);
        bad.program.audio_list[iw] = sa;
        for op in &mut bad.program.ops {
            if let Op::Output { src, .. } = op
                && *src == sw
            {
                *src = sa;
            }
        }
        for v in &mut bad.program.audio_values {
            if v.slot == sw {
                v.slot = sa;
            }
        }
        let err = verify(&bad).expect_err("W may overwrite A while C reads it");
        assert!(err.0.contains("concurrently"), "{err}");
    }

    /// Two generators summed at one output channel: the plan holds one
    /// `Op::Sum` of two addends, the first aliased.
    fn summed_pair() -> Plan {
        let mut t = Topology::default();
        let shapes: Shapes = [NodeKey(1), NodeKey(2)]
            .into_iter()
            .map(|k| {
                t.nodes.insert(
                    k,
                    NodeSpec::new("gen", ChannelLayout::EMPTY, ChannelLayout::MONO),
                );
                (k, Shape::source_unassigned(ChannelLayout::MONO))
            })
            .collect();
        t.outputs = vec![
            [NodeKey(1), NodeKey(2)]
                .into_iter()
                .map(|node| PortSource::Node(OutputPort::new(node, 0)))
                .collect(),
        ];
        let valid = GraphSpec::new(t).validate().expect("valid");
        compile(
            &valid,
            &shapes,
            &crate::node::Prepare::new(
                tutti_units::SampleRate::SR_48K,
                tutti_buffers::MaxBlock::of(tutti_units::Samples::new(64)),
            ),
            None,
        )
        .expect("compiles")
        .0
    }

    /// A sum op adds two to `MAX_PORTS` addends, and writes only its first
    /// addend's slot among the slots it reads.
    #[test]
    fn the_verifier_rejects_a_malformed_sum() {
        let good = summed_pair();
        verify(&good).expect("sound");
        let i = good
            .program
            .ops
            .iter_enumerated()
            .find(|(_, o)| matches!(o, Op::Sum { .. }))
            .map(|(i, _)| i)
            .expect("a sum");
        let Op::Sum { srcs, dst } = good.program.ops[i] else {
            unreachable!()
        };
        assert_eq!(srcs.len(), 2);
        assert_eq!(dst, good.program.audio_list[srcs.range().start], "in place");

        // One addend is an alias, never an op.
        let mut bad = good.clone();
        bad.program.ops[i] = Op::Sum {
            srcs: Span::of(srcs.range().start, srcs.range().start + 1),
            dst,
        };
        assert!(verify(&bad).unwrap_err().0.contains("addends"));

        // 65 addends: more than the executor's table holds.
        let mut bad = good.clone();
        let start = bad.program.audio_list.len();
        let first = good.program.audio_list[srcs.range().start];
        bad.program
            .audio_list
            .extend(std::iter::repeat_n(first, 65));
        bad.program.ops[i] = Op::Sum {
            srcs: Span::of(start, start + 65),
            dst,
        };
        assert!(verify(&bad).unwrap_err().0.contains("addends"));

        // A write that aliases the second read, not the first.
        let mut bad = good.clone();
        let second = good.program.audio_list[srcs.range().start + 1];
        bad.program.ops[i] = Op::Sum { srcs, dst: second };
        assert!(verify(&bad).unwrap_err().0.contains("without declaring"));
    }

    /// A read the DAG does not order after its write is reported.
    #[test]
    fn the_verifier_rejects_a_read_before_its_write() {
        let mut bad = independent_pair();
        // Cut every edge: the outputs no longer follow their generators.
        bad.program.deps = Csr::from_table(&Table::<OpId, Vec<OpId>>::filled(
            bad.program.ops.len(),
            Vec::new(),
        ));
        assert!(verify(&bad).is_err());
    }

    #[test]
    fn the_verifier_rejects_each_structural_corruption() {
        let good = independent_pair();
        verify(&good).expect("sound");

        // An in-place bit where the slots differ.
        let mut bad = good.clone();
        for op in &mut bad.program.ops {
            if let Op::Node {
                in_place, audio_in, ..
            } = op
                && audio_in.is_empty()
            {
                // A generator: no input, so any bit is a lie.
                *in_place = in_place.with(0);
                break;
            }
        }
        assert!(verify(&bad).unwrap_err().0.contains("in place"));

        // An op in two tasks: a new one-op task repeating an op, with task
        // tables that account for it, so only the partition rule can object.
        let mut bad = good.clone();
        let dup = bad.task_ops[0];
        let start = bad.task_ops.len();
        bad.task_ops.push(dup);
        bad.tasks.push(Span::of(start, start + 1));
        let mut rows = bad.task_succ.to_table::<TaskId>();
        rows.push(Vec::new());
        bad.task_succ = Csr::from_table(&rows);
        bad.task_activation.push(0);
        assert!(verify(&bad).unwrap_err().0.contains("two tasks"));

        // A cross-task edge missing from task_succ (needs two tasks).
        let mut bad = good.clone();
        assert!(bad.tasks.len() >= 2, "the pair has independent tasks");
        assert!(!bad.program.deps.targets.is_empty());
        bad.task_succ = no_successors(bad.tasks.len());
        bad.task_activation = Table::filled(bad.tasks.len(), 0);
        // Only fails if some edge crossed tasks; the pair's generator → output
        // edges are fused, so add a cross edge by splitting a task.
        let crossing = {
            let mut t = good.clone();
            let span = t.tasks.as_slice()[0];
            if span.len() >= 2 {
                let (start, end) = (span.range().start, span.range().end);
                let mut spans = t.tasks.as_slice().to_vec();
                spans[0] = Span::of(start, start + 1);
                spans.insert(1, Span::of(start + 1, end));
                t.tasks = spans.into();
                t.task_succ = no_successors(t.tasks.len());
                t.task_activation = Table::filled(t.tasks.len(), 0);
            }
            t
        };
        assert!(verify(&crossing).unwrap_err().0.contains("crosses task"));

        // A task's ops out of ascending order (a packed task may hold ops
        // the DAG does not order, so ascending is what keeps the ones it
        // does in order).
        let mut bad = good.clone();
        let t = bad
            .tasks
            .iter_enumerated()
            .find(|(_, s)| s.len() >= 2)
            .map(|(t, _)| t)
            .expect("the pair fuses a chain");
        let r = bad.tasks[t].range();
        bad.task_ops[r].reverse();
        assert!(verify(&bad).unwrap_err().0.contains("ascending"));

        // Activation counts that do not match.
        let mut bad = good.clone();
        let first = bad.task_activation.ids().next().expect("a task");
        bad.task_activation[first] += 1;
        assert!(verify(&bad).unwrap_err().0.contains("activation"));

        // A delay no op uses.
        let mut bad = good.clone();
        bad.program.delays.push(crate::compile::plan::DelaySpec {
            key: crate::compile::plan::DelayKey::Output {
                channel: 0,
                from: PortSource::Zero,
            },
            len: tutti_units::Samples::new(1),
        });
        assert!(verify(&bad).unwrap_err().0.contains("delay 0 is used by 0"));

        // Rule 6: a unit run by two ops (the second node op takes the first's
        // unit, so one unit is run twice and the other not at all).
        let mut bad = good.clone();
        let first = bad
            .program
            .ops
            .iter()
            .find_map(|op| match *op {
                Op::Node { unit, .. } => Some(unit),
                _ => None,
            })
            .expect("the pair has node ops");
        let second = bad
            .program
            .ops
            .iter_mut()
            .filter_map(|op| match op {
                Op::Node { unit, .. } => Some(unit),
                _ => None,
            })
            .nth(1)
            .expect("two node ops");
        *second = first;
        assert!(verify(&bad).unwrap_err().0.contains("is run by 2 ops"));

        // Two units at one store index.
        let mut bad = good.clone();
        let units: Vec<PlanUnitId> = bad.units.ids().collect();
        let idx = bad.units[units[0]].idx;
        bad.units[units[1]].idx = idx;
        assert!(verify(&bad).unwrap_err().0.contains("store index"));
    }

    /// The one past-the-table case a type cannot rule out: the per-task
    /// tables are separate and can differ in length. (An op naming a tap,
    /// meter, unit, delay, feedback or slot past its table, or a span past its
    /// list, cannot be written: ids and spans are minted only by their tables.)
    #[test]
    fn the_verifier_rejects_per_task_tables_of_different_lengths() {
        let good = independent_pair();
        verify(&good).expect("sound");
        let mut bad = good.clone();
        bad.task_activation.push(0);
        assert!(verify(&bad).unwrap_err().0.contains("wrong length"));
        let mut bad = good.clone();
        let mut rows = bad.task_succ.to_table::<TaskId>();
        rows.push(Vec::new());
        bad.task_succ = Csr::from_table(&rows);
        assert!(verify(&bad).unwrap_err().0.contains("wrong length"));
    }

    /// A merge whose slot is smaller than its inputs together is refused —
    /// it could drop a note-off — in either currency: one source declares
    /// 5 events, the other takes the default.
    #[test]
    fn the_verifier_rejects_a_merge_slot_too_small() {
        use crate::model::spec::{EventEdge, EventInputPort, EventOutputPort};
        let mut t = Topology::default();
        let mut shapes = Shapes::new();
        let src = Shape::EMPTY.events(0, 1);
        let declared = src.event_capacity(crate::testing::cap_of(5));
        let sink = Shape::EMPTY.events(1, 0);
        for (k, s) in [(1, declared), (2, src), (3, sink)] {
            t.nodes
                .insert(NodeKey(k), NodeSpec::new("n", s.audio_in(), s.audio_out()));
            shapes.insert(NodeKey(k), s);
        }
        let mut g = GraphSpec::new(t);
        for k in [1, 2] {
            g.connect_events(
                EventInputPort::new(NodeKey(3), 0),
                EventEdge::Direct(EventOutputPort::new(NodeKey(k), 0)),
            );
        }
        let prep = crate::node::Prepare::new(
            tutti_units::SampleRate::SR_48K,
            tutti_buffers::MaxBlock::of(tutti_units::Samples::new(64)),
        );
        let good = compile(&g.validate().unwrap(), &shapes, &prep, None)
            .unwrap()
            .0;
        verify(&good).expect("sound");
        let dst = good
            .program
            .ops
            .iter()
            .find_map(|op| match *op {
                Op::EventMerge { dst, .. } => Some(dst),
                _ => None,
            })
            .expect("a merge");
        // A port of 5 declared events holds them and the note-off room past
        // them (`note_off_room`), which is what a merge of it must hold.
        let room = crate::note_off_room(5) as u32;
        assert_eq!(
            good.program.sizing.events[dst],
            EventSlotCapacity {
                declared: room,
                defaults: 1
            },
            "five declared events (and their note-off room) and one default"
        );
        for short in [
            EventSlotCapacity {
                declared: room - 1,
                defaults: 1,
            },
            EventSlotCapacity {
                declared: room,
                defaults: 0,
            },
        ] {
            let mut bad = good.clone();
            let slot = dst;
            bad.program.sizing.events[slot] = short;
            let err = verify(&bad).unwrap_err().0;
            assert!(err.contains("writes"), "{short:?}: {err}");
        }
    }

    /// Rule 8: a fade must name a key its delta replaces, once, between
    /// units whose shapes agree in all but the tail.
    #[test]
    fn fades_are_checked_against_both_plans() {
        use crate::model::fade::Fade;
        use tutti_curve::FadeLaw;
        let (a, b) = (NodeKey(1), NodeKey(2));
        let generator = || Shape::source_unassigned(ChannelLayout::MONO);
        let build = |shape_a: Shape, gen_a: u32, prev: Option<&Plan>| {
            let mut t = Topology::default();
            let shapes: Shapes = [(a, shape_a), (b, generator())]
                .into_iter()
                .map(|(k, s)| {
                    t.nodes.insert(
                        k,
                        NodeSpec::new("gen", s.audio_in(), s.audio_out()).tail(s.tail),
                    );
                    (k, s)
                })
                .collect();
            t.set_outputs(vec![PortSource::Node(OutputPort::new(a, 0))]);
            let mut spec = GraphSpec::new(t);
            spec.generations.insert(a, gen_a);
            let prepare = crate::node::Prepare::new(
                tutti_units::SampleRate::SR_48K,
                tutti_buffers::MaxBlock::of(tutti_units::Samples::new(64)),
            );
            compile(&spec.validate().expect("valid"), &shapes, &prepare, prev).expect("compiles")
        };
        let (base, _) = build(generator(), 0, None);
        let fade = Fade::new(tutti_units::Samples::new(32)).law(FadeLaw::EqualPower);
        let check = |shape_a: Shape, fades: Vec<(NodeKey, Fade)>| {
            let (plan, mut delta) = build(shape_a, 1, Some(&base));
            delta.fades = fades;
            verify_fades(Some(&base), &plan, &delta)
        };

        assert_eq!(check(generator(), vec![(a, fade)]), Ok(()));
        let tail = generator().tail(tutti_units::Tail::Finite(tutti_units::Samples::new(9)));
        assert_eq!(
            check(tail, vec![(a, fade)]),
            Ok(()),
            "only the tail differs"
        );
        let not_replaced = check(generator(), vec![(b, fade)]).unwrap_err();
        assert!(
            not_replaced.0.contains("does not replace"),
            "{not_replaced}"
        );
        let twice = check(generator(), vec![(a, fade), (a, fade)]).unwrap_err();
        assert!(twice.0.contains("twice"), "{twice}");
        let in_place = check(
            Shape::source_unassigned(ChannelLayout::MONO).in_place(),
            vec![(a, fade)],
        )
        .unwrap_err();
        assert!(in_place.0.contains("only the tail"), "{in_place}");
        let capacity = check(
            Shape::source_unassigned(ChannelLayout::MONO).event_capacity(crate::testing::cap_of(4)),
            vec![(a, fade)],
        )
        .unwrap_err();
        assert!(capacity.0.contains("only the tail"), "{capacity}");
    }
}
