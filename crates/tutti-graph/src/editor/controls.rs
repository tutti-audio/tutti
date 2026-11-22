//! The controls a running graph takes without a commit: param ranges, edge
//! gains and fades, pass states, modes and mixes.

use std::collections::BTreeMap;

use tutti_buffers::Float;
use tutti_rt::ParamCell;
use tutti_units::{Gain, Mix};

use super::{CommitError, Editor};
use crate::NodeKey;
use crate::bus_edge::{BusOutputPort, NodeBus};
use crate::gain::{EdgeFade, EdgeGain, EdgeGainError, GainCell};
use crate::mode::{MixError, ModeCell, ModeError, NodeMix, NodeMode};
use crate::param::{ParamIn, ParamRange, ParamRangeCell, ParamRangeError};
use crate::pass::Pass;
use crate::{Edge, EdgeId, PortSink};

impl<F: Float> Editor<F> {
    /// Sets param port `at`'s range — what its driven value is clamped
    /// to — **without a commit**: the editor records it, and the running unit
    /// clamps to it from its next block on, as it reads a base moved through
    /// its control. Nothing is compiled or sent, so a host may call this
    /// for every move of a range control.
    ///
    /// - **When it lands.** The executor reads each port's range once per
    ///   block, at the port's step: a range set between two blocks clamps
    ///   the whole of the next one, frame for frame. A range set while the
    ///   audio thread is inside a block lands on that block or the next.
    /// - **What it clamps.** The driven value only (the event source's
    ///   value, or the base before its first event): a port nothing drives
    ///   reads its base, unclamped, as it always did.
    /// - **What it never rewrites.** The node's base (its control) and the
    ///   event source's ramp run on unclamped; the clamp is applied as the
    ///   value is read. A range that shrinks around a held base or a ramp in
    ///   flight pins the value to its bound, and one that widens again lets
    ///   it go back to where the base and the ramp have got to by then.
    /// - **Before a commit.** The editor holds the range, so it is also the
    ///   initial range of a unit the next commit places, and a unit not yet
    ///   committed starts with it.
    /// - **No declick.** The value steps to the new bound on the block's
    ///   first frame, as it did when a range change was a commit. A host
    ///   that wants a range to ramp moves it in steps, or drives the param
    ///   instead.
    /// - **Cost.** A publish and a map lookup on the control side (the
    ///   publish allocates the new range; [`reclaim`](Self::reclaim) frees
    ///   the old one once no block holds it); nothing
    ///   on the audio thread but the per-block lend it always makes.
    /// - A port its node does not declare is recorded and clamps nothing.
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`ParamRangeError::InvalidRange`] for crossed
    /// bounds on a circular param (`ParamRange::arc` writes one), and
    /// [`ParamRangeError::NoSuchNode`] for a key with no node.
    pub fn set_param_range(
        &mut self,
        at: ParamIn,
        range: ParamRange,
    ) -> Result<(), ParamRangeError> {
        if !range.fits(at.param.param) {
            return Err(ParamRangeError::InvalidRange { at });
        }
        if !self.spec.topology.nodes.contains_key(&at.node) {
            return Err(ParamRangeError::NoSuchNode { node: at.node });
        }
        self.touch();
        self.values.set_range(at, range);
        if at.param == NodeMix::ADDR {
            // The node's mix: its range is the mode cell's.
            if let Some(cell) = self.cells.modes.get(at.node) {
                cell.store_range(range);
            }
        } else {
            self.cells.ranges.store(at, range);
        }
        Ok(())
    }

    /// A read-only handle on `at`'s live range cell, the very cell its port
    /// clamps to and [`set_param_range`](Self::set_param_range) publishes to:
    /// for a reader outside the graph (a control lane registers a reader on
    /// it and lends once per block). Control side. Writes go through
    /// `set_param_range`, which keeps the table and the cell together (a
    /// commit rewrites every cell from the table).
    ///
    /// `None` for a node not staged, or one whose unit does not declare
    /// `at.param`. The view stays the port's while the units placed at the
    /// key declare the same params; staging one with another list gives the
    /// key fresh cells, and an old view then reads ranges nothing writes. Ask
    /// again after such a replace (a rebind). From that replace to its commit,
    /// a range set reaches the new unit's cells only: the old unit, still
    /// running, keeps the range it had.
    pub fn param_range_cell(&self, at: ParamIn) -> Option<ParamRangeCell> {
        if at.param == NodeMix::ADDR {
            return self.cells.modes.get(at.node).map(ModeCell::range_cell);
        }
        self.cells.ranges.cell(at)
    }

    /// Frees the ranges [`set_param_range`](Self::set_param_range),
    /// [`set_edge_gain_range`](Self::set_edge_gain_range) and a commit
    /// replaced, once no block holds them; how many are still held by one.
    /// Control side, and cheap when nothing is retired: call it on a tick
    /// the owner already has (the session's reclaim phase).
    pub fn reclaim(&self) -> usize {
        self.cells.reclaim()
    }

    /// Moves edge `id`'s weight to `base` from the next block on, without a
    /// new plan: the control of a mixer's fader. It
    /// stores into the editor's value and the lane's cell and **marks nothing for
    /// commit**: the executor reads the cell once at the start of a block and
    /// ramps from the weight the last block was given to this one's across
    /// the block, so a move does not zipper. A move set while the audio
    /// thread is inside a block lands on the next.
    ///
    /// An edge with no entry yet gets one (with the default range): that is
    /// structural, and the next [`commit`](Self::commit) compiles the weighted
    /// sum. A lane's range is kept.
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`EdgeGainError::NotFinite`] for a base
    /// that is not finite, [`EdgeGainError::NoSuchEdge`] when `id` is not an
    /// edge of the spec, and [`EdgeGainError::BusLane`] for a lane of a bus
    /// edge.
    pub fn set_edge_gain(&mut self, id: EdgeId, base: Gain) -> Result<(), EdgeGainError> {
        if !base.get().is_finite() {
            return Err(EdgeGainError::NotFinite { id });
        }
        self.check_gain_edge(id)?;
        self.touch();
        self.declare_lane(id);
        self.values.set_base(id, base);
        if let Some(cell) = self.cells.gains.get(id) {
            cell.store(base);
        }
        Ok(())
    }

    /// Gives edge `id` a gain lane if it has none, starting at unity with the
    /// default range: a value left from an earlier lane is dropped. The lane's
    /// cell is made here, so it can be bound before a commit.
    pub(super) fn declare_lane(&mut self, id: EdgeId) {
        if !self.spec.gains.contains_key(&id) {
            self.values.forget_edge(id);
            self.spec.set_edge_gain(id, EdgeGain::default());
            self.cells.gains.declare(id, &self.values);
        } else if !self.cells.gains.has(id) {
            self.cells.gains.declare(id, &self.values);
        }
    }

    /// Makes `entries` the spec's gain entries, as a caller that builds the
    /// editor whole does (a builder, a fork, the test harness): an entry
    /// the spec did not hold is declared, its cell made from the values; one
    /// it held keeps its cell; one left out is dropped. The values are set
    /// first, so a new lane starts from them.
    pub(crate) fn enter_gains(&mut self, entries: BTreeMap<EdgeId, EdgeGain>) {
        self.touch();
        self.spec.gains.retain(|id, _| entries.contains_key(id));
        for (id, entry) in entries {
            if self.spec.gains.insert(id, entry).is_none() {
                self.cells.gains.declare(id, &self.values);
            }
        }
    }

    /// Sets edge `id`'s declick fade ([`EdgeFade`]). A value, like the base:
    /// it stores into the table and the lane's cell and marks nothing for
    /// commit, and the executor reads it at the start of a block. An edge with
    /// no entry gets one (unity, the default range), which is structural.
    ///
    /// The fade is the lane's: it starts when the kind the lane reads is not
    /// the one it last acted on, and its end is reported through
    /// [`finish_fades`](Self::finish_fades), which finishes it (an `Out` edge
    /// leaves the spec, an `In` one forgets the fade). A host that
    /// never sets one never has a fade.
    ///
    /// # Errors
    ///
    /// [`EdgeGainError::NoSuchEdge`] when `id` is not an edge of the spec,
    /// and [`EdgeGainError::BusLane`] for a lane of a bus edge (fade the bus
    /// edge with [`set_bus_fade`](Self::set_bus_fade)).
    pub fn set_edge_fade(&mut self, id: EdgeId, fade: EdgeFade) -> Result<(), EdgeGainError> {
        self.check_gain_edge(id)?;
        self.touch();
        self.declare_lane(id);
        self.values.set_fade(id, fade);
        if let Some(cell) = self.cells.gains.get(id) {
            cell.store_fade(fade);
        }
        Ok(())
    }

    /// Sets the declick fade of the bus edge `from -> to` (`None`: the main output):
    /// every lane of its fold fades by [`set_edge_fade`](Self::set_edge_fade)'s
    /// rule. Unlike an edge's, a bus edge's fade is part of the spec's
    /// structure (its lanes are made at the commit), so the next commit
    /// recompiles. Returns whether the bus edge was there and changed.
    pub fn set_bus_fade(
        &mut self,
        from: BusOutputPort,
        to: Option<NodeBus>,
        fade: EdgeFade,
    ) -> bool {
        self.touch();
        self.spec.set_bus_fade(from, to, fade)
    }

    /// Whether edge `id` has been sent in a plan, so that a fade out of it
    /// starts from a weight that was heard. An edge added since the last
    /// commit has not, and fading it out is just removing it.
    #[must_use]
    pub fn edge_is_live(&self, id: EdgeId) -> bool {
        self.life.plan().is_some() && self.life.committed().is_none_or(|c| c.spec.has_edge(id))
    }

    /// As [`edge_is_live`](Self::edge_is_live) for the bus edge `from -> to`
    /// (`None`: the main output).
    #[must_use]
    pub fn bus_edge_is_live(&self, from: BusOutputPort, to: Option<NodeBus>) -> bool {
        self.life.plan().is_some()
            && self
                .life
                .committed()
                .is_none_or(|c| c.spec.bus_fade(from, to).is_some())
    }

    /// Whether the plan sent last carries an old main output whose crossfade the
    /// executor has not yet been reported through ([`finish_fades`](Self::finish_fades)
    /// reports it): a declaration of the main output made now would end that fade at
    /// its block.
    #[must_use]
    pub fn main_output_fade_running(&self) -> bool {
        self.bank.is_some_and(|b| !b.ended)
    }

    /// Finishes, in the spec, every declick fade the executor has reported
    /// ended since the last call: an edge that faded `Out` is removed (with
    /// its entry, and a bus edge with its lanes), an edge or bus edge that
    /// faded `In` forgets the fade (and its entry, when it carries nothing
    /// else: a plain edge again), and a node
    /// [`remove_declicked`](Self::remove_declicked) kept goes once no edge it
    /// feeds is left. Returns whether the spec changed, which a caller commits.
    /// A report for an edge since reconnected, disconnected or given another
    /// fade is stale and ignored.
    ///
    /// Control thread. Cheap when nothing ended: a pop of an empty ring.
    pub fn finish_fades(&mut self) -> bool {
        let mut changed = false;
        while let Some((id, kind)) = self.link.pop_edge_fade() {
            changed |= self.settle_edge(id, kind);
        }
        let leaving = self.placements.leaving();
        if !leaving.is_empty() {
            let gone: Vec<NodeKey> = leaving
                .into_iter()
                .filter(|&k| !self.feeds_anything(k))
                .collect();
            for key in gone {
                self.placements.stop_leaving(key);
                self.remove(key);
                changed = true;
            }
        }
        // A main output change's crossfade the executor has rendered through: the
        // commit that follows compiles the old main output's channels away.
        let ended = self.link.main_output_fade_ended();
        if let Some(bank) = self.bank.as_mut().filter(|b| !b.ended && b.seq <= ended) {
            bank.ended = true;
            changed = true;
        }
        if changed {
            self.touch();
        }
        changed
    }

    fn settle_edge(&mut self, id: EdgeId, kind: EdgeFade) -> bool {
        if self.spec.has_edge(id) {
            let Some(g) = self
                .spec
                .gains
                .get(&id)
                .copied()
                .filter(|_| self.values.fade(id) == kind)
            else {
                return false;
            };
            return match kind {
                EdgeFade::Out => match (id.sink, id.from) {
                    (PortSink::Port(at), from) => self.spec.disconnect(at, from),
                    (PortSink::Output(c), Edge::Direct(s)) => {
                        self.spec.disconnect_output(usize::from(c), s)
                    }
                    (PortSink::Output(_), Edge::Feedback(_)) => false,
                },
                EdgeFade::In => {
                    let plain = g.source.is_none()
                        && self.values.base(id) == Gain::UNITY
                        && self.values.gain_range(id) == EdgeGain::DEFAULT_RANGE;
                    if plain {
                        self.spec.clear_edge_gain(id);
                        self.values.forget_edge(id);
                    } else {
                        self.values.set_fade(id, EdgeFade::None);
                    }
                    true
                }
                EdgeFade::None => false,
            };
        }
        if !self.spec.has_bus_edges() {
            return false;
        }
        let Some((from, to, fade)) = self.spec.feed_of_lane(&self.shapes, id) else {
            return false;
        };
        if fade != kind {
            return false;
        }
        match kind {
            EdgeFade::Out => match to {
                Some(to) => self.spec.disconnect_bus(from, to),
                None => self.spec.disconnect_bus_from_main_output(from),
            },
            EdgeFade::In => self.spec.set_bus_fade(from, to, EdgeFade::None),
            EdgeFade::None => false,
        }
    }

    /// Whether edge `id` is one the author may weight: an edge of the spec,
    /// and not a lane a bus edge produces (its weight is the fold's).
    ///
    /// # Errors
    ///
    /// [`EdgeGainError::NoSuchEdge`] when `id` is not an edge of the spec,
    /// and [`EdgeGainError::BusLane`] for a lane of a bus edge.
    pub fn check_gain_edge(&self, id: EdgeId) -> Result<(), EdgeGainError> {
        if self.spec.has_edge(id) {
            return Ok(());
        }
        if self.spec.has_bus_edges() && self.lowered().is_ok_and(|l| l.lanes.contains_key(&id)) {
            return Err(EdgeGainError::BusLane { id });
        }
        Err(EdgeGainError::NoSuchEdge { id })
    }

    /// Clamps edge `id`'s driven value to `range` from the next block on,
    /// without a new plan: [`set_param_range`](Self::set_param_range)'s
    /// contract for an edge gain (the cell and the editor's value move
    /// together; the base is never clamped; no declick). Like
    /// [`set_edge_gain`](Self::set_edge_gain) it gives an edge with no entry
    /// one (unity, the new range), which is a recompile at the next commit.
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`EdgeGainError::NoSuchEdge`] when `id` is
    /// not an edge of the spec, and [`EdgeGainError::BusLane`] for a lane of a
    /// bus edge.
    pub fn set_edge_gain_range(
        &mut self,
        id: EdgeId,
        range: ParamRange,
    ) -> Result<(), EdgeGainError> {
        self.check_gain_edge(id)?;
        self.touch();
        self.declare_lane(id);
        self.values.set_gain_range(id, range);
        if let Some(cell) = self.cells.gains.get(id) {
            cell.store_range(range);
        }
        Ok(())
    }

    /// A read-only handle on edge `id`'s live range cell, the very cell its
    /// driven lane clamps to and
    /// [`set_edge_gain_range`](Self::set_edge_gain_range) publishes to: the
    /// edge's counterpart of [`param_range_cell`](Self::param_range_cell), for
    /// a control lane. `None` while the edge has no entry (and again once
    /// the entry is dropped: the view then reads a range nothing writes).
    pub fn edge_gain_range_cell(&self, id: EdgeId) -> Option<ParamRangeCell> {
        self.gain_cell(id).map(GainCell::range_cell)
    }

    /// Edge `id`'s cell, while its entry is in the spec.
    fn gain_cell(&self, id: EdgeId) -> Option<&GainCell> {
        self.cells
            .gains
            .get(id)
            .filter(|_| self.spec.gains.contains_key(&id))
    }

    /// Edge `id`'s live base: the very cell [`set_edge_gain`](Self::set_edge_gain)
    /// stores into, which a control lane reads as its knob (a native param's
    /// knob is the node's own cell; an edge's is its gain). `None` while the
    /// edge has no entry. Writing it moves the
    /// base without touching the editor's value: [`edge_gain`](Self::edge_gain)
    /// reads the cell, never the value, once there is one.
    pub fn edge_gain_cell(&self, id: EdgeId) -> Option<ParamCell> {
        self.gain_cell(id).map(GainCell::knob)
    }

    /// Whether `key` has a pass: where its input goes when it does not run (a
    /// bypass) or is blended with its output (a mix short of [`Mix::WET`]).
    /// Its main buses fold onto each other, or agree in width and name no
    /// placement; anything else would be a guess, and every door that would
    /// hear one refuses it ([`set_mode`](Self::set_mode),
    /// [`set_mix`](Self::set_mix), a [`commit`](Self::commit) of a spec that
    /// bypasses, mixes or drives the mix of such a node). A node not inserted
    /// yet has no shape to check, and passes until it has.
    ///
    /// # Errors
    ///
    /// The [`PassError`](crate::PassError) the node would need to resolve.
    pub fn check_pass(&self, key: NodeKey) -> Result<(), crate::PassError> {
        self.shapes
            .get(&key)
            .map_or(Ok(()), |shape| Pass::<f32>::of(shape).map(drop))
    }

    /// Whether `key`'s shape declares its own bypass
    /// ([`Shape::own_bypass`](crate::Shape::own_bypass)).
    #[must_use]
    pub fn bypasses_itself(&self, key: NodeKey) -> bool {
        self.shapes.get(&key).is_some_and(|shape| shape.own_bypass)
    }

    /// Refuses a spec that would hear a node's pass where it has none: a
    /// bypass, a mix short of [`Mix::WET`], or a lane on the mix.
    pub(super) fn check_passes(&self) -> Result<(), CommitError> {
        let heard = |key: NodeKey| {
            (self.values.mode(key) == NodeMode::Bypassed && !self.bypasses_itself(key))
                || self.values.mix(key) != Mix::WET
                || self.spec.params.contains_key(&ParamIn {
                    node: key,
                    param: NodeMix::ADDR,
                })
        };
        for &key in self.spec.topology.nodes.keys() {
            if heard(key)
                && let Err(error) = self.check_pass(key)
            {
                return Err(CommitError::Unplaced { node: key, error });
            }
        }
        Ok(())
    }

    /// Sets `key`'s [`NodeMode`] **without a commit**: the editor records it
    /// and the running unit reads it once per block, from the next block on
    /// (a store while the audio thread is inside a block lands on that block
    /// or the next, and every op of a block sees one mode). Nothing is
    /// compiled or sent, so the plan is the same `Arc` and a host may call
    /// this for every click.
    ///
    /// A change of mode is a crossfade of [`DECLICK`](crate::DECLICK),
    /// counted from the first frame of the block it lands on; see
    /// [`NodeMode`] and `src/mode.rs` for what each mode does. A fork starts
    /// every unit steady in the mode the editor holds, with no fade.
    ///
    /// Before the unit is committed the editor holds the mode, and the unit
    /// starts in it.
    ///
    /// # Errors
    ///
    /// [`ModeError::NoSuchNode`] when the spec has no node at `key`;
    /// [`ModeError::Unplaced`] to bypass a node with no pass (its buses do not
    /// say where its input goes, [`check_pass`](Self::check_pass)); nothing
    /// changes.
    pub fn set_mode(&mut self, key: NodeKey, mode: NodeMode) -> Result<(), ModeError> {
        if !self.spec.topology.nodes.contains_key(&key) {
            return Err(ModeError::NoSuchNode { node: key });
        }
        if mode == NodeMode::Bypassed
            && !self.bypasses_itself(key)
            && let Err(error) = self.check_pass(key)
        {
            return Err(ModeError::Unplaced { node: key, error });
        }
        self.touch();
        self.values.set_mode(key, mode);
        if let Some(cell) = self.cells.modes.get(key) {
            cell.store(mode);
        }
        Ok(())
    }

    /// `key`'s mode: [`NodeMode::Active`] unless [`set_mode`](Self::set_mode)
    /// set another.
    pub fn mode(&self, key: NodeKey) -> NodeMode {
        self.values.mode(key)
    }

    /// Sets `key`'s [`Mix`] **without a commit**: the blend of the node's output
    /// with its aligned input, `out = y * m + x̂ * (1 - m)` per channel. The editor records it and the
    /// running unit reads it once per block, from the next block on; the plan
    /// is the same `Arc` and a host may call this for every move of a knob.
    ///
    /// A move ramps across the block it lands on, as an edge's gain does (see
    /// `src/mode.rs`). [`Mix::WET`] (all of the node's output) is the plain,
    /// unblended node; [`Mix::DRY`] (all of the aligned input) is what Bypassed
    /// writes in steady state, but the node keeps running. A node that is
    /// [`Bypassed`](NodeMode::Bypassed) or [`Muted`](NodeMode::Muted) ignores
    /// the mix: the mode wins. A fork starts every unit at the mix the editor
    /// holds.
    ///
    /// # Errors
    ///
    /// [`MixError::NoSuchNode`] when the spec has no node at `key`,
    /// [`MixError::OutOfRange`] for a mix that is NaN or outside `0..=1`
    /// (make one with [`Mix::new_clamped`]), [`MixError::Unplaced`] for a mix
    /// short of [`Mix::WET`] on a node with no pass
    /// ([`check_pass`](Self::check_pass)); nothing changes.
    pub fn set_mix(&mut self, key: NodeKey, mix: Mix) -> Result<(), MixError> {
        if !self.spec.topology.nodes.contains_key(&key) {
            return Err(MixError::NoSuchNode { node: key });
        }
        if !NodeMix::is_sound(mix) {
            return Err(MixError::OutOfRange);
        }
        if mix != Mix::WET
            && let Err(error) = self.check_pass(key)
        {
            return Err(MixError::Unplaced { node: key, error });
        }
        self.touch();
        self.values.set_mix(key, mix);
        if let Some(cell) = self.cells.modes.get(key) {
            cell.store_mix(mix);
        }
        Ok(())
    }

    /// `key`'s mix: [`Mix::WET`] unless [`set_mix`](Self::set_mix) set another.
    /// The live cell's (the one writer's, whether it is `set_mix` or a lane's
    /// knob); it is the base a driving lane modulates around, not the lane's
    /// value.
    pub fn mix(&self, key: NodeKey) -> Mix {
        self.cells
            .modes
            .get(key)
            .filter(|_| self.spec.topology.nodes.contains_key(&key))
            .map_or_else(|| self.values.mix(key), ModeCell::load_mix)
    }

    /// `key`'s live mix: the very cell [`set_mix`](Self::set_mix) stores into,
    /// which a control lane reads as its knob (a native param's knob is the
    /// node's own cell; a node's mix is this), so `set_mix` and a route are
    /// one writer. `None` for a key not staged. Writing it moves
    /// the mix without touching the editor's value: [`mix`](Self::mix) reads
    /// the cell.
    pub fn mix_cell(&self, key: NodeKey) -> Option<ParamCell> {
        self.cells.modes.get(key).map(ModeCell::knob)
    }

    /// Edge `id`'s base, if the edge has an entry: the live cell's (the one
    /// writer's, whether it is [`set_edge_gain`](Self::set_edge_gain) or a
    /// lane's knob).
    pub fn edge_gain(&self, id: EdgeId) -> Option<Gain> {
        self.spec.gains.get(&id)?;
        Some(
            self.cells
                .gains
                .get(id)
                .map_or_else(|| self.values.base(id), GainCell::load),
        )
    }

    /// Edge `id`'s declick fade ([`EdgeFade`]), if the edge has an entry: what
    /// [`set_edge_fade`](Self::set_edge_fade) set, [`EdgeFade::None`] before.
    pub fn edge_fade(&self, id: EdgeId) -> Option<EdgeFade> {
        self.spec.gains.get(&id)?;
        Some(self.values.fade(id))
    }

    /// Edge `id`'s range, if the edge has an entry: what
    /// [`set_edge_gain_range`](Self::set_edge_gain_range) set, the default
    /// before.
    pub fn edge_gain_range(&self, id: EdgeId) -> Option<ParamRange> {
        self.spec.gains.get(&id)?;
        Some(self.values.gain_range(id))
    }

    /// Write every live cell from the values and the spec's fades: every
    /// range, mode and fade.
    pub(super) fn sync_ranges(&self) {
        self.cells.sync(&self.spec, &self.values);
    }
}
