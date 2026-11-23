//! Applying a commit: installing its plan, units and state, or checking
//! every unit out for a re-prepare.

use tutti_buffers::Float;
use tutti_rt::CappedVec;
use tutti_units::Samples;

use super::commit::{Commit, Incoming, Relined, cut_fades, retire_fade, send_back};
use super::stage::{RingFrom, Route, Staged, sort_by_offset};
use super::state::{Gone, Inject, Ring, State, Unit};
use super::{Executor, Suspension, place, place_all};
use crate::NodeKey;
use crate::compile::ids::{Generation, PlanUnitId, TableSlice};
use crate::compile::plan::Plan;
use crate::event::types::Event;
use crate::exec::held::Held;
use crate::model::gain::SumLane;
use crate::model::spec::{EventInputPort, EventOutputPort};

impl<F: Float> Executor<F> {
    /// Applies every queued commit, in the order the editor sent them, and
    /// sends each box back. [`process`](Self::process) calls this first; it is
    /// public so a caller can install a commit without rendering.
    ///
    /// Delay rings, feedback state and units carry over by key (see
    /// [`DelayKey`](crate::DelayKey) and [`FeedbackKey`](crate::FeedbackKey)). An event delay or event feedback
    /// whose key disappears flushes its pending events to its sink when the
    /// sink survives: they arrive on the sink's next call, keeping their
    /// relative spacing, with the earliest at offset 0.
    ///
    /// # Panics
    ///
    /// If a plan was compiled for a different [`Prepare`](crate::Prepare) than this
    /// executor's (only possible through
    /// [`Editor::package`](crate::Editor::package)).
    pub fn apply_pending(&mut self) {
        let _rt = self.prepare.enter_rt_scope();
        while let Some(mut commit) = self.stash.pop_front() {
            self.apply(&mut commit);
            send_back(&mut self.inbox.back, commit);
        }
        while let Some(mut commit) = self.inbox.queue.pop() {
            self.apply(&mut commit);
            send_back(&mut self.inbox.back, commit);
        }
    }

    pub(super) fn apply(&mut self, c: &mut Commit<F>) {
        self.inbox.applied = c.seq;
        if let Some(prepare) = c.suspend {
            // Check every unit out, into the box, for the editor to
            // re-prepare on the control thread. The plan and every piece of
            // delay and feedback state stay where they are, for the resume
            // commit's rebuild to carry — or, after a rate change, to reset.
            //
            // A re-prepare cuts every crossfade: what is checked out is the
            // newest unit at each key (a waiting one, if any), the one the
            // plan's generation names; the others go back with their fades.
            std::mem::swap(&mut self.parked, &mut c.parked);
            if let Some(plan) = &self.plan {
                for u in &plan.units {
                    let Some(mut unit) =
                        self.store.get_mut(u.idx.0 as usize).and_then(Option::take)
                    else {
                        continue;
                    };
                    // The unit a cut displaced still owes its keys: they close
                    // in the first block after the resume.
                    if let Some(stale) = cut_fades(&mut unit, &mut self.inbox.fade_back) {
                        place(&mut c.spare_held, unit.keys.park(stale));
                    }
                    place(&mut c.retired, (u.key, unit.node));
                    place(&mut c.spare_params, unit.params);
                    place(&mut c.spare_modes, unit.mode);
                    place(&mut self.parked, (u.key, unit.keys));
                }
            }
            let mut reset_time = false;
            let (old, new) = (
                self.prepare.sample_rate().get(),
                prepare.sample_rate().get(),
            );
            if old != new {
                // `Frame` always means samples at the *current* rate since
                // start. Graph time tracks device time, so the clock and
                // every pending frame-timed command move to the same
                // wall-clock time at the new rate.
                reset_time = true;
                // Frames at the old rate name nothing at the new one.
                self.clock.history.clear();
                let ratio = new / old;
                self.clock.frame = self.clock.frame.scaled(ratio);
                self.clock.play.rescale(ratio);
                self.inbox.commands.rescale(ratio, c.seq);
            }
            self.suspension = Suspension::Suspended {
                next: prepare,
                reset_time,
            };
            return;
        }
        // The resume commit of a re-prepare: its plan is for the `Prepare`
        // the suspend carried, and its rebuild carries time-based state
        // unless the rate changed.
        let carry = match std::mem::replace(&mut self.suspension, Suspension::Running) {
            Suspension::Running => true,
            Suspension::Suspended { next, reset_time } => {
                self.prepare = next;
                !reset_time
            }
        };
        let plan = c
            .plan
            .take()
            .expect("a commit carries its plan until applied");
        assert_eq!(
            plan.prepare, self.prepare,
            "a plan compiled for another Prepare: every unit is prepared for the executor's"
        );
        let mut st = c
            .staged
            .take()
            .expect("a commit that places a plan carries what it needs staged");

        // A key retired or replaced leaves its held keys behind: the rebuild
        // below closes them on the edges they were held on. Not so a key
        // replaced with a fade while one runs: its unit keeps writing, and
        // the replacement waits (`end_fades` closes its keys then).
        let mut departed = std::mem::take(&mut c.gone);
        let n_retire = c.delta.retire.len();
        for (i, p) in c
            .delta
            .retire
            .iter()
            .chain(c.delta.replace.iter().map(|(old, _)| old))
            .enumerate()
        {
            let waits = i >= n_retire
                && c.fades_in(c.delta.replace[i - n_retire].1.key)
                && self
                    .store
                    .get(p.idx.0 as usize)
                    .and_then(Option::as_ref)
                    .is_some_and(|u| u.fade.is_some());
            if waits {
                continue;
            }
            if let Some(u) = self
                .store
                .get_mut(p.idx.0 as usize)
                .and_then(Option::as_mut)
            {
                let tables = u.keys.depart(&mut departed.spare);
                place(&mut departed.held, (p.key, tables));
            }
        }
        for (_, new) in &c.delta.replace {
            place(&mut departed.replaced, new.key);
        }
        // The state a replaced node hands its successor, written while it is
        // still in the store (see `Node::chase`).
        departed.chase_events = std::mem::take(&mut self.chase);
        departed.chase_events.clear();
        for (old, new) in &c.delta.replace {
            let running = c.fades_in(new.key)
                && self
                    .store
                    .get(old.idx.0 as usize)
                    .and_then(Option::as_ref)
                    .is_some_and(|u| u.fade.is_some());
            let capacity = self
                .plan
                .as_ref()
                .and_then(|p| p.unit(old.key))
                .and_then(|u| u.shape.chase);
            let node = self.store.get(old.idx.0 as usize).and_then(Option::as_ref);
            if let (Some(capacity), Some(u)) = (capacity, node) {
                let from = departed.chase_events.len();
                let mut out =
                    crate::node::Chase::new(&mut departed.chase_events, capacity.get() as usize);
                u.node.chase(&mut out);
                let refused = out.refused();
                if running {
                    // Behind a running fade: the unit fading in is the
                    // predecessor, and the successor starts once that fade
                    // ends. What it wrote waits in the queued crossfade
                    // (`end_fades` delivers it).
                    let fade = c
                        .incoming
                        .iter_mut()
                        .find(|i| i.idx == new.idx)
                        .and_then(|i| i.fade.as_mut());
                    if let Some(fade) = fade {
                        // The editor sized the buffer for this predecessor
                        // (`prepare_incoming`); a push never grows it, and
                        // what does not fit is refused like the rest.
                        let wrote = &departed.chase_events[from..];
                        let kept = fade.chase.extend(wrote.iter().copied());
                        let unfit = u32::try_from(wrote.len() - kept).unwrap_or(u32::MAX);
                        place(
                            &mut departed.held_refused,
                            (new.key, refused.saturating_add(unfit)),
                        );
                    }
                    departed.chase_events.drain(from..);
                } else {
                    place(
                        &mut departed.chased,
                        (new.key, from..departed.chase_events.len(), refused),
                    );
                }
            }
        }
        // A retired key, or one replaced without a fade, goes whole: its
        // unit and any crossfade running or waiting there. A key replaced
        // with a fade stays, for the incoming unit to fade in over it.
        for i in 0..c.delta.retire.len() + c.delta.replace.len() {
            let p = match i.checked_sub(c.delta.retire.len()) {
                None => c.delta.retire[i],
                Some(j) => {
                    let (old, new) = c.delta.replace[j];
                    if c.fades_in(new.key) {
                        continue;
                    }
                    old
                }
            };
            if let Some(u) = self.store.get_mut(p.idx.0 as usize).and_then(Option::take) {
                debug_assert_eq!(
                    u.generation,
                    Generation::new(p.generation),
                    "retiring the unit the delta named"
                );
                place(&mut c.retired, (p.key, u.node));
                place(&mut c.spare_params, u.params);
                place(&mut c.spare_modes, u.mode);
                for f in [u.fade, u.queued].into_iter().flatten() {
                    retire_fade(&mut self.inbox.fade_back, f);
                }
            }
        }
        // Cuts: a kept unit's crossfades go, its newest unit stays.
        for p in &c.delta.cuts {
            if let Some(u) = self
                .store
                .get_mut(p.idx.0 as usize)
                .and_then(Option::as_mut)
            {
                debug_assert_eq!(
                    u.generation,
                    Generation::new(p.generation),
                    "cutting the unit the delta named"
                );
                if let Some(stale) = cut_fades(u, &mut self.inbox.fade_back) {
                    place(&mut departed.held, (p.key, stale));
                }
            }
        }
        if let Some(mut store) = st.store.take() {
            // A store with room for the plan's units, built with the commit:
            // the units move into it.
            store.append(&mut self.store);
            std::mem::swap(&mut self.store, &mut store);
            st.store = Some(store);
        }
        if self.store.len() < c.delta.store_len as usize {
            self.store.resize_with(c.delta.store_len as usize, || None);
        }
        // Taken out and put back, so the list's buffer is not freed here.
        let mut incoming = std::mem::take(&mut c.incoming);
        for Incoming {
            key: _,
            idx,
            generation,
            node,
            fade,
            params,
            mode,
            keys,
            staging,
        } in incoming.drain(..)
        {
            let slot = &mut self.store[idx.0 as usize];
            match (fade, slot.as_mut()) {
                (Some(mut x), Some(u)) => {
                    // The key's param state and mode run on across the
                    // replace; the ones built for the incoming unit go back
                    // unused.
                    place(&mut c.spare_params, params);
                    place(&mut c.spare_modes, mode);
                    // A crossfade replaced without one running here has
                    // departed the unit's keys: it gets its tables back.
                    u.keys.refill(keys, &mut c.spare_held);
                    // A crossfade: the unit at this key fades out — or, with
                    // a fade already running here, this one waits for it,
                    // superseding one that was waiting.
                    u.generation = generation;
                    if u.fade.is_some() {
                        x.waiting = Some(node);
                        if let Some(q) = u.queued.replace(x) {
                            retire_fade(&mut self.inbox.fade_back, q);
                        }
                        // The tables the waiting unit will write, unless the
                        // unit has them already.
                        place(&mut c.spare_held, u.keys.wait(staging));
                    } else {
                        place(&mut c.spare_held, staging);
                        x.out = Some(std::mem::replace(&mut u.node, node));
                        u.fade = Some(x);
                        u.quiet = Samples::ZERO;
                        u.last_quiet = false;
                        u.last_idle = false;
                        u.reported = None;
                    }
                }
                (fade, _) => {
                    debug_assert!(slot.is_none(), "store index {} is occupied", idx.0);
                    *slot = Some(Unit::new(generation, node, params, mode, keys));
                    place(&mut c.spare_held, staging);
                    // Nothing to fade from (reachable only through
                    // `package`): the crossfade goes back unused.
                    if let Some(x) = fade {
                        retire_fade(&mut self.inbox.fade_back, x);
                    }
                }
            }
        }
        c.incoming = incoming;
        // A kept unit's bypass line, rebuilt for its new declared latency.
        let mut relined = std::mem::take(&mut c.relined);
        for Relined { idx, latency, line } in relined.drain(..) {
            if let Some(u) = self.store.get_mut(idx.0 as usize).and_then(Option::as_mut)
                && let Some(old) = u.mode.aligned.relined(latency, line)
            {
                place(&mut c.spare_lines, old);
            }
        }
        c.relined = relined;

        let old_plan = self.plan.take();
        debug_assert_eq!(
            st.carry, carry,
            "staged for the rate the executor resumes at"
        );
        let new_state = self.adopt(&plan, old_plan.as_deref(), &mut st, &mut departed);
        // The chase buffer sized for this plan; the old one goes back.
        departed.chase_events.clear();
        self.chase = std::mem::replace(&mut st.chase, std::mem::take(&mut departed.chase_events));
        for (_, tables) in departed.held.drain(..) {
            place(&mut c.spare_held, tables);
        }
        place_all(&mut c.spare_held, &mut departed.spare);
        // Units whose mix this plan drives and that have no lane yet.
        for (store, lane) in st.drives.drain(..) {
            match self
                .store
                .get_mut(store.0 as usize)
                .and_then(Option::as_mut)
            {
                Some(u) => u.mode.mix.drive_with(lane, &mut st.spare_drives),
                None => place(&mut st.spare_drives, lane),
            }
        }
        let old_state = std::mem::replace(&mut self.state, new_state);
        c.old_state = Some(old_state);
        c.gone = departed;
        // A re-prepare's buffer of parked tables goes back with the resume.
        if self.parked.capacity() > 0 {
            std::mem::swap(&mut self.parked, &mut c.parked);
        }
        c.staged = Some(st);
        self.main_output_fade
            .install(&plan, old_plan.as_deref(), c.seq);
        self.plan = Some(plan);
        c.plan = old_plan;
    }

    /// Installs the state staged for `plan` (`stage.rs`), moving into it
    /// what carries out of `self.state` (which keeps what does not, to be
    /// retired with it), and flushing what disappeared to the sinks that
    /// remain. Allocates and frees nothing: every buffer was built with the
    /// commit, and what is replaced goes back in it.
    #[allow(clippy::too_many_lines)]
    fn adopt(
        &mut self,
        plan: &Plan,
        old_plan: Option<&Plan>,
        st: &mut Staged<F>,
        departed: &mut Gone,
    ) -> State<F> {
        let max_block = self.prepare.max_block().get();
        let mut new = st.state.take().expect("a staged commit carries its state");
        let old = &mut self.state;

        // PDC rings and FIFOs: one moves in whole when its length stands, and
        // is copied into the ring built for its new length when it does not.
        // A ring copied from one that another ring then takes: before it goes.
        for (i, from) in st.rings.iter().enumerate() {
            if let RingFrom::Copy { from } = *from
                && let (Some(Ring::Audio(n)), Some(Some(Ring::Audio(o)))) =
                    (new.rings[i].as_mut(), old.rings.get(from as usize))
            {
                n.transfer_from(o);
            }
        }
        for (i, from) in st.rings.iter().enumerate() {
            match *from {
                RingFrom::New | RingFrom::Copy { .. } => {}
                RingFrom::Keep { from, rate } => {
                    let mut ring = old.rings.get_mut(from as usize).and_then(Option::take);
                    if let Some(Ring::Event(f)) = &mut ring {
                        f.retune(plan.delays.as_slice()[i].len);
                        f.resize(rate, max_block);
                    }
                    new.rings[i] = ring;
                }
                RingFrom::Move { from } => {
                    let carried = old.rings.get_mut(from as usize).and_then(Option::take);
                    let lost = move_ring(new.rings[i].as_mut(), carried, &mut st.spare_rings);
                    self.dropped.charge_delay(plan, i as u32, lost as u64);
                }
            }
        }
        for (i, from) in st.audio_fb.iter().enumerate() {
            if let Some(j) = *from {
                new.audio_fb[i] = old.audio_fb.get_mut(j as usize).and_then(Option::take);
            }
        }
        for (i, from) in st.event_fb.iter().enumerate() {
            match *from {
                RingFrom::New | RingFrom::Copy { .. } => {}
                RingFrom::Keep { from, rate } => {
                    let mut fifo = old.event_fb.get_mut(from as usize).and_then(Option::take);
                    if let Some(f) = &mut fifo {
                        f.resize(rate, max_block);
                    }
                    new.event_fb[i] = fifo;
                }
                RingFrom::Move { from } => {
                    let carried = old
                        .event_fb
                        .get_mut(from as usize)
                        .and_then(Option::take)
                        .map(Ring::Event);
                    let mut target = new.event_fb[i].take().map(Ring::Event);
                    let lost = move_ring(target.as_mut(), carried, &mut st.spare_rings);
                    new.event_fb[i] = match target {
                        Some(Ring::Event(f)) => Some(f),
                        other => {
                            debug_assert!(other.is_none(), "a feedback FIFO is an event FIFO");
                            None
                        }
                    };
                    self.dropped.charge_feedback(plan, i as u32, lost as u64);
                }
            }
        }
        for (i, from) in st.taps.iter().enumerate() {
            if let Some(j) = *from {
                new.taps[i] = old.taps.get_mut(j as usize).and_then(Option::take);
            }
        }
        for (i, from) in st.event_taps.iter().enumerate() {
            if let Some(j) = *from {
                new.event_taps[i] = old.event_taps.get_mut(j as usize).and_then(Option::take);
            }
        }
        for (i, from) in st.meters.iter().enumerate() {
            if let Some(j) = *from {
                let writer = old.meters.get_mut(j as usize).and_then(Option::take);
                if let Some(w) = &writer {
                    w.set_width(plan.meters.as_slice()[i].1);
                }
                new.meters[i] = writer;
            }
        }
        // A driven state keeps its place across commits that keep the declick
        // length (not the rate: distinct rates can round to one length).
        let same_declick = old_plan.is_none_or(|o| o.prepare.declick() == plan.prepare.declick());
        for (i, from) in st.lanes.iter().enumerate() {
            if let Some(carried) = from
                .and_then(|j| old.lanes.get_mut(j as usize))
                .map(SumLane::carry)
            {
                new.lanes[i].adopt(carried, max_block, same_declick, &mut st.spare_drives);
            }
        }

        // Flush what disappeared, to sinks that survive. Order: what replaced
        // nodes wrote for their successors, injections not yet delivered,
        // then event delays and event feedback in key order.
        let (inject, has_inject) = (&mut new.inject, &mut new.has_inject);
        for (key, span, refused) in departed.chased.drain(..) {
            let at = EventInputPort::new(key, 0);
            let events = &departed.chase_events[span];
            match flush_target(plan, inject, has_inject, at) {
                Some((u, shell)) => {
                    let lost = fill(&mut shell.events, events);
                    self.dropped.at_node(u, refused.saturating_add(lost));
                }
                None => {
                    if let Some(u) = plan.unit_id(key) {
                        let lost = u32::try_from(events.len()).unwrap_or(u32::MAX);
                        self.dropped.at_node(u, lost.saturating_add(refused));
                    }
                }
            }
        }
        if let Some(op) = old_plan {
            for inj in old.inject.iter_mut().filter(|i| i.live) {
                let at = EventInputPort::new(op.units[inj.unit].key, inj.port);
                if let Some((u, shell)) = flush_target(plan, inject, has_inject, at) {
                    let mut lost = fill(&mut shell.events, &inj.events);
                    // What was owed there is still owed: the tables move.
                    for table in inj.owed.drain(..) {
                        lost = lost.saturating_add(shell.owe(table, &mut st.spare_tables));
                    }
                    self.dropped.at_node(u, lost);
                }
            }
            for g in &st.gone_delays {
                if let Some(Some(Ring::Event(f))) = old.rings.get_mut(g.from as usize)
                    && let Some((u, shell)) = flush_target(plan, inject, has_inject, g.at)
                {
                    let mut lost =
                        u32::try_from(f.flush_into(&mut shell.events)).unwrap_or(u32::MAX);
                    // What the FIFO still owed its sink is owed on.
                    if let Some(table) = f.take_owed() {
                        lost = lost.saturating_add(shell.owe(table, &mut st.spare_tables));
                    }
                    self.dropped.at_node(u, lost);
                }
            }
            for g in &st.gone_feedback {
                if let Some(Some(f)) = old.event_fb.get(g.from as usize)
                    && let Some((u, shell)) = flush_target(plan, inject, has_inject, g.at)
                {
                    let lost = f.flush_into(&mut shell.events);
                    self.dropped
                        .at_node(u, u32::try_from(lost).unwrap_or(u32::MAX));
                }
            }
        }
        // A re-prepare's tables go back to the units that come back; the
        // others are as retired.
        let mut parked = std::mem::take(&mut self.parked);
        for (key, mut keys) in parked.drain(..) {
            let same = |a: Option<&Plan>| a.and_then(|p| p.unit(key)).map(|u| u.generation);
            let back = (same(old_plan) == same(Some(plan)))
                .then(|| plan.unit_id(key))
                .flatten()
                .and_then(|i| self.store[plan.units[i].idx.0 as usize].as_mut());
            match back {
                Some(u) => {
                    keys.settle();
                    u.keys = keys;
                }
                None => {
                    let tables = keys.depart(&mut departed.spare);
                    place(&mut departed.held, (key, tables));
                }
            }
        }
        self.parked = parked;
        // Keys held on an edge that ended go to the sink that lost it, after
        // everything flushed to that sink (the offs of notes whose ons were in
        // flight). See `held.rs`.
        if old_plan.is_some() {
            for i in inject.iter_mut() {
                i.closed_from = i.events.len();
            }
            let source_gone = |n: NodeKey| departed.held.iter().any(|(k, _)| *k == n);
            // What a port holds, and what it owes and has not yet sent.
            let held_of = |from: EventOutputPort| -> (Option<&Held>, Option<&Held>) {
                let port = usize::from(from.port);
                if let Some((_, tables)) = departed.held.iter().find(|(k, _)| *k == from.node) {
                    return (tables.get(port).and_then(Option::as_ref), None);
                }
                let unit = plan
                    .unit_id(from.node)
                    .and_then(|i| self.store[plan.units[i].idx.0 as usize].as_ref());
                match unit {
                    Some(u) => (
                        u.keys.held().get(port).and_then(Option::as_ref),
                        u.keys.owed(from.port),
                    ),
                    None => (None, None),
                }
            };
            for closer in &mut st.closers {
                let (from, at) = (closer.from, closer.at);
                let in_plan = plan.event_edges.binary_search(&(from, at)).is_ok();
                if (in_plan && !source_gone(from.node)) || departed.replaced.contains(&at.node) {
                    continue;
                }
                let (held, owed) = held_of(from);
                let owing = [held, owed].into_iter().flatten().filter(|h| !h.is_empty());
                if owing.clone().next().is_none() {
                    continue;
                }
                match std::mem::replace(&mut closer.route, Route::Placed) {
                    // A source whose edge (and its delay) goes on: the close
                    // is owed to the delay's FIFO, which pours it in behind
                    // what is in flight.
                    Route::Delay(i) => {
                        let placed = match new.rings[i as usize].as_mut() {
                            Some(Ring::Event(f)) => {
                                let owed = owing.clone().all(|h| f.owe(h));
                                // What fits goes in now, as the close of a
                                // cut edge always did; the rest pours as
                                // room frees.
                                f.pour();
                                owed
                            }
                            _ => false,
                        };
                        if !placed {
                            // The FIFO was built with its table.
                            debug_assert!(false, "a FIFO that owes closes has a table");
                            let mut lost = 0u32;
                            owing.for_each(|h| h.close_each(|_| lost = lost.saturating_add(1)));
                            if let Some(u) = plan.unit_id(at.node) {
                                self.dropped.at_node(u, lost);
                            }
                        }
                    }
                    // The edge ended: the table takes what the source holds
                    // and owes. What fits goes now, after the flush; the rest
                    // is owed to the sink.
                    Route::Sink(mut table) => {
                        let target = flush_target(plan, inject, has_inject, at);
                        let Some((u, shell)) = target else {
                            closer.route = Route::Sink(table);
                            continue;
                        };
                        for h in owing {
                            table.merge(h);
                        }
                        table.drain_into(&mut shell.events);
                        if table.is_empty() {
                            closer.route = Route::Sink(table);
                        } else {
                            let lost = shell.owe(table, &mut st.spare_tables);
                            self.dropped.at_node(u, lost);
                        }
                    }
                    Route::Placed => {}
                }
            }
            // The closes land at the end of what was flushed, after it.
            for shell in inject.iter_mut().filter(|i| i.events.len() > i.closed_from) {
                let (before, closes) = shell.events.split_at_mut(shell.closed_from);
                let end = before
                    .iter()
                    .map(|e| e.offset)
                    .max()
                    .unwrap_or(crate::node::time::Offset::ZERO);
                for e in closes {
                    e.offset = end;
                }
            }
        }
        // By offset, ties in flush order.
        for shell in inject.iter_mut().filter(|i| i.live) {
            sort_by_offset(&mut shell.events);
        }

        // What a crossfade queued behind a running one holds for the unit
        // that waits (`Node::chase`): delivered by `end_fades` into room made
        // with the plan. What its buffer refused is counted now, and what a
        // successor with no event input cannot take is dropped, counted.
        for (key, refused) in departed.held_refused.drain(..) {
            if let Some(u) = plan.unit_id(key) {
                self.dropped.at_node(u, refused);
            }
        }
        for &u in &st.queued {
            let unit = &plan.units[u];
            if unit.shape.event_in > 0 {
                continue;
            }
            if let Some(queued) = self
                .store
                .get_mut(unit.idx.0 as usize)
                .and_then(Option::as_mut)
                .and_then(|s| s.queued.as_mut())
                && !queued.chase.is_empty()
            {
                let lost = u32::try_from(queued.chase.len()).unwrap_or(u32::MAX);
                self.dropped.at_node(u, lost);
                queued.chase.clear();
            }
        }
        new
    }
}

/// The injection buffer for the sink `at`, marked as holding a flush: `None`
/// when the plan has no such input.
fn flush_target<'a>(
    plan: &Plan,
    inject: &'a mut [Inject],
    has_inject: &mut TableSlice<PlanUnitId, bool>,
    at: EventInputPort,
) -> Option<(PlanUnitId, &'a mut Inject)> {
    let id = plan.unit_id(at.node)?;
    if at.port >= plan.units[id].shape.event_in {
        return None;
    }
    let u = id;
    let i = inject
        .binary_search_by_key(&(u, at.port), |i| (i.unit, i.port))
        .ok()?;
    has_inject[u] = true;
    inject[i].live = true;
    Some((u, &mut inject[i]))
}

/// Appends `events` to `to` as far as its capacity takes. Returns how many
/// did not fit.
fn fill(to: &mut CappedVec<Event>, events: &[Event]) -> u32 {
    let kept = to.extend(events.iter().copied());
    u32::try_from(events.len() - kept).unwrap_or(u32::MAX)
}

/// `carried`, copied into `target`, the ring built for its new length: both
/// go on, the old one to `spare`. Returns how many events a FIFO that was
/// built too small could not keep.
fn move_ring<F: Float>(
    target: Option<&mut Ring<F>>,
    carried: Option<Ring<F>>,
    spare: &mut CappedVec<Ring<F>>,
) -> usize {
    match (target, carried) {
        (Some(Ring::Audio(n)), Some(Ring::Audio(o))) => {
            n.transfer_from(&o);
            place(spare, Ring::Audio(o));
            0
        }
        (Some(Ring::Event(n)), Some(Ring::Event(mut o))) => {
            let lost = n.transfer_from(&mut o);
            place(spare, Ring::Event(o));
            lost
        }
        (_, Some(o)) => {
            debug_assert!(false, "a carried ring has a ring built for it");
            let lost = match &o {
                Ring::Event(f) => f.queued(),
                Ring::Audio(_) => 0,
            };
            place(spare, o);
            lost
        }
        (_, None) => 0,
    }
}
