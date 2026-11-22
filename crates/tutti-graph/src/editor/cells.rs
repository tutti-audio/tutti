//! The control halves of the cells the editor shares with running units: each
//! param port's range, each weighted lane's gain, each unit's mode and mix.
//!
//! A control (`set_param_range`, `set_edge_gain`, `set_mode` ...) stores into
//! its value and the cell at once and the running unit reads it next block,
//! with no commit; a commit writes every cell from the values (a fade from the
//! spec), keeps the cells of what the new plan keeps, and makes cells for what
//! it places.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tutti_curve::declick_frames;

use super::BusLanes;
use crate::gain::{EdgeFade, EdgeGain, GainCell};
use crate::mode::{ModeCell, ModeState, NodeMix, NodeMode};
use crate::param::{ParamIn, ParamPorts, ParamRange, ParamRangeCell, ParamRanges, ParamState};
use crate::plan::Plan;
use crate::spec::GraphSpec;
use crate::values::GraphValues;
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

    /// Drops `key`'s mode cell: a node inserted at the key later starts from
    /// its own values.
    pub(super) fn forget(&mut self, key: NodeKey) {
        self.modes.0.remove(&key);
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

    /// The param state of a unit placed at `key` with `ports` param ports
    /// declaring `declared`: the key's cells, kept for the same list (a
    /// crossfade keeps the running state, which reads them) and fresh for
    /// another, written from `values`.
    pub(super) fn state<F: Float>(
        &mut self,
        key: NodeKey,
        ports: usize,
        max: usize,
        declick: usize,
        declared: Option<ParamPorts>,
        values: &GraphValues,
    ) -> ParamState<F> {
        if ports == 0 {
            self.0.remove(&key);
            return ParamState::default();
        }
        let list = declared.unwrap_or(ParamPorts::NONE);
        let cells = self
            .0
            .entry(key)
            .and_modify(|c| {
                if c.declared() != list {
                    *c = ParamRanges::new(list);
                }
            })
            .or_insert_with(|| ParamRanges::new(list));
        for &param in list.as_slice() {
            cells.store(param, values.range(ParamIn { node: key, param }));
        }
        ParamState::new(ports, max, declick, cells.readers())
    }
}

/// Each weighted lane's gain cell, shared with the running executor's lane
/// state.
pub(super) struct GainCells(BTreeMap<EdgeId, GainCell>);

impl GainCells {
    /// Lane `id`'s cell, once a plan with its entry was sent.
    pub(super) fn get(&self, id: EdgeId) -> Option<&GainCell> {
        self.0.get(&id)
    }

    fn reclaim(&self) -> usize {
        self.0.values().map(GainCell::reclaim).sum()
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
        let lanes: BTreeSet<EdgeId> = plan.sum_lanes.iter().flatten().copied().collect();
        self.0.retain(|id, _| lanes.contains(id));
        lanes
            .into_iter()
            .map(|id| {
                if let Some(&(w, fade)) = bus_weights.get(&id) {
                    let cell = GainCell::new(w, EdgeGain::DEFAULT_RANGE);
                    cell.store_fade(fade);
                    return (id, cell);
                }
                let fade = if spec.gains.contains_key(&id) {
                    values.fade(id)
                } else {
                    EdgeFade::None
                };
                let range = values.gain_range(id);
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
    /// `key`'s cell, once a plan placing it was sent.
    pub(super) fn get(&self, key: NodeKey) -> Option<&ModeCell> {
        self.0.get(&key)
    }

    fn reclaim(&self) -> usize {
        self.0.values().map(ModeCell::reclaim).sum()
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
        ModeState::new(
            cell.clone(),
            shape,
            plan.prepare.max_block().get(),
            declick_frames(plan.prepare.sample_rate()).get(),
        )
    }
}

/// The port a node's mix is driven at, and whose range it clamps to.
fn mix_at(node: NodeKey) -> ParamIn {
    ParamIn {
        node,
        param: NodeMix::ADDR,
    }
}
