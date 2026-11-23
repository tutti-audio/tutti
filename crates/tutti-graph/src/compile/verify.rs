//! The plan verifier (compiler step 8; Dropseed's verifier is the prior
//! art).
//!
//! It re-derives every slot access from the **ops** — what the executor will
//! actually do — and checks it against the op DAG, trusting neither the
//! colouring nor the value table it is checking:
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
//! Run by `compile` in every debug build, and callable directly — the tests
//! run it on every proptest graph.

use crate::compile::ids::{
    DelayId, EventSlotId, Id, OpId, PlanEventTapId, PlanMeterId, PlanTapId, PlanUnitId, Table,
    TableSlice, TaskId,
};
use crate::compile::plan::{Delta, EMPTY_SLOT, EventSlotCapacity, Op, Plan, ZERO_SLOT};
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
    slot: u32,
    write: bool,
}

fn accesses(plan: &Plan, op: &Op) -> Vec<Access> {
    let a = |slot, write| Access {
        kind: PortKind::Audio,
        slot,
        write,
    };
    let e = |slot, write| Access {
        kind: PortKind::Event,
        slot,
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
                plan.gain_ports
                    .get(gains.range())
                    .unwrap_or(&[])
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

/// Every event slot a node op's driven params read. Out-of-range spans read
/// nothing here; [`verify_params`] reports them.
fn param_reads(plan: &Plan, params: crate::compile::plan::Span) -> impl Iterator<Item = u32> + '_ {
    plan.param_ports
        .get(params.range())
        .unwrap_or(&[])
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
    for (i, op) in plan.ops.iter().enumerate() {
        match *op {
            Op::Sum { srcs, .. } if !(2..=MAX_PORTS).contains(&(srcs.len as usize)) => {
                return Err(VerifyError(format!(
                    "sum op {i} adds {} addends, not 2 to {MAX_PORTS}",
                    srcs.len
                )));
            }
            Op::WeightedSum {
                srcs, lanes, gains, ..
            } => {
                if !(1..=MAX_PORTS).contains(&(srcs.len as usize)) {
                    return Err(VerifyError(format!(
                        "weighted sum op {i} adds {} addends, not 1 to {MAX_PORTS}",
                        srcs.len
                    )));
                }
                if lanes.len != srcs.len {
                    return Err(VerifyError(format!(
                        "weighted sum op {i} has {} lanes for {} addends",
                        lanes.len, srcs.len
                    )));
                }
                let Some(list) = plan.sum_lanes.get(lanes.range()) else {
                    return Err(VerifyError(format!(
                        "weighted sum op {i} names lanes past the {} the plan lists",
                        plan.sum_lanes.len()
                    )));
                };
                if list.iter().all(Option::is_none) {
                    return Err(VerifyError(format!(
                        "weighted sum op {i} has no weighted lane (a plain sum is `Op::Sum`)"
                    )));
                }
                let Some(driven) = plan.gain_ports.get(gains.range()) else {
                    return Err(VerifyError(format!(
                        "weighted sum op {i} names driven lanes past the {} the plan lists",
                        plan.gain_ports.len()
                    )));
                };
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
    if covered != plan.gain_ports.len() {
        return Err(VerifyError("gain ports no weighted sum names".into()));
    }
    Ok(())
}

/// Rule 7: each node op's driven params.
fn verify_params(plan: &Plan) -> Result<(), VerifyError> {
    let mut covered = 0usize;
    for op in &plan.ops {
        let Op::Node { unit, params, .. } = *op else {
            continue;
        };
        let bad = |what: String| VerifyError(format!("unit {unit}'s params {what}"));
        let ports = plan
            .param_ports
            .get(params.range())
            .ok_or_else(|| bad("name ports past the table".into()))?;
        if params.start as usize != covered {
            return Err(bad("are not the next run of the table".into()));
        }
        covered += ports.len();
        let declared = plan
            .units
            .get(PlanUnitId::new(unit))
            .map(|u| u.shape.params)
            .unwrap_or_default();
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
    if covered != plan.param_ports.len() {
        return Err(VerifyError("param ports no node op names".into()));
    }
    Ok(())
}

/// Checks `plan` (see the [module docs](self) for the rules).
pub fn verify(plan: &Plan) -> Result<(), VerifyError> {
    verify_sums(plan)?;
    let rows: Table<OpId, Vec<OpId>> = plan
        .ops
        .ids()
        .map(|i| plan.op_succ.ids::<OpId, OpId>(i).to_vec())
        .collect();
    for (a, row) in rows.iter_enumerated() {
        if row.iter().any(|&b| b <= a) {
            return Err(VerifyError(format!(
                "op {a} has a successor that does not follow it in the serial order"
            )));
        }
    }
    let reach = Reach::new(&rows);
    let ordered = |a: OpId, b: OpId| a == b || reach.before(a, b) || reach.before(b, a);

    let acc: Vec<Vec<Access>> = plan.ops.iter().map(|op| accesses(plan, op)).collect();
    let audio_fixed = 1 + plan.audio_feedback.len() as u32;
    let event_fixed = 1 + plan.event_feedback.len() as u32;

    // Rule 1 and rule 4.
    for (i, (op, list)) in plan.ops.iter().zip(&acc).enumerate() {
        for x in list.iter().filter(|x| x.write) {
            let reserved = match x.kind {
                PortKind::Audio => x.slot == ZERO_SLOT.get(),
                PortKind::Event => x.slot == EMPTY_SLOT.get(),
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
                        s == x.slot
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
            PortKind::Audio => plan.audio_slots,
            PortKind::Event => plan.event_slots,
        } as usize;
        let mut touch: Vec<Vec<(OpId, bool)>> = vec![Vec::new(); slots];
        for (i, list) in acc.iter().enumerate() {
            for x in list.iter().filter(|x| x.kind == kind) {
                if x.slot as usize >= slots {
                    return Err(VerifyError(format!(
                        "op {i} names {kind:?} slot {} of {slots}",
                        x.slot
                    )));
                }
                touch[x.slot as usize].push((OpId::from_index(i), x.write));
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
        let (values, fixed) = match kind {
            PortKind::Audio => (plan.audio_values.as_slice(), audio_fixed),
            PortKind::Event => (plan.event_values.as_slice(), event_fixed),
        };
        let mut covered: Vec<(OpId, u32)> = Vec::new();
        for v in values.iter() {
            let writer = OpId::new(v.writer);
            for &r in &plan.value_readers[v.readers.range()] {
                covered.push((r, v.slot));
                if !reach.before(writer, r) {
                    return Err(VerifyError(format!(
                        "op {r} reads {kind:?} slot {} before its writer op {} runs",
                        v.slot, v.writer
                    )));
                }
                for &(w, is_write) in &touch[v.slot as usize] {
                    if is_write
                        && w != writer
                        && w != r
                        && reach.before(writer, w)
                        && reach.before(w, r)
                    {
                        return Err(VerifyError(format!(
                            "op {w} overwrites {kind:?} slot {} between its writer {} and reader {r}",
                            v.slot, v.writer
                        )));
                    }
                }
            }
        }
        for (i, list) in acc.iter().enumerate() {
            for x in list
                .iter()
                .filter(|x| x.kind == kind && !x.write && x.slot >= fixed)
            {
                if !covered.contains(&(OpId::from_index(i), x.slot)) {
                    return Err(VerifyError(format!(
                        "op {i} reads {kind:?} slot {} that no value delivers to it",
                        x.slot
                    )));
                }
            }
        }
    }

    // Rule 1, second half: one capture per feedback delay.
    for (kind, fb_len) in [
        (PortKind::Audio, plan.audio_feedback.len()),
        (PortKind::Event, plan.event_feedback.len()),
    ] {
        let mut captures = vec![0usize; fb_len];
        for op in &plan.ops {
            match (kind, op) {
                (PortKind::Audio, Op::Capture { feedback, .. })
                | (PortKind::Event, Op::EventCapture { feedback, .. }) => {
                    match captures.get_mut(*feedback as usize) {
                        Some(c) => *c += 1,
                        None => {
                            return Err(VerifyError(format!(
                                "a capture names {kind:?} feedback {feedback} of {fb_len}"
                            )));
                        }
                    }
                }
                _ => {}
            }
        }
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
    for (i, op) in plan.ops.iter().enumerate() {
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
    let n = plan.ops.len();
    let tasks = plan.tasks.len();
    let mut found: Table<OpId, Option<TaskId>> = Table::filled(n, None);
    for (t, span) in plan.tasks.iter_enumerated() {
        let ops = plan
            .task_ops
            .get(span.range())
            .ok_or_else(|| VerifyError(format!("task {t} runs past the task op list")))?;
        for (j, &op) in ops.iter().enumerate() {
            let slot = found
                .get_mut(op)
                .ok_or_else(|| VerifyError(format!("task {t} names op {op} of {n}")))?;
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
    if plan.task_succ.rows() != tasks || plan.task_activation.len() != tasks {
        return Err(VerifyError("task tables have the wrong length".into()));
    }
    for a in plan.ops.ids() {
        for &b in plan.op_succ.ids::<OpId, OpId>(a) {
            let (ta, tb) = (task_of[a], task_of[b]);
            if ta != tb && !plan.task_succ.ids::<TaskId, TaskId>(ta).contains(&tb) {
                return Err(VerifyError(format!(
                    "op edge {a} → {b} crosses task {ta} → {tb}, which task_succ lacks"
                )));
            }
        }
    }
    let mut indeg: Table<TaskId, u32> = Table::filled(tasks, 0);
    for t in plan.tasks.ids() {
        let mut seen = std::collections::BTreeSet::new();
        for &u in plan.task_succ.ids::<TaskId, TaskId>(t) {
            if u.index() >= tasks || !seen.insert(u) || u == t {
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
        for &u in plan.task_succ.ids::<TaskId, TaskId>(t) {
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
    // The public ops name delays, taps, meters and units by raw index; each
    // is checked against its table here.
    let mut delay_uses: Table<DelayId, usize> = Table::filled(plan.delays.len(), 0);
    let mut unit_uses: Table<PlanUnitId, usize> = Table::filled(plan.units.len(), 0);
    for op in &plan.ops {
        let d = match *op {
            Op::Delay { delay, .. } | Op::EventDelay { delay, .. } => Some(delay),
            Op::Output { delay, .. } => delay,
            Op::Tap { tap, .. } => {
                if plan.taps.get(PlanTapId::new(tap)).is_none() {
                    return Err(VerifyError(format!("a tap op names tap {tap}")));
                }
                None
            }
            Op::EventTap { tap, .. } => {
                if plan.event_taps.get(PlanEventTapId::new(tap)).is_none() {
                    return Err(VerifyError(format!("an event tap op names tap {tap}")));
                }
                None
            }
            Op::Meter { meter, .. } => {
                if plan.meters.get(PlanMeterId::new(meter)).is_none() {
                    return Err(VerifyError(format!("a meter op names meter {meter}")));
                }
                None
            }
            Op::Node { unit, .. } => {
                *unit_uses
                    .get_mut(PlanUnitId::new(unit))
                    .ok_or_else(|| VerifyError(format!("a node op names unit {unit}")))? += 1;
                None
            }
            _ => None,
        };
        if let Some(d) = d {
            *delay_uses
                .get_mut(DelayId::new(d))
                .ok_or_else(|| VerifyError(format!("an op names delay {d}")))? += 1;
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
    let slot_cap = |s: u32| {
        plan.event_slot_capacity
            .get(EventSlotId::new(s))
            .copied()
            .unwrap_or_default()
    };
    for (f, spec) in plan.event_feedback.iter_enumerated() {
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
        Table::filled(plan.ops.len(), EventSlotCapacity::NONE);
    for (m, op) in plan.ops.iter_enumerated() {
        // What op `m` reads from slot `s`, priced by its writer.
        let read_need = |s: u32, op_need: &TableSlice<OpId, EventSlotCapacity>| {
            if s == EMPTY_SLOT.get() {
                return Ok(EventSlotCapacity::NONE);
            }
            if let Some(f) = EventSlotId::new(s).feedback(plan.event_feedback.len()) {
                let spec = &plan.event_feedback[f];
                return match spec.key {
                    crate::compile::plan::FeedbackKey::Event { from, delay, .. } => Ok(
                        EventSlotCapacity::fifo(plan.event_port_capacity(from), delay, block),
                    ),
                    _ => Err(VerifyError(format!("event slot {s} is not event feedback"))),
                };
            }
            plan.event_values
                .iter()
                .find(|v| v.slot == s && plan.value_readers[v.readers.range()].contains(&m))
                .map(|v| op_need[OpId::new(v.writer)])
                .ok_or_else(|| VerifyError(format!("op {m} reads event slot {s} of no value")))
        };
        op_need[m] = match *op {
            Op::Node { unit, .. } => plan
                .units
                .get(PlanUnitId::new(unit))
                .map_or(EventSlotCapacity::NONE, |u| {
                    EventSlotCapacity::port(u.shape.event_capacity)
                }),
            Op::EventDelay { delay, .. } => {
                let d = plan
                    .delays
                    .get(DelayId::new(delay))
                    .ok_or_else(|| VerifyError(format!("op {m} names delay {delay}")))?;
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
    for v in &plan.event_values {
        let need = op_need[OpId::new(v.writer)];
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
    use crate::compile::ids::{AudioSlotId, AudioValueId, EventSlotId, PlanUnitId, TaskId};
    use crate::compile::{Shapes, compile};
    use crate::model::spec::GraphSpec;
    use crate::node::InPlaceMask;
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
        let u = plan.units.iter().position(|u| u.key == NodeKey(2)).unwrap() as u32;
        let op = plan
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
        bad.param_ports[0].param = tutti_units::UnitParam::Q.into();
        assert!(err(&bad).contains("does not declare"), "{}", err(&bad));

        // A port no node op names.
        let mut bad = good.clone();
        for o in &mut bad.ops {
            if let Op::Node { params, .. } = o {
                *params = crate::compile::plan::Span { start: 0, len: 0 };
            }
        }
        bad.nodes
            .recs
            .iter_mut()
            .for_each(|r| r.params = crate::compile::plan::Span { start: 0, len: 0 });
        assert!(
            err(&bad).contains("param ports no node op names"),
            "{}",
            err(&bad)
        );

        // A param read of node 4's event slot, which nothing orders before
        // node 2: a read of a value that may not be written yet.
        let u4 = good.units.iter().position(|u| u.key == NodeKey(4)).unwrap() as u32;
        let other = good
            .ops
            .iter()
            .find_map(|o| match *o {
                Op::Node {
                    unit, event_out, ..
                } if unit == u4 => Some(good.event_list[event_out.start as usize].get()),
                _ => None,
            })
            .expect("node 4 sends events");
        assert_ne!(other, good.param_ports[0].slot);
        let mut bad = good.clone();
        bad.param_ports[0].slot = other;
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
        let a = bad.audio_values[AudioValueId::new(0)].slot;
        let b = bad.audio_values[AudioValueId::new(1)].slot;
        assert_ne!(a, b);
        for s in &mut bad.audio_list {
            if s.get() == b {
                *s = AudioSlotId::new(a);
            }
        }
        for op in &mut bad.ops {
            if let Op::Output { src, .. } = op
                && *src == b
            {
                *src = a;
            }
        }
        bad.audio_values[AudioValueId::new(1)].slot = a;
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
        let unit = |k| good.units.iter().position(|u| u.key == k).unwrap() as u32;
        let out_slot = |p: &Plan, k| {
            p.ops
                .iter()
                .find_map(|op| match *op {
                    Op::Node {
                        unit: u, audio_out, ..
                    } if u == unit(k) => Some(audio_out.start as usize),
                    _ => None,
                })
                .unwrap()
        };
        let mut bad = good.clone();
        let (ia, iw) = (out_slot(&bad, a), out_slot(&bad, w));
        let (sa, sw) = (bad.audio_list[ia], bad.audio_list[iw]);
        assert_ne!(sa, sw);
        bad.audio_list[iw] = sa;
        for op in &mut bad.ops {
            if let Op::Output { src, .. } = op
                && *src == sw.get()
            {
                *src = sa.get();
            }
        }
        for v in &mut bad.audio_values {
            if v.slot == sw.get() {
                v.slot = sa.get();
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
            .ops
            .iter_enumerated()
            .find(|(_, o)| matches!(o, Op::Sum { .. }))
            .map(|(i, _)| i)
            .expect("a sum");
        let Op::Sum { srcs, dst } = good.ops[i] else {
            unreachable!()
        };
        assert_eq!(srcs.len, 2);
        assert_eq!(dst, good.audio_list[srcs.start as usize].get(), "in place");

        // One addend is an alias, never an op.
        let mut bad = good.clone();
        bad.ops[i] = Op::Sum {
            srcs: crate::compile::plan::Span {
                start: srcs.start,
                len: 1,
            },
            dst,
        };
        assert!(verify(&bad).unwrap_err().0.contains("addends"));

        // 65 addends: more than the executor's table holds.
        let mut bad = good.clone();
        let start = bad.audio_list.len() as u32;
        let first = good.audio_list[srcs.start as usize];
        bad.audio_list.extend(std::iter::repeat_n(first, 65));
        bad.ops[i] = Op::Sum {
            srcs: crate::compile::plan::Span { start, len: 65 },
            dst,
        };
        assert!(verify(&bad).unwrap_err().0.contains("addends"));

        // A write that aliases the second read, not the first.
        let mut bad = good.clone();
        let second = good.audio_list[srcs.start as usize + 1].get();
        bad.ops[i] = Op::Sum { srcs, dst: second };
        assert!(verify(&bad).unwrap_err().0.contains("without declaring"));
    }

    /// A read the DAG does not order after its write is reported.
    #[test]
    fn the_verifier_rejects_a_read_before_its_write() {
        let mut bad = independent_pair();
        // Cut every edge: the outputs no longer follow their generators.
        let rows = bad.op_succ.rows();
        bad.op_succ = crate::compile::plan::Csr::from_rows(&vec![Vec::new(); rows]);
        assert!(verify(&bad).is_err());
    }

    #[test]
    fn the_verifier_rejects_each_structural_corruption() {
        let good = independent_pair();
        verify(&good).expect("sound");

        // An in-place bit where the slots differ.
        let mut bad = good.clone();
        for op in &mut bad.ops {
            if let Op::Node {
                in_place, audio_in, ..
            } = op
                && audio_in.len == 0
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
        let start = bad.task_ops.len() as u32;
        bad.task_ops.push(dup);
        bad.tasks.push(crate::compile::plan::Span { start, len: 1 });
        let rows: Vec<Vec<u32>> = (0..bad.tasks.len() - 1)
            .map(|t| bad.task_succ.row(t).to_vec())
            .chain([Vec::new()])
            .collect();
        bad.task_succ = crate::compile::plan::Csr::from_rows(&rows);
        bad.task_activation.push(0);
        assert!(verify(&bad).unwrap_err().0.contains("two tasks"));

        // A cross-task edge missing from task_succ (needs two tasks).
        let mut bad = good.clone();
        assert!(bad.tasks.len() >= 2, "the pair has independent tasks");
        let edges: Vec<(usize, u32)> = (0..bad.ops.len())
            .flat_map(|a| bad.op_succ.row(a).iter().map(move |&b| (a, b)))
            .collect();
        assert!(!edges.is_empty());
        bad.task_succ = crate::compile::plan::Csr::from_rows(&vec![Vec::new(); bad.tasks.len()]);
        bad.task_activation = Table::filled(bad.tasks.len(), 0);
        // Only fails if some edge crossed tasks; the pair's generator → output
        // edges are fused, so add a cross edge by splitting a task.
        let crossing = {
            let mut t = good.clone();
            let span = t.tasks.as_slice()[0];
            if span.len >= 2 {
                let mut spans = t.tasks.as_slice().to_vec();
                spans[0].len = 1;
                spans.insert(
                    1,
                    crate::compile::plan::Span {
                        start: span.start + 1,
                        len: span.len - 1,
                    },
                );
                t.tasks = spans.into();
                t.task_succ =
                    crate::compile::plan::Csr::from_rows(&vec![Vec::new(); t.tasks.len()]);
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
            .find(|(_, s)| s.len >= 2)
            .map(|(t, _)| t)
            .expect("the pair fuses a chain");
        let r = bad.tasks[t].range();
        bad.task_ops[r].reverse();
        assert!(verify(&bad).unwrap_err().0.contains("ascending"));

        // Activation counts that do not match.
        let mut bad = good.clone();
        if let Some(a) = bad.task_activation.get_mut(TaskId::new(0)) {
            *a += 1;
        }
        assert!(verify(&bad).unwrap_err().0.contains("activation"));

        // A delay no op uses.
        let mut bad = good.clone();
        bad.delays.push(crate::compile::plan::DelaySpec {
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
            .ops
            .iter()
            .find_map(|op| match *op {
                Op::Node { unit, .. } => Some(unit),
                _ => None,
            })
            .expect("the pair has node ops");
        let second = bad
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
        let idx = bad.units[PlanUnitId::new(0)].idx;
        bad.units[PlanUnitId::new(1)].idx = idx;
        assert!(verify(&bad).unwrap_err().0.contains("store index"));
    }

    /// Every node record carries what the plan says: its unit's fields, the
    /// shape's port counts, the op's slots and sorted borrow runs, and the
    /// form its shape and slots call for. A table row is one node: a source
    /// with latency and a finite tail, an in-place node with an unbounded
    /// tail, a split, an event source and sink (the general form), a stereo
    /// node (direct) and a three-channel one (the audio walk). The expected
    /// values are read off the shapes and the plan's ops, never off the
    /// lowering.
    #[test]
    fn every_node_record_carries_what_the_plan_says() {
        use crate::compile::plan::{Direct, Form};
        use crate::exec::arena::Role;
        use crate::model::spec::{EventEdge, EventInputPort, EventOutputPort};
        use tutti_units::{Latency, Samples, Tail};
        let mono = ChannelLayout::MONO;
        let latent = Latency::new(Samples::new(4));
        let wide = ChannelLayout::from_count(3);
        // (key, shape, the audio source feeding it, if any).
        let rows = [
            (
                1,
                Shape::source_unassigned(mono)
                    .latency(latent)
                    .tail(Tail::Finite(Samples::new(9))),
                None,
            ),
            (
                2,
                Shape::through_unassigned(mono)
                    .in_place()
                    .tail(Tail::Unbounded),
                Some(1),
            ),
            (3, Shape::through_unassigned(mono), Some(2)),
            (
                4,
                Shape::EMPTY
                    .events(0, 1)
                    .event_capacity(crate::testing::cap_of(5)),
                None,
            ),
            (5, Shape::source_unassigned(mono).events(1, 0), None),
            (6, Shape::source_unassigned(ChannelLayout::STEREO), None),
            (7, Shape::through_unassigned(ChannelLayout::STEREO), Some(6)),
            (8, Shape::source_unassigned(wide), None),
            (9, Shape::through_unassigned(wide), Some(8)),
        ];
        let mut t = Topology::default();
        let mut shapes = Shapes::new();
        for &(k, shape, from) in &rows {
            let key = NodeKey(k);
            let mut spec = NodeSpec::new("n", shape.audio_in(), shape.audio_out());
            spec.latency = shape.latency.samples();
            spec.tail = shape.tail;
            t.nodes.insert(key, spec);
            shapes.insert(key, shape);
            for port in 0..shape.audio_in().count() {
                let from = NodeKey(from.expect("a node with inputs is fed"));
                t.edges.insert(
                    crate::InputPort::new(key, port),
                    vec![crate::Edge::Direct(PortSource::Node(OutputPort::new(
                        from, port,
                    )))],
                );
            }
        }
        t.set_outputs(
            [3, 5, 7, 7, 9, 9, 9]
                .into_iter()
                .zip([0, 0, 0, 1, 0, 1, 2])
                .map(|(k, p)| PortSource::Node(OutputPort::new(NodeKey(k), p)))
                .collect::<Vec<_>>(),
        );
        let mut g = GraphSpec::new(t);
        g.connect_events(
            EventInputPort::new(NodeKey(5), 0),
            EventEdge::Direct(EventOutputPort::new(NodeKey(4), 0)),
        );
        let prep = crate::node::Prepare::new(
            tutti_units::SampleRate::SR_48K,
            tutti_buffers::MaxBlock::of(Samples::new(64)),
        );
        let plan = compile(&g.validate().expect("valid"), &shapes, &prep, None)
            .expect("compiles")
            .0;

        assert_eq!(plan.nodes.recs.len(), plan.units.len(), "a record a unit");
        assert_eq!(plan.units.len(), rows.len());
        let mut forms = Vec::new();
        for op in &plan.ops {
            let Op::Node {
                unit,
                audio_in,
                audio_out,
                event_in,
                event_out,
                in_place,
                params,
            } = *op
            else {
                continue;
            };
            let unit = PlanUnitId::new(unit);
            let rec = &plan.nodes.recs[unit];
            let pu = &plan.units[unit];
            let shape = shapes[&pu.key];
            let at = format!("node {:?}", pu.key);
            assert_eq!(rec.unit, unit, "{at}");
            assert_eq!(rec.store, pu.idx, "{at}");
            assert_eq!(
                rec.generation,
                crate::compile::ids::Generation::new(pu.generation),
                "{at}"
            );
            assert_eq!(rec.arrival, pu.arrival, "{at}");
            assert_eq!(rec.latency, shape.latency.samples(), "{at}");
            assert_eq!(rec.tail, shape.tail, "{at}");
            assert_eq!(
                rec.park_after,
                Tail::Finite(shape.latency.samples()).then(shape.tail),
                "{at}"
            );
            assert_eq!(rec.event_capacity, shape.event_capacity, "{at}");
            assert_eq!(rec.in_place, in_place, "{at}");
            assert_eq!(rec.params, params, "{at}");
            assert_eq!(rec.declared, shape.params, "{at}");
            assert_eq!(
                in_place.get(0),
                pu.key == NodeKey(2),
                "only the in-place node is aliased: {at}"
            );

            let ain = &plan.audio_list[audio_in.range()];
            let aout = &plan.audio_list[audio_out.range()];
            let ein = &plan.event_list[event_in.range()];
            let eout = &plan.event_list[event_out.range()];
            let counts = [rec.ain, rec.aout, rec.ein, rec.eout].map(usize::from);
            assert_eq!(
                counts,
                [
                    usize::from(shape.audio_in().count()),
                    usize::from(shape.audio_out().count()),
                    usize::from(shape.event_in),
                    usize::from(shape.event_out)
                ],
                "{at}"
            );
            assert_eq!(
                counts,
                [ain.len(), aout.len(), ein.len(), eout.len()],
                "{at}"
            );
            assert_eq!(
                rec.ports(&plan.nodes),
                crate::compile::plan::NodePorts {
                    ain,
                    aout,
                    ein,
                    eout
                },
                "{at}"
            );

            // The requests the op makes, each run sorted.
            fn sorted<S: Ord>(mut v: Vec<(S, Role)>) -> Vec<(S, Role)> {
                v.sort_unstable();
                v
            }
            fn reads<S: Copy>(slots: &[S], skip: InPlaceMask) -> Vec<(S, Role)> {
                slots
                    .iter()
                    .enumerate()
                    .filter(|&(c, _)| !skip.get(c))
                    .map(|(c, &s)| (s, Role::Read(c as u8)))
                    .collect()
            }
            fn writes<S: Copy>(slots: &[S]) -> Vec<(S, Role)> {
                slots
                    .iter()
                    .enumerate()
                    .map(|(c, &s)| (s, Role::Write(c as u8)))
                    .collect()
            }
            assert_eq!(
                plan.nodes.audio_borrows[rec.borrows.range()],
                sorted([reads(ain, in_place), writes(aout)].concat()),
                "{at}"
            );
            assert_eq!(
                plan.nodes.event_borrows[rec.event_borrows.range()],
                sorted([reads(ein, InPlaceMask::NONE), writes(eout)].concat()),
                "{at}"
            );

            let width = (ain.len(), aout.len());
            let want = match (ein.len() + eout.len() > 0, width) {
                (true, _) => "General",
                (_, (0, 1)) => "Source",
                (_, (1, 1)) if in_place.get(0) => "InPlace",
                (_, (1, 1)) => "Split",
                (_, w) if Direct::SHAPES.contains(&w) => "Direct",
                _ => "Audio",
            };
            let got = match rec.form {
                Form::General => "General",
                Form::Source { out } => {
                    assert_eq!(aout, [out], "{at}");
                    "Source"
                }
                Form::Split { input, out } => {
                    assert_eq!((ain, aout), (&[input][..], &[out][..]), "{at}");
                    assert_ne!(input, out, "a split never reads and writes one slot");
                    "Split"
                }
                Form::InPlace { slot } => {
                    assert_eq!((ain, aout), (&[slot][..], &[slot][..]), "{at}");
                    "InPlace"
                }
                Form::Direct(d) => {
                    assert_eq!((usize::from(d.ins), usize::from(d.outs)), width, "{at}");
                    "Direct"
                }
                Form::Audio => "Audio",
            };
            assert_eq!(got, want, "{at}");
            forms.push(got);
        }
        forms.sort_unstable();
        forms.dedup();
        assert_eq!(
            forms,
            ["Audio", "Direct", "General", "InPlace", "Source", "Split"],
            "the table holds a node of each form"
        );
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
            good.event_slot_capacity[EventSlotId::new(dst)],
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
            bad.event_slot_capacity[EventSlotId::new(dst)] = short;
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
