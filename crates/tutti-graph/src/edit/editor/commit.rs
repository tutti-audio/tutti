//! Compiling the spec and sending the plan: `commit`, `package`, `collect`.

use std::collections::BTreeMap;
use std::sync::Arc;
use tutti_buffers::Float;

use super::lifecycle::Committed;
use super::{BusLanes, CommitError, Editor, Fading, starting};
use crate::compile::plan::{Delta, Plan, Rewired};
use crate::compile::{CompileError, Shapes, compile};
use crate::exec::Commit;
use crate::exec::{Opened, StageIn};
use crate::model::gain::{EdgeFade, EdgeGain};
use crate::model::spec::{GraphSpec, ValidGraph};
use crate::node::Node;
use crate::node::param::{ParamPorts, ParamState};
use crate::{Edge, EdgeId, PortSink, PortSource};
use crate::{Fade, MainOutputBank, NodeKey};
use tutti_units::Gain;

impl<F: Float> Editor<F> {
    /// `Err(Poisoned)` once a re-prepare has failed with its units out.
    pub(super) fn check_poisoned(&self) -> Result<(), CommitError> {
        match self.poison_cause() {
            Some(cause) => Err(CommitError::Poisoned {
                cause: cause.to_owned(),
            }),
            None => Ok(()),
        }
    }

    /// Drains the boxes the executor sent back and frees what they retired,
    /// here on the control thread. Returns the retired units' keys, one per
    /// unit: those a commit retired, and those a crossfade retired — an
    /// outgoing unit whose fade ended or was cut (a re-prepare cuts every
    /// fade), or a waiting one superseded before it ran.
    ///
    /// When the box coming back is a [`reprepare`](Self::reprepare)'s first
    /// half, this re-prepares the units it carries and sends the second.
    ///
    /// Items an executor apply had no room to hand back, leaked rather than
    /// freed on the audio thread, are logged with `tracing::error!` once per
    /// increase: such a count is a tutti bug.
    pub fn collect(&mut self) -> Vec<NodeKey> {
        let mut keys = Vec::new();
        while let Some(done) = self.link.pop_returned() {
            if done.is_suspend() {
                self.resume(done);
                continue;
            }
            keys.extend(done.retired());
            drop(done);
        }
        // The executor applies a commit before it hands the box back, so a
        // tap the commit left out has had its last block written.
        self.link.end_applied_taps();
        while let Some(x) = self.link.pop_faded() {
            keys.extend(std::iter::repeat_n(x.key(), x.units()));
            self.stager.faded(x.key());
            drop(x);
        }
        self.stager.observe(self.link.acked());
        // Control thread: the audio thread only counts what its lists refused.
        let leaked = self.link.take_leaked();
        if leaked > 0 {
            tracing::error!(
                leaked,
                "an executor apply had no room to hand back {leaked} owned items and leaked them; this is a tutti bug"
            );
        }
        keys
    }

    /// `Backpressure` if the crossfades `delta` starts would take more than
    /// [`FADE_CAPACITY`](crate::FADE_CAPACITY) slots on the fade-return ring.
    fn fade_room(&self, delta: &Delta) -> Result<(), CommitError> {
        self.link.check_fade_room(starting(delta))
    }

    /// Refuses a main output generation the attached host has no folds for.
    fn check_main_output_published(&self) -> Result<(), CommitError> {
        if !self.main_output.publishes(self.spec.main_output_id) {
            return Err(CommitError::MainOutputNotPublished {
                main_output: self.spec.main_output_id,
                outputs: self.spec.topology.outputs.len(),
            });
        }
        Ok(())
    }

    /// The cheap checks, before the spec is lowered: what [`commit`](Self::commit) asks even when nothing changed.
    fn check_spec(&self) -> Result<(), CommitError> {
        self.limits.outputs(self.spec.topology.outputs.len())?;
        self.check_main_output_published()?;
        self.check_tap_widths()?;
        self.check_meter_widths()?;
        self.check_passes()
    }

    /// Validates, compiles against the plan sent last, and sends the result
    /// with the units it places. Returns `Backpressure` — having compiled and sent
    /// nothing — when [`QUEUE_CAPACITY`](crate::QUEUE_CAPACITY) commits are out, or when the fades
    /// it starts would put more than [`FADE_CAPACITY`](crate::FADE_CAPACITY) in flight. A running
    /// fade holds neither: its commit comes back when applied, and its slot
    /// on the fade-return ring is its own.
    ///
    /// A commit that changes nothing structural sends nothing and returns
    /// `Ok`: the spec and shapes those of the last commit, no unit pending,
    /// no crossfade, latency cut, tap, event tap or meter waiting. It would
    /// compile the plan the executor already has, with an empty delta. So it
    /// costs a comparison of two counters (the editor's structure against
    /// the one that commit recorded) when no spec or shape was written since,
    /// and a comparison of the contents when one was, not a compile, and a
    /// host may commit every frame. A param range, a base, a mode or a mix is
    /// a control, not structure: it is stored into the running unit's cells
    /// when set (see [`set_param_range`](Self::set_param_range)) and never
    /// moves the counter.
    ///
    /// **A range and a rewire in one commit** do not land together: the
    /// commit writes every range cell at once, and the plan lands when the
    /// executor next drains its queue, so the old wiring can run a block
    /// under the new range. A range is a clamp the node may hear at any
    /// block, so that block is a legal state, not a torn one.
    ///
    /// Returns the nodes whose audio wiring the commit changed (see
    /// [`Rewired`]): empty when nothing was sent. The plan the executor has
    /// answers [`Io::wired_inputs`](crate::Io::wired_inputs) from the block
    /// that applies this commit; a node that reacts on the control side (a
    /// plugin's bus activation) acts on this list.
    ///
    /// # Errors
    ///
    /// Nothing is sent, and the plan sent last stays the base, on
    /// [`CommitError::Invalid`] (the spec is malformed),
    /// [`CommitError::Compile`], [`CommitError::MissingUnit`] (a node with no
    /// unit to place), [`CommitError::Backpressure`],
    /// [`CommitError::Repreparing`], [`CommitError::Poisoned`] or a
    /// [`Limits`](crate::Limits) refusal ([`CommitError::TooManyOutputs`]).
    #[must_use = "the nodes this commit rewired"]
    #[allow(
        clippy::missing_panics_doc,
        reason = "every pending unit the delta needs was checked present just before it is taken"
    )]
    pub fn commit(&mut self) -> Result<Vec<Rewired>, CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        self.link.check_room()?;
        self.check_spec()?;
        if let Some(c) = self.life.committed()
            && !self.entries.is_staged()
            && !self.taps.has_pending()
            && !self.event_taps.has_pending()
            && !self.meters.has_pending()
            // A plan whose main output crossfade has ended is compiled again without
            // the old main output's channels.
            && !self.main_output.fade_ended()
        {
            // An equal structure is the spec and shapes committed. A moved
            // one may have come back to them (a route dropped and added
            // again), which the contents say.
            if c.structure == self.structure {
                // A write that skipped the guards would leave the structure
                // equal and the spec changed.
                debug_assert!(
                    c.spec.same_plan(&self.spec) && *c.shapes == *self.shapes,
                    "a write bypassed the structural guards"
                );
                return Ok(Vec::new());
            }
            if c.spec.same_plan(&self.spec) && *c.shapes == *self.shapes {
                return Ok(Vec::new());
            }
        }
        let (valid, weights, fading) = self.lower_with_bank()?;
        let (plan, mut delta) = compile(
            &valid,
            &self.shapes,
            &self.prepare,
            self.base().map(|p| &**p),
        )
        .map_err(CommitError::Compile)?;
        // The crossfades `replace` asked for, for the keys this delta
        // replaces (a key removed since has none to carry).
        let fades = delta
            .replace()
            .iter()
            .filter_map(|&(_, new)| self.entries.fade(new.key).map(|f| (new.key, f)))
            .collect();
        // The keys `set_latency` changed that this delta keeps: their
        // crossfades are cut (see `Delta::cuts`).
        let base = self.life.plan().map(|p| &**p);
        let cuts = self
            .entries
            .latency_cuts()
            .filter_map(|key| {
                let now = plan.unit(key)?;
                let was = base?.unit(key)?;
                (was.generation == now.generation && was.idx == now.idx).then(|| now.placement())
            })
            .collect();
        delta.attach(fades, cuts);
        debug_assert_eq!(
            plan.verify_fades(self.base().map(|p| &**p), &delta),
            Ok(()),
            "`replace` checked every fade it attached"
        );
        let needed: Vec<NodeKey> = delta
            .insert()
            .iter()
            .map(|p| p.key)
            .chain(delta.replace().iter().map(|(_, new)| new.key))
            .collect();
        // Check before taking anything, so a failed commit leaves every
        // pending unit where it was.
        if let Some(&node) = needed.iter().find(|&&k| !self.entries.has_unit(k)) {
            return Err(CommitError::MissingUnit { node });
        }
        self.fade_room(&delta)?;
        let units: BTreeMap<NodeKey, Box<dyn Node<F>>> = needed
            .into_iter()
            .map(|k| (k, self.entries.take_unit(k).expect("checked above")))
            .collect();
        self.entries.clear_fades_and_cuts();
        let rewired = delta.take_rewired();
        self.send(plan, delta, units, &weights, false);
        self.main_output.fading = fading;
        self.entries.sync(&self.values);
        self.gains.sync(&self.spec, &self.values);
        self.life.set_committed(Committed {
            structure: self.structure,
            spec: Arc::clone(&self.spec),
            shapes: Arc::clone(&self.shapes),
        });
        // A lane the spec no longer has takes its base and range with it.
        let spec = &self.spec;
        self.values.retain_edges(|id| spec.gains.contains_key(&id));
        Ok(rewired)
    }

    /// How a commit that changes the main output's generation trades the old main output
    /// for the new: the old main output's channels are rendered beside the new main output's
    /// for the `fade`'s length, and the host folds each with its own main output's
    /// fold and mixes them by the fade's gains ([`MainOutputBank`]).
    /// `None` (the default) cuts at the commit's block, as does a zero
    /// duration. Read when a commit that changes the main output is compiled.
    ///
    /// A crossfade is one fade of one law and length, counted from the block
    /// that applies the commit that began it:
    ///
    /// - A commit that leaves the main output's generation alone continues it: the
    ///   frames already rendered stay counted, and the old main output's channels
    ///   keep the wiring they had when the fade began, less the sources
    ///   removed since.
    /// - A commit that declares the main output again ends it at that block and
    ///   begins another, of the fade set now, from the main output that was fading
    ///   in (at full level) to the newest: the main output that was fading out is
    ///   dropped, as a plan holds two main outputs at most. That is two steps of
    ///   `1 - g`, `g` the dropped main output's gain at the block: a caller that
    ///   wants no step declares the new main output after [`finish_fades`](Self::finish_fades)
    ///   reports the running fade over (the session does).
    ///
    /// While a crossfade runs the editor shares the committed spec and
    /// shapes, the old main output's wiring, until it ends.
    pub fn set_main_output_fade(&mut self, fade: Option<Fade>) {
        self.main_output.fade = fade.filter(|f| !f.is_cut());
    }

    /// The spec lowered and validated, as [`lower_and_validate`](Self::lower_and_validate)
    /// does, and, when this commit changes the main output's generation and a main output
    /// fade is set, with the previous main output's output channels after the new
    /// main output's: the old main output's wiring as it was committed, over the nodes that
    /// are still here. Its lanes are static at the weights they had.
    fn lower_with_bank(&self) -> Result<(ValidGraph, BusLanes, Option<Fading>), CommitError> {
        let plain = || {
            Self::lower_and_validate(&self.spec, &self.shapes)
                .map(|(valid, lanes)| (valid, lanes, None))
        };
        let committed = self.life.committed();
        // The crossfade running, when this commit leaves its main output alone and
        // its channels stand.
        let continuing = self.main_output.continuing(
            self.spec.main_output_id,
            committed.map(|c| c.spec.main_output_id),
        );
        let (fade, was, published) = match (continuing, self.main_output.fade, committed) {
            (Some(f), _, _) => {
                debug_assert_eq!(f.bank.base, self.spec.topology.outputs.len());
                (f.bank.fade, &f.was, true)
            }
            (None, Some(fade), Some(c)) if c.spec.main_output_id != self.spec.main_output_id => {
                (fade, c, false)
            }
            _ => return plain(),
        };
        // The bases the old main output's lanes had when the crossfade began:
        // the running crossfade's own, or the table's now.
        let bases: BTreeMap<EdgeId, Gain> = continuing.map_or_else(
            || {
                was.spec
                    .gains
                    .keys()
                    .map(|&id| (id, self.values.base(id)))
                    .collect()
            },
            |f| f.bases.clone(),
        );
        let fades: BTreeMap<EdgeId, EdgeFade> = continuing.map_or_else(
            || {
                was.spec
                    .gains
                    .keys()
                    .map(|&id| (id, self.values.fade(id)))
                    .filter(|&(_, fade)| fade != EdgeFade::None)
                    .collect()
            },
            |f| f.fades.clone(),
        );
        let lower = |spec: &GraphSpec, shapes: &Shapes| {
            spec.lowered(shapes)
                .map_err(|e| CommitError::Compile(CompileError::Bus(e)))
        };
        let mut now = lower(&self.spec, &self.shapes)?;
        let before = lower(&was.spec, &was.shapes)?;
        let base = now.spec.topology.outputs.len();
        let old_width = before.spec.topology.outputs.len();
        let published = published || self.main_output.publishes(was.spec.main_output_id);
        if base == 0 || old_width == 0 || !published {
            let valid = now.spec.validate().map_err(CommitError::Invalid)?;
            return Ok((valid, now.weights(), None));
        }
        let alive = |source: PortSource| match source {
            PortSource::Node(p) => self
                .shapes
                .get(&p.node)
                .is_some_and(|shape| usize::from(p.port) < usize::from(shape.audio_out().count())),
            PortSource::MainInput(ch) => ch < now.spec.topology.inputs.count(),
            PortSource::Zero => false,
        };
        let mut kept = false;
        for (c, sources) in before.spec.topology.outputs.iter().enumerate() {
            let mut bank_sources = Vec::new();
            for &source in sources.iter().filter(|&&s| alive(s)) {
                let was_id = EdgeId {
                    sink: PortSink::Output(c as u16),
                    from: Edge::Direct(source),
                };
                if fades.get(&was_id) == Some(&EdgeFade::Out) {
                    continue;
                }
                let id = EdgeId {
                    sink: PortSink::Output((base + c) as u16),
                    from: Edge::Direct(source),
                };
                let weight = before
                    .lanes
                    .get(&was_id)
                    .or_else(|| bases.get(&was_id))
                    .copied()
                    .unwrap_or(Gain::UNITY);
                if weight != Gain::UNITY {
                    now.spec.gains.insert(id, EdgeGain::default());
                }
                now.lanes.insert(id, weight);
                bank_sources.push(source);
            }
            kept |= !bank_sources.is_empty();
            now.spec.topology.outputs.push(bank_sources);
        }
        if !kept {
            now.spec.topology.outputs.truncate(base);
            now.lanes
                .retain(|id, _| !matches!(id.sink, PortSink::Output(c) if usize::from(c) >= base));
            now.spec
                .gains
                .retain(|id, _| !matches!(id.sink, PortSink::Output(c) if usize::from(c) >= base));
        }
        let bank_width = if kept { old_width } else { 0 };
        let valid = now
            .spec
            .validate_banked(bank_width)
            .map_err(CommitError::Invalid)?;
        if !kept {
            return Ok((valid, now.weights(), None));
        }
        let bank = MainOutputBank {
            old: was.spec.main_output_id,
            base,
            fade,
            continues: continuing.is_some(),
        };
        let fading = Fading {
            was: was.clone(),
            bases,
            fades,
            main_output: self.spec.main_output_id,
            bank,
        };
        Ok((
            valid.with_main_output_bank(bank),
            now.weights(),
            Some(fading),
        ))
    }

    /// Sends a plan compiled elsewhere — against [`base`](Self::base) — with
    /// the units its delta places, as this editor's next commit. For a caller
    /// that compiles itself (a test harness); [`commit`](Self::commit) is the
    /// usual path.
    ///
    /// # Errors
    ///
    /// Nothing is sent on [`CommitError::Poisoned`],
    /// [`CommitError::Repreparing`], [`CommitError::Backpressure`],
    /// [`CommitError::TooManyOutputs`], [`CommitError::StaleDelta`] when the
    /// delta was compiled against another plan than [`base`](Self::base), or
    /// [`CommitError::Fade`] when the delta's crossfades fail
    /// [`Plan::verify_fades`].
    pub fn package(
        &mut self,
        plan: Plan,
        delta: Delta,
        units: BTreeMap<NodeKey, Box<dyn Node<F>>>,
    ) -> Result<(), CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        self.link.check_room()?;
        self.limits.outputs(plan.main_outputs())?;
        if delta.carry.prev != self.base().map(|p| p.id.0) {
            return Err(CommitError::StaleDelta);
        }
        self.fade_room(&delta)?;
        plan.verify_fades(self.base().map(|p| &**p), &delta)
            .map_err(CommitError::Fade)?;
        self.life.clear_committed();
        // The plan was compiled elsewhere, from the spec as it stands (as a
        // caller that lowers it must have).
        let weights = self.lowered().map(|l| l.weights()).unwrap_or_default();
        self.send(plan, delta, units, &weights, false);
        self.main_output.fading = None;
        Ok(())
    }

    /// Builds the commit of `plan` and sends it: the caller checked the
    /// link's room (`check_room`, `fade_room`), so the send cannot fail.
    /// `resumed` is a re-prepare's second half.
    pub(super) fn send(
        &mut self,
        plan: Plan,
        delta: Delta,
        units: BTreeMap<NodeKey, Box<dyn Node<F>>>,
        bus_weights: &BusLanes,
        resumed: bool,
    ) {
        self.touch();
        let fades = starting(&delta);
        self.main_output
            .sent(plan.main_output_bank(), self.link.next_seq());
        let plan = Arc::new(plan);
        for &key in units.keys() {
            let declared = plan.unit(key).map_or(ParamPorts::NONE, |u| u.shape.params);
            self.entries.stage_cells(key, declared, &self.values);
        }
        let gains = self
            .gains
            .for_plan(&plan, &self.spec, &self.values, bus_weights);
        let entries = &self.entries;
        let params = |key: NodeKey, ports: usize, max: usize| entries.param_state(key, ports, max);
        let modes = |key: NodeKey| entries.mode_state(key, &plan);
        // A unit handed in whole (`send`'s callers) was never bound: its flag
        // is a fresh one nothing holds.
        let wakes = |key: NodeKey| entries.wake(key).cloned().unwrap_or_default();
        let taps = self.taps.take_pending();
        let event_taps = self.event_taps.take_pending();
        let meters = self.meters.take_pending();
        let mut commit = Commit::build(
            self.link.next_seq(),
            Arc::clone(&plan),
            delta,
            resumed,
            units,
            params,
            modes,
            wakes,
            Opened {
                gains,
                taps,
                event_taps,
                meters,
            },
        );
        // Everything applying needs that is a function of the plan is built
        // here, so the audio thread only swaps and moves.
        let max = plan.prepare.max_block().get();
        let old = self.life.plan().map(Arc::clone);
        self.stager.stage(
            &mut commit,
            StageIn {
                old: old.as_deref(),
                mix_lane: &|key| {
                    entries
                        .mode(key)
                        .map(|cell| ParamState::lane(max, cell.ranges()))
                },
            },
        );
        self.entries.keep_placed(&plan, &self.values);
        self.link.send(commit, fades);
        // `commit` syncs the cells against the spec it last sent, then
        // records this one.
        self.life.sent(plan);
    }
}
