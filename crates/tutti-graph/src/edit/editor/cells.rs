//! The control halves of the cells the editor shares with running units: each
//! param port's range, each weighted lane's gain, each unit's mode and mix.
//!
//! A node's cells are made when the node is staged (`insert`, `replace_or_swap`) and an
//! edge's when its gain entry is declared, so a lane can bind to them before
//! any commit. A control (`set_param_range`, `set_edge_gain`, `set_mode` ...)
//! stores into its value and the cell at once and the running unit reads it
//! next block, with no commit; a commit writes every cell from the values (a
//! fade from the spec), hands the staged cells to the units it places, and
//! drops the cells of what the new plan no longer has.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::BusLanes;
use crate::compile::plan::Plan;
use crate::model::gain::{EdgeFade, EdgeGain, GainCell};
use crate::model::mode::{ModeCell, ModeState, NodeMix, NodeMode};
use crate::model::spec::GraphSpec;
use crate::model::values::GraphValues;
use crate::node::param::{
    EffectiveCell, ParamIn, ParamPorts, ParamRange, ParamRangeCell, ParamRanges, ParamState,
};
use crate::{EdgeId, NodeKey};
use tutti_buffers::Float;

/// Every live cell, by kind. The kinds are separate fields so a commit can
/// hand out cells of two kinds at once.
pub(super) struct LiveCells {
    pub(super) ranges: RangeCells,
    pub(super) gains: GainCells,
    pub(super) modes: ModeCells,
}

impl LiveCells {
    pub(super) fn new() -> Self {
        Self {
            ranges: RangeCells(BTreeMap::new()),
            gains: GainCells(BTreeMap::new()),
            modes: ModeCells(BTreeMap::new()),
        }
    }

    /// Writes every cell from `values` (a gain lane's fade from `spec`). A
    /// base and a mix are never written here: each cell is its one writer's
    /// (a knob a route moves too).
    pub(super) fn sync(&self, spec: &GraphSpec, values: &GraphValues) {
        self.modes.sync(values);
        self.gains.sync(spec, values);
        self.ranges.sync(values);
    }

    /// Makes the cells of the unit staged at `key`, which declares `declared`
    /// param ports, from `values`: the mode cell is kept for a key that
    /// stays, the range cells for the same list and fresh for another.
    pub(super) fn stage(&mut self, key: NodeKey, declared: ParamPorts, values: &GraphValues) {
        self.modes.declare(key, values);
        self.ranges.declare(key, declared, values);
    }

    /// Drops `key`'s cells: a node inserted at the key later starts from its
    /// own values.
    pub(super) fn forget(&mut self, key: NodeKey) {
        self.modes.0.remove(&key);
        self.ranges.0.remove(&key);
    }

    /// Frees the ranges the audio thread has let go of, in every cell kind;
    /// how many are still held by a block. Control side.
    pub(super) fn reclaim(&self) -> usize {
        self.ranges.reclaim() + self.gains.reclaim() + self.modes.reclaim()
    }

    /// Drops the cells of every key `plan` does not place, and writes each
    /// kept unit's mix range from `values`.
    pub(super) fn keep_placed(&mut self, plan: &Plan, values: &GraphValues) {
        for (&key, cell) in &self.modes.0 {
            cell.store_range(values.range(mix_at(key)));
        }
        self.ranges.0.retain(|k, _| plan.unit(*k).is_some());
        self.modes.0.retain(|k, _| plan.unit(*k).is_some());
    }
}

/// Each key's param range cells, shared with its running unit's state.
pub(super) struct RangeCells(BTreeMap<NodeKey, Arc<ParamRanges>>);

impl RangeCells {
    /// Stores `range` into `at`'s cell, if a unit declaring it is placed.
    pub(super) fn store(&self, at: ParamIn, range: ParamRange) {
        if let Some(cells) = self.0.get(&at.node) {
            cells.store(at.param, range);
        }
    }

    /// A read-only handle on `at`'s cell.
    pub(super) fn cell(&self, at: ParamIn) -> Option<ParamRangeCell> {
        self.0.get(&at.node).and_then(|cells| cells.cell(at.param))
    }

    /// A handle on the value `at`'s lane last delivered.
    pub(super) fn effective(&self, at: ParamIn) -> Option<EffectiveCell> {
        self.0.get(&at.node).and_then(|cells| cells.lane(at.param))
    }

    fn reclaim(&self) -> usize {
        self.0.values().map(|cells| cells.reclaim()).sum()
    }

    /// The keys with cells.
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    fn sync(&self, values: &GraphValues) {
        for (&node, cells) in &self.0 {
            for &param in cells.declared().as_slice() {
                cells.store(param, values.range(ParamIn { node, param }));
            }
        }
    }

    /// The cells of a unit at `key` declaring `declared`: the key's, kept for
    /// the same list (a crossfade keeps the running state, which reads them)
    /// and fresh for another, written from `values`. A list of no params has
    /// none.
    fn declare(&mut self, key: NodeKey, declared: ParamPorts, values: &GraphValues) {
        if declared.is_empty() {
            self.0.remove(&key);
            return;
        }
        let cells = self
            .0
            .entry(key)
            .and_modify(|c| {
                if c.declared() != declared {
                    *c = ParamRanges::new(declared);
                }
            })
            .or_insert_with(|| ParamRanges::new(declared));
        for &param in declared.as_slice() {
            cells.store(param, values.range(ParamIn { node: key, param }));
        }
    }

    /// The param state of a unit placed at `key` with `ports` param ports
    /// declaring `declared`, over the key's cells written from `values`.
    pub(super) fn state<F: Float>(
        &mut self,
        key: NodeKey,
        ports: usize,
        max: usize,
        declared: Option<ParamPorts>,
        values: &GraphValues,
    ) -> ParamState<F> {
        self.declare(key, declared.unwrap_or(ParamPorts::NONE), values);
        match self.0.get(&key) {
            Some(cells) if ports > 0 => ParamState::new(ports, max, cells.readers(), cells.lanes()),
            _ => ParamState::default(),
        }
    }
}

/// Each weighted lane's gain cell, shared with the running executor's lane
/// state.
pub(super) struct GainCells(BTreeMap<EdgeId, GainCell>);

impl GainCells {
    /// Lane `id`'s cell, once its entry is declared.
    pub(super) fn get(&self, id: EdgeId) -> Option<&GainCell> {
        self.0.get(&id)
    }

    fn reclaim(&self) -> usize {
        self.0.values().map(GainCell::reclaim).sum()
    }

    /// Whether edge `id` has a cell.
    pub(super) fn has(&self, id: EdgeId) -> bool {
        self.0.contains_key(&id)
    }

    /// Declares edge `id` from `values`: a lane declared again starts from
    /// unity, the default range and no fade. An edge keeps the one cell the
    /// executor may already read (a lane dropped and declared again before
    /// the next commit sends no new plan), so the values are written into
    /// it; a new edge gets a new cell.
    pub(super) fn declare(&mut self, id: EdgeId, values: &GraphValues) {
        if let Some(cell) = self.0.get(&id) {
            cell.store(values.base(id));
            cell.store_range(values.gain_range(id));
            cell.store_fade(values.fade(id));
            return;
        }
        let cell = GainCell::new(values.base(id), values.gain_range(id));
        cell.store_fade(values.fade(id));
        self.0.insert(id, cell);
    }

    fn sync(&self, spec: &GraphSpec, values: &GraphValues) {
        for (id, cell) in &self.0 {
            if spec.gains.contains_key(id) {
                cell.store_range(values.gain_range(*id));
                cell.store_fade(values.fade(*id));
            }
        }
    }

    /// One cell per weighted lane of `plan`, kept across commits for an edge
    /// that stays (its base is its one writer's) and made from `values` for
    /// one that is new. A lane a bus edge made gets the fold's static weight
    /// in a cell of its own that nothing writes and no one is handed.
    pub(super) fn for_plan(
        &mut self,
        plan: &Plan,
        spec: &GraphSpec,
        values: &GraphValues,
        bus_weights: &BusLanes,
    ) -> BTreeMap<EdgeId, GainCell> {
        let lanes: BTreeSet<EdgeId> = plan.program.sum_lanes.iter().flatten().copied().collect();
        self.0.retain(|id, _| lanes.contains(id));
        lanes
            .into_iter()
            .map(|id| {
                if let Some(&(w, fade)) = bus_weights.get(&id) {
                    let cell = GainCell::new(w, EdgeGain::DEFAULT_RANGE);
                    cell.store_fade(fade);
                    return (id, cell);
                }
                let range = values.gain_range(id);
                // An entry has its cell from its declaration; one a fork
                // or a builder wrote whole has none yet, and a lane without an
                // entry is derived by this commit (the main output
                // crossfade's): the cell is made here from the values.
                let fade = if spec.gains.contains_key(&id) {
                    values.fade(id)
                } else {
                    EdgeFade::None
                };
                let cell = self
                    .0
                    .entry(id)
                    .or_insert_with(|| GainCell::new(values.base(id), range));
                cell.store_range(range);
                cell.store_fade(fade);
                (id, cell.clone())
            })
            .collect()
    }
}

/// Each placed unit's mode cell (its mode, mix and mix range), shared with the
/// running unit.
pub(super) struct ModeCells(BTreeMap<NodeKey, ModeCell>);

impl ModeCells {
    /// `key`'s cell, once the node is staged.
    pub(super) fn get(&self, key: NodeKey) -> Option<&ModeCell> {
        self.0.get(&key)
    }

    fn reclaim(&self) -> usize {
        self.0.values().map(ModeCell::reclaim).sum()
    }

    /// `key`'s cell, made from `values` when the key has none; a key that
    /// stays keeps its cell.
    fn declare(&mut self, key: NodeKey, values: &GraphValues) {
        let cell = self
            .0
            .entry(key)
            .or_insert_with(|| ModeCell::new(NodeMode::Active, values.mix(key)));
        cell.store(values.mode(key));
        cell.store_range(values.range(mix_at(key)));
    }

    fn sync(&self, values: &GraphValues) {
        for (&node, cell) in &self.0 {
            cell.store(values.mode(node));
            cell.store_range(values.range(mix_at(node)));
        }
    }

    /// The mode state of the unit `plan` places at `key`: the key's cell,
    /// kept for a key that stays; a unit placed now starts steady in the
    /// node values' mode and mix.
    pub(super) fn state<F: Float>(
        &mut self,
        key: NodeKey,
        plan: &Plan,
        values: &GraphValues,
    ) -> ModeState<F> {
        let cell = self
            .0
            .entry(key)
            .or_insert_with(|| ModeCell::new(NodeMode::Active, values.mix(key)));
        cell.store(values.mode(key));
        cell.store_range(values.range(mix_at(key)));
        let shape = &plan.unit(key).expect("a placed unit is in its plan").shape;
        ModeState::new(cell.clone(), shape, plan.prepare.max_block().get())
    }
}

/// The port a node's mix is driven at, and whose range it clamps to.
fn mix_at(node: NodeKey) -> ParamIn {
    ParamIn {
        node,
        param: NodeMix::ADDR,
    }
}
