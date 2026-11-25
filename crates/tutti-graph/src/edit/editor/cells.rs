//! The control half of each weighted lane's gain cell, shared with the running
//! executor.
//!
//! An edge's cell is made when its gain entry is declared, so a lane can bind
//! to it before any commit. A control (`set_edge_gain`, `set_edge_gain_range`
//! ...) stores into its value and the cell at once and the running executor
//! reads it next block, with no commit; a commit writes every cell from the
//! values (a fade from the spec) and drops the cells of the lanes the new plan
//! no longer has. A unit's own cells are its [`Entry`](super::entries)'s.

use std::collections::{BTreeMap, BTreeSet};

use super::BusLanes;
use crate::compile::plan::Plan;
use crate::model::gain::{EdgeFade, EdgeGain, GainCell};
use crate::model::mode::NodeMix;
use crate::model::spec::GraphSpec;
use crate::model::values::GraphValues;
use crate::node::param::ParamIn;
use crate::{EdgeId, NodeKey};

/// Each weighted lane's gain cell, shared with the running executor's lane
/// state.
#[derive(Default)]
pub(super) struct GainCells(BTreeMap<EdgeId, GainCell>);

impl GainCells {
    /// Lane `id`'s cell, once its entry is declared.
    pub(super) fn get(&self, id: EdgeId) -> Option<&GainCell> {
        self.0.get(&id)
    }

    pub(super) fn reclaim(&self) -> usize {
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

    /// Writes every lane's range and fade from `values`, for each lane `spec`
    /// has an entry for.
    pub(super) fn sync(&self, spec: &GraphSpec, values: &GraphValues) {
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

/// The port a node's mix is driven at, and whose range it clamps to.
pub(super) fn mix_at(node: NodeKey) -> ParamIn {
    ParamIn {
        node,
        param: NodeMix::ADDR,
    }
}
