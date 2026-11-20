//! Compiling the spec and sending the plan: `commit`, `package`, `collect`.

use std::collections::BTreeMap;
use std::sync::Arc;
use tutti_buffers::Float;

use tutti_curve::declick_frames;

use super::lifecycle::Resume;
use super::{BusLanes, CommitError, Editor, Fading, SentBank, starting};
use crate::compile::{CompileError, Shapes, compile};
use crate::exec::Commit;
use crate::exec::{Opened, StageIn};
use crate::gain::{EdgeFade, EdgeGain};
use crate::node::{Node, Wired};
use crate::param::ParamState;
use crate::plan::{Delta, Placement, Plan};
use crate::spec::{GraphSpec, ValidGraph};
use crate::{Edge, EdgeId, PortSink, PortSource};
use crate::{Fade, NodeKey, RootBank};
use tutti_units::Amplitude;

/// A node whose audio wiring changed in a commit: a port gained its first
/// source or consumer, or lost its last.
///
/// From the plans, not the spec: the old and the new plan's
/// [`PlanUnit::wired`](crate::PlanUnit::wired) for the key differ. A node
/// the commit placed with no wiring, a node it removed and a rewire that
/// leaves each port's answer as it was are not listed. A re-insert at the
/// same key with a different shape but the same masks reports nothing: bus
/// layout changes reach a plugin through its own re-activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rewired {
    /// The node.
    pub node: NodeKey,
    /// Its wired ports in the plan sent before (none for a node new to it).
    pub was: Wired,
    /// Its wired ports in the plan this commit sent.
    pub now: Wired,
}

impl Rewired {
    /// The nodes of `new` whose wired ports differ from `old`'s, in key
    /// order.
    fn between(old: Option<&Plan>, new: &Plan) -> Vec<Self> {
        new.units()
            .iter()
            .filter_map(|u| {
                let was = old
                    .and_then(|p| p.unit(u.key))
                    .map_or_else(Wired::default, |o| o.wired);
                (was != u.wired).then_some(Self {
                    node: u.key,
                    was,
                    now: u.wired,
                })
            })
            .collect()
    }
}

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
        keys
    }

    /// `Backpressure` if the crossfades `delta` starts would take more than
    /// [`FADE_CAPACITY`](crate::FADE_CAPACITY) slots on the fade-return ring.
    fn fade_room(&self, delta: &Delta) -> Result<(), CommitError> {
        self.link.check_fade_room(starting(delta))
    }

    /// Whether the spec as it stands would be accepted by
    /// [`commit`](Self::commit): the refusals that depend on the spec alone,
    /// checked now and without sending anything or changing the editor.
    ///
    /// It checks, in order, the [`Limits`](crate::Limits) on the root's width, that every
    /// open tap and meter still fits the reader opened for it, that every
    /// node that may be bypassed or mixed has a dry path, and that the spec
    /// lowers (its bus edges) and validates as a graph. It does not look at
    /// the editor's state or at the units: a [`CommitError::Poisoned`],
    /// [`CommitError::Repreparing`], [`CommitError::Backpressure`],
    /// [`CommitError::MissingUnit`] or a compile against the plan sent last
    /// is `commit`'s to report. A host that edits and wants to know whether
    /// the edit stands asks here, then commits.
    ///
    /// # Errors
    ///
    /// [`CommitError::TooManyOutputs`], [`CommitError::TapWidth`],
    /// [`CommitError::MeterWidth`], [`CommitError::Unplaced`] (a node whose
    /// dry path is not declared), [`CommitError::Invalid`] or
    /// [`CommitError::Compile`] (the bus edges do not lower): the same
    /// values `commit` returns for them.
    pub fn validate(&self) -> Result<(), CommitError> {
        self.check_spec()?;
        Self::lower_and_validate(&self.spec, &self.shapes).map(|_| ())
    }

    /// Refuses a root generation the attached host has no folds for.
    fn check_root_published(&self) -> Result<(), CommitError> {
        let outputs = self.spec.topology.outputs.len();
        match &self.root_gate {
            Some(gate) if outputs > 0 && !gate.published(self.spec.root_id, outputs) => {
                Err(CommitError::RootNotPublished {
                    root: self.spec.root_id,
                    outputs,
                })
            }
            _ => Ok(()),
        }
    }

    /// The cheap checks of [`validate`](Self::validate), before the spec is
    /// lowered: what [`commit`](Self::commit) asks even when nothing changed.
    fn check_spec(&self) -> Result<(), CommitError> {
        self.limits.outputs(self.spec.topology.outputs.len())?;
        self.check_root_published()?;
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
    /// A commit that changes nothing — the spec and shapes those of the last
    /// commit, no unit pending, no crossfade or latency cut waiting — sends
    /// nothing and returns `Ok`: it would compile the plan the executor
    /// already has, with an empty delta. So it costs a comparison, not a
    /// compile, and a host may commit every frame. Nor does one whose spec
    /// differs only in param **ranges**: those are controls, not plan, so it
    /// validates the spec, writes the ranges to the running units (see
    /// [`set_param_range`](Self::set_param_range)) and sends nothing. That
    /// still costs the validate and a clone of the spec, on this thread;
    /// [`set_param_range`](Self::set_param_range) costs a store.
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
        if !self.placements.is_staged()
            && !self.taps.has_pending()
            && !self.event_taps.has_pending()
            && !self.meters.has_pending()
            // A plan whose root crossfade has ended is compiled again without
            // the old root's channels.
            && !self.bank.is_some_and(|b| b.ended)
            && self
                .life
                .committed()
                .is_some_and(|c| c.shapes == self.shapes && c.spec.same_but_ranges(&self.spec))
        {
            // The plan the executor has is this spec's: at most a range
            // moved, which is a control.
            if self.life.committed().is_some_and(|c| c.spec != self.spec) {
                Self::lower_and_validate(&self.spec, &self.shapes)?;
                self.sync_ranges();
                self.life
                    .set_committed(self.spec.clone(), self.shapes.clone());
            }
            return Ok(Vec::new());
        }
        let (valid, weights, fading) = self.lower_with_bank()?;
        let (mut plan, mut delta) = compile(
            &valid,
            &self.shapes,
            &self.prepare,
            self.base().map(|p| &**p),
        )
        .map_err(CommitError::Compile)?;
        plan.root_id = self.spec.root_id;
        // The crossfades `replace` asked for, for the keys this delta
        // replaces (a key removed since has none to carry).
        delta.fades = delta
            .replace
            .iter()
            .filter_map(|&(_, new)| self.placements.fade(new.key).map(|f| (new.key, f)))
            .collect();
        // The keys `set_latency` changed that this delta keeps: their
        // crossfades are cut (see `Delta::cuts`).
        let base = self.life.plan().map(|p| &**p);
        delta.cuts = self
            .placements
            .latency_cuts()
            .filter_map(|key| {
                let now = plan.unit(key)?;
                let was = base?.unit(key)?;
                (was.generation == now.generation && was.idx == now.idx).then_some(Placement {
                    key,
                    generation: now.generation,
                    idx: now.idx,
                })
            })
            .collect();
        debug_assert_eq!(
            plan.verify_fades(self.base().map(|p| &**p), &delta),
            Ok(()),
            "`replace` checked every fade it attached"
        );
        let needed: Vec<NodeKey> = delta
            .insert
            .iter()
            .map(|p| p.key)
            .chain(delta.replace.iter().map(|(_, new)| new.key))
            .collect();
        // Check before taking anything, so a failed commit leaves every
        // pending unit where it was.
        if let Some(&node) = needed.iter().find(|&&k| !self.placements.has_unit(k)) {
            return Err(CommitError::MissingUnit { node });
        }
        self.fade_room(&delta)?;
        let units: BTreeMap<NodeKey, Box<dyn Node<F>>> = needed
            .into_iter()
            .map(|k| (k, self.placements.take_unit(k).expect("checked above")))
            .collect();
        self.placements.clear_fades_and_cuts();
        let rewired = Rewired::between(self.base().map(|p| &**p), &plan);
        self.send(plan, delta, units, &weights, None);
        self.fading = fading;
        self.sync_ranges();
        self.life
            .set_committed(self.spec.clone(), self.shapes.clone());
        Ok(rewired)
    }

    /// How a commit that changes the root's generation trades the old root
    /// for the new: the old root's channels are rendered beside the new root's
    /// for the `fade`'s length, and the host folds each with its own root's
    /// fold and mixes them by the fade's gains ([`RootBank`]).
    /// `None` (the default) cuts at the commit's block, as does a zero
    /// duration. Read when a commit that changes the root is compiled.
    ///
    /// A crossfade is one fade of one law and length, counted from the block
    /// that applies the commit that began it:
    ///
    /// - A commit that leaves the root's generation alone continues it: the
    ///   frames already rendered stay counted, and the old root's channels
    ///   keep the wiring they had when the fade began, less the sources
    ///   removed since.
    /// - A commit that declares the root again ends it at that block and
    ///   begins another, of the fade set now, from the root that was fading
    ///   in (at full level) to the newest: the root that was fading out is
    ///   dropped, as a plan holds two roots at most. That is two steps of
    ///   `1 - g`, `g` the dropped root's gain at the block: a caller that
    ///   wants no step declares the new root after [`finish_fades`](Self::finish_fades)
    ///   reports the running fade over (the session does).
    /// - A commit that changes the root's width without declaring it again
    ///   ends the crossfade running as a cut: the old root's channels cannot
    ///   stand beside a root of another width.
    ///
    /// While a crossfade runs the editor holds a clone of the committed spec
    /// and shapes, the old root's wiring, until it ends.
    pub fn set_root_fade(&mut self, fade: Option<Fade>) {
        self.root_fade = fade.filter(|f| !f.is_cut());
    }

    /// The fade a root change takes ([`set_root_fade`](Self::set_root_fade)).
    pub fn root_fade(&self) -> Option<Fade> {
        self.root_fade
    }

    /// The spec lowered and validated, as [`lower_and_validate`](Self::lower_and_validate)
    /// does, and, when this commit changes the root's generation and a root
    /// fade is set, with the previous root's output channels after the new
    /// root's: the old root's wiring as it was committed, over the nodes that
    /// are still here. Its lanes are static at the weights they had.
    fn lower_with_bank(&self) -> Result<(ValidGraph, BusLanes, Option<Fading>), CommitError> {
        let plain = || {
            Self::lower_and_validate(&self.spec, &self.shapes)
                .map(|(valid, lanes)| (valid, lanes, None))
        };
        let committed = self.life.committed();
        // The crossfade running, when this commit leaves its root alone and
        // its channels stand.
        let continuing = self.fading.as_ref().filter(|f| {
            f.root == self.spec.root_id
                && committed.is_some_and(|c| c.spec.root_id == self.spec.root_id)
                && !self.bank.is_some_and(|b| b.ended)
                && f.bank.base == self.spec.topology.outputs.len()
        });
        let (fade, was, published) = match (continuing, self.root_fade, committed) {
            (Some(f), _, _) => (f.bank.fade, &f.was, true),
            (None, Some(fade), Some(c)) if c.spec.root_id != self.spec.root_id => (fade, c, false),
            _ => return plain(),
        };
        let lower = |spec: &GraphSpec, shapes: &Shapes| {
            spec.lowered(shapes)
                .map_err(|e| CommitError::Compile(CompileError::Bus(e)))
        };
        let mut now = lower(&self.spec, &self.shapes)?;
        let before = lower(&was.spec, &was.shapes)?;
        let base = now.spec.topology.outputs.len();
        let old_width = before.spec.topology.outputs.len();
        let published = published
            || self
                .root_gate
                .as_ref()
                .is_none_or(|gate| gate.published(was.spec.root_id, old_width));
        if base == 0 || old_width == 0 || !published {
            let valid = now.spec.validate().map_err(CommitError::Invalid)?;
            return Ok((valid, now.weights(), None));
        }
        let alive = |source: PortSource| match source {
            PortSource::Node(p) => self
                .shapes
                .get(&p.node)
                .is_some_and(|shape| usize::from(p.port) < usize::from(shape.audio_out().count())),
            PortSource::Global(ch) => ch < now.spec.topology.inputs.count(),
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
                let gain = before.spec.gains.get(&was_id);
                if gain.is_some_and(|g| g.fade == EdgeFade::Out) {
                    continue;
                }
                let id = EdgeId {
                    sink: PortSink::Output((base + c) as u16),
                    from: Edge::Direct(source),
                };
                let weight = gain.map_or(Amplitude::UNITY, |g| g.base);
                if weight != Amplitude::UNITY {
                    now.spec.gains.insert(id, EdgeGain::new(weight));
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
        let valid = now.spec.validate().map_err(CommitError::Invalid)?;
        if !kept {
            return Ok((valid, now.weights(), None));
        }
        let bank = RootBank {
            old: was.spec.root_id,
            base,
            fade,
        };
        let fading = Fading {
            was: was.clone(),
            root: self.spec.root_id,
            bank,
        };
        Ok((valid.with_root_bank(bank), now.weights(), Some(fading)))
    }

    /// Sends a plan compiled elsewhere — against [`base`](Self::base) — with
    /// the units its delta places, as this editor's next commit. For a caller
    /// that compiles itself (a test harness driving two interpreters from one
    /// spec); [`commit`](Self::commit) is the usual path.
    ///
    /// # Errors
    ///
    /// Nothing is sent on [`CommitError::Poisoned`],
    /// [`CommitError::Repreparing`], [`CommitError::Backpressure`],
    /// [`CommitError::TooManyOutputs`], or [`CommitError::Fade`] when the
    /// delta's crossfades fail [`Plan::verify_fades`].
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
        self.limits.outputs(plan.global_outputs())?;
        self.fade_room(&delta)?;
        plan.verify_fades(self.base().map(|p| &**p), &delta)
            .map_err(CommitError::Fade)?;
        self.life.clear_committed();
        // The plan was compiled elsewhere, from the spec as it stands (as a
        // caller that lowers it must have).
        let weights = self.lowered().map(|l| l.weights()).unwrap_or_default();
        self.send(plan, delta, units, &weights, None);
        self.fading = None;
        Ok(())
    }

    /// Builds the commit of `plan` and sends it: the caller checked the
    /// link's room (`check_room`, `fade_room`), so the send cannot fail.
    /// `resume` is a re-prepare's second half.
    pub(super) fn send(
        &mut self,
        plan: Plan,
        delta: Delta,
        units: BTreeMap<NodeKey, Box<dyn Node<F>>>,
        bus_weights: &BusLanes,
        resume: Option<Resume>,
    ) {
        self.touch();
        let fades = starting(&delta);
        // A plan that carries the crossfade the last one carried continues
        // it: the executor keeps its progress, and it ends under the seq that
        // began it.
        self.bank = plan.root_bank().map(|bank| {
            let (old, root) = (bank.old, plan.root_id());
            self.bank
                .filter(|b| !b.ended && b.old == old && b.root == root)
                .unwrap_or(SentBank {
                    seq: self.link.next_seq(),
                    old,
                    root,
                    ended: false,
                })
        });
        if self.bank.is_none() {
            self.fading = None;
        }
        let plan = Arc::new(plan);
        let committed = self.life.committed().map(|c| &c.spec);
        let spec = &self.spec;
        let (ranges, modes_cells) = (&mut self.cells.ranges, &mut self.cells.modes);
        let declick = declick_frames(plan.prepare.sample_rate()).get();
        let params = |key: NodeKey, ports: usize, max: usize| {
            let declared = plan.unit(key).map(|u| u.shape.params);
            ranges.state(key, ports, max, declick, declared, spec)
        };
        let gains = self
            .cells
            .gains
            .for_plan(&plan, spec, committed, bus_weights);
        let modes = |key: NodeKey| modes_cells.state(key, &plan, spec, committed);
        let placements = &self.placements;
        // A unit handed in whole (`send`'s callers) was never bound: its flag
        // is a fresh one nothing holds.
        let wakes = |key: NodeKey| placements.wake(key).cloned().unwrap_or_default();
        let taps = self.taps.take_pending();
        let event_taps = self.event_taps.take_pending();
        let meters = self.meters.take_pending();
        let mut commit = Commit::build(
            self.link.next_seq(),
            Arc::clone(&plan),
            delta,
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
        let (max, declick) = (plan.prepare.max_block().get(), declick);
        let mode_cells = &self.cells.modes;
        let old = self.life.plan().map(Arc::clone);
        self.stager.stage(
            &mut commit,
            StageIn {
                old: old.as_deref(),
                carry: resume.is_none_or(|r| r.carry),
                resumed: resume.is_some(),
                mix_lane: &|key| {
                    mode_cells
                        .get(key)
                        .map(|cell| ParamState::lane(max, declick, cell.ranges()))
                },
            },
        );
        self.cells.keep_placed(&plan, &self.spec);
        self.link.send(commit, fades);
        // `commit` syncs the cells against the spec it last sent, then
        // records this one.
        self.life.sent(plan);
    }
}
