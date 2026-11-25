//! What the editor holds for each key besides its spec and shape: the unit
//! waiting for the next commit, its wake, its fork source and health probe,
//! the crossfade or latency cut that commit carries, whether the key is
//! leaving, and the cells it shares with its running unit.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::cells::mix_at;
use crate::NodeKey;
use crate::compile::plan::Plan;
use crate::model::fade::Fade;
use crate::model::mode::{ModeCell, ModeState, NodeMode};
use crate::model::values::GraphValues;
use crate::node::Node;
use crate::node::fork::{ForkHealth, ForkSource};
use crate::node::param::{
    EffectiveCell, ParamIn, ParamPorts, ParamRange, ParamRangeCell, ParamRanges, ParamState,
};
use crate::node::wake::Wake;
use tutti_buffers::Float;

/// One [`Entry`] per key that was ever placed.
pub(super) struct Entries<F: Float>(BTreeMap<NodeKey, Entry<F>>);

/// One key's record, made when a unit is first placed (or a fade, a latency
/// cut or a leave first names the key) and never removed.
///
/// **The record outlives its node**: the generation a key's next unit gets
/// keeps counting across a removal, so a unit placed after it is never taken
/// for one before. [`forget`](Entries::forget) clears everything else at
/// once, and the key's leaving only when it ends.
struct Entry<F: Float> {
    /// The generation the next unit placed here gets. The unit placed last
    /// has `next_gen - 1`.
    next_gen: u32,
    /// The unit placed since the last commit, which the next one sends.
    unit: Option<Box<dyn Node<F>>>,
    /// The [`Wake`] the latest placed unit was prepared with. A commit hands
    /// the executor a clone with the unit, and a re-prepare hands the unit
    /// this same one again.
    wake: Option<Wake>,
    /// Where the key's forks come from, for the unit placed last. `None`:
    /// the key is not forkable.
    fork: Option<Box<dyn ForkSource<F>>>,
    /// On a fork: the health probe the placed unit's source handed over.
    fork_health: Option<Arc<dyn ForkHealth>>,
    /// The crossfade a `replace` asked for, carried by the next commit.
    fade: Option<Fade>,
    /// Whether `set_latency` changed the key since the last commit: a
    /// crossfade running or waiting there is cut by it.
    latency_cut: bool,
    /// Whether `remove_declicked` keeps the key until its output fades.
    leaving: bool,
    /// The mode, mix and mix range cells shared with the unit running at the
    /// key. `Some` from the key's staging until a commit places a plan
    /// without it, or it is removed.
    mode: Option<ModeCell>,
    /// The param range and lane cells shared with the running unit; `None`
    /// for a unit that declares no params.
    ranges: Option<Arc<ParamRanges>>,
}

impl<F: Float> Default for Entry<F> {
    fn default() -> Self {
        Self {
            next_gen: 0,
            unit: None,
            wake: None,
            fork: None,
            fork_health: None,
            fade: None,
            latency_cut: false,
            leaving: false,
            mode: None,
            ranges: None,
        }
    }
}

impl<F: Float> Entries<F> {
    pub(super) fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Places `unit` at `key` with its wake and fork source, and returns the
    /// unit's generation. A fade or latency cut waiting at `key` was for the
    /// unit this one supersedes, and goes; a unit with no fork source makes
    /// the key unforkable.
    pub(super) fn place(
        &mut self,
        key: NodeKey,
        unit: Box<dyn Node<F>>,
        wake: Wake,
        fork: Option<Box<dyn ForkSource<F>>>,
    ) -> u32 {
        let e = self.0.entry(key).or_default();
        let generation = e.next_gen;
        e.next_gen += 1;
        e.unit = Some(unit);
        e.wake = Some(wake);
        e.fork = fork;
        e.fork_health = None;
        e.fade = None;
        e.latency_cut = false;
        generation
    }

    /// The crossfade the next commit carries for the unit placed at `key`.
    pub(super) fn set_fade(&mut self, key: NodeKey, fade: Fade) {
        self.0.entry(key).or_default().fade = Some(fade);
    }

    /// Marks `key`'s latency changed: the next commit cuts a crossfade
    /// running or waiting there, and a replace not yet committed lands as a
    /// swap.
    pub(super) fn cut_latency(&mut self, key: NodeKey) {
        let e = self.0.entry(key).or_default();
        e.fade = None;
        e.latency_cut = true;
    }

    /// Forgets everything `key` holds but its generation count, and its
    /// leaving while it lasts.
    pub(super) fn forget(&mut self, key: NodeKey) {
        if let Some(e) = self.0.get_mut(&key) {
            *e = Entry {
                next_gen: e.next_gen,
                leaving: e.leaving,
                ..Entry::default()
            };
        }
    }

    /// Keeps `key` until its output fades.
    pub(super) fn leave(&mut self, key: NodeKey) {
        self.0.entry(key).or_default().leaving = true;
    }

    /// Ends `key`'s leaving.
    pub(super) fn stop_leaving(&mut self, key: NodeKey) {
        if let Some(e) = self.0.get_mut(&key) {
            e.leaving = false;
        }
    }

    /// Whether `key` is leaving.
    pub(super) fn is_leaving(&self, key: NodeKey) -> bool {
        self.0.get(&key).is_some_and(|e| e.leaving)
    }

    /// The keys leaving.
    pub(super) fn leaving(&self) -> Vec<NodeKey> {
        self.0
            .iter()
            .filter(|(_, e)| e.leaving)
            .map(|(&k, _)| k)
            .collect()
    }

    /// The wake of the unit placed at `key` last.
    pub(super) fn wake(&self, key: NodeKey) -> Option<&Wake> {
        self.0.get(&key).and_then(|e| e.wake.as_ref())
    }

    /// Where `key`'s forks come from, and the generation of the unit that
    /// handed it over, if it is forkable.
    pub(super) fn fork_source(&self, key: NodeKey) -> Option<(u32, &dyn ForkSource<F>)> {
        let e = self.0.get(&key)?;
        Some((e.next_gen.checked_sub(1)?, e.fork.as_deref()?))
    }

    /// Watches `health` for the forked unit placed at `key`.
    pub(super) fn watch_fork(&mut self, key: NodeKey, health: Arc<dyn ForkHealth>) {
        self.0.entry(key).or_default().fork_health = Some(health);
    }

    /// The forked units' probes, in key order.
    pub(super) fn fork_probes(&self) -> impl Iterator<Item = (NodeKey, &dyn ForkHealth)> {
        self.0
            .iter()
            .filter_map(|(&k, e)| Some((k, e.fork_health.as_deref()?)))
    }

    /// Whether a unit placed at `key` waits for a commit.
    pub(super) fn has_unit(&self, key: NodeKey) -> bool {
        self.0.get(&key).is_some_and(|e| e.unit.is_some())
    }

    /// Takes the unit waiting at `key`.
    pub(super) fn take_unit(&mut self, key: NodeKey) -> Option<Box<dyn Node<F>>> {
        self.0.get_mut(&key).and_then(|e| e.unit.take())
    }

    /// Takes every unit waiting for a commit.
    pub(super) fn take_units(&mut self) -> BTreeMap<NodeKey, Box<dyn Node<F>>> {
        self.0
            .iter_mut()
            .filter_map(|(&k, e)| e.unit.take().map(|u| (k, u)))
            .collect()
    }

    /// The crossfade waiting at `key`.
    pub(super) fn fade(&self, key: NodeKey) -> Option<Fade> {
        self.0.get(&key).and_then(|e| e.fade)
    }

    /// The keys whose latency changed since the last commit.
    pub(super) fn latency_cuts(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.0
            .iter()
            .filter(|(_, e)| e.latency_cut)
            .map(|(&k, _)| k)
    }

    /// Clears every waiting crossfade and latency cut: a commit carried
    /// them, or a re-prepare made them moot.
    pub(super) fn clear_fades_and_cuts(&mut self) {
        for e in self.0.values_mut() {
            e.fade = None;
            e.latency_cut = false;
        }
    }

    /// Whether anything waits for the next commit: a unit, a crossfade or a
    /// latency cut.
    pub(super) fn is_staged(&self) -> bool {
        self.0
            .values()
            .any(|e| e.unit.is_some() || e.fade.is_some() || e.latency_cut)
    }

    /// `key`'s mode cell, once the node is staged.
    pub(super) fn mode(&self, key: NodeKey) -> Option<&ModeCell> {
        self.0.get(&key).and_then(|e| e.mode.as_ref())
    }

    /// Stores `range` into `at`'s cell, if a unit declaring it is staged.
    pub(super) fn store_range(&self, at: ParamIn, range: ParamRange) {
        if let Some(cells) = self.0.get(&at.node).and_then(|e| e.ranges.as_ref()) {
            cells.store(at.param, range);
        }
    }

    /// A read-only handle on `at`'s range cell.
    pub(super) fn range_cell(&self, at: ParamIn) -> Option<ParamRangeCell> {
        self.ranges(at.node)?.cell(at.param)
    }

    /// A handle on the value `at`'s lane last delivered.
    pub(super) fn effective(&self, at: ParamIn) -> Option<EffectiveCell> {
        self.ranges(at.node)?.lane(at.param)
    }

    fn ranges(&self, key: NodeKey) -> Option<&Arc<ParamRanges>> {
        self.0.get(&key)?.ranges.as_ref()
    }

    /// Frees the ranges the audio thread has let go of; how many are still
    /// held by a block. Control side.
    pub(super) fn reclaim(&self) -> usize {
        self.0
            .values()
            .map(|e| {
                e.mode.as_ref().map_or(0, ModeCell::reclaim)
                    + e.ranges.as_ref().map_or(0, |r| r.reclaim())
            })
            .sum()
    }

    /// Writes every mode and range cell from `values`. A base and a mix are
    /// never written here: each cell is its one writer's (a knob a route
    /// moves too).
    pub(super) fn sync(&self, values: &GraphValues) {
        for (&node, e) in &self.0 {
            if let Some(cell) = &e.mode {
                cell.store(values.mode(node));
                cell.store_range(values.range(mix_at(node)));
            }
            if let Some(cells) = &e.ranges {
                for &param in cells.declared().as_slice() {
                    cells.store(param, values.range(ParamIn { node, param }));
                }
            }
        }
    }

    /// Makes `key`'s cells for a unit declaring `declared` param ports, from
    /// `values`: the mode cell is kept for a key that stays, the range cells
    /// for the same list (a crossfade keeps the running state, which reads
    /// them) and fresh for another. A list of no params has none.
    pub(super) fn stage_cells(&mut self, key: NodeKey, declared: ParamPorts, values: &GraphValues) {
        let e = self.0.entry(key).or_default();
        let mode = e
            .mode
            .get_or_insert_with(|| ModeCell::new(NodeMode::Active, values.mix(key)));
        mode.store(values.mode(key));
        mode.store_range(values.range(mix_at(key)));
        if declared.is_empty() {
            e.ranges = None;
            return;
        }
        let cells = match &mut e.ranges {
            Some(c) if c.declared() == declared => c,
            slot => slot.insert(ParamRanges::new(declared)),
        };
        for &param in declared.as_slice() {
            cells.store(param, values.range(ParamIn { node: key, param }));
        }
    }

    /// The param state of the unit placed at `key` with `ports` param ports,
    /// over the cells [`stage_cells`](Self::stage_cells) made.
    pub(super) fn param_state(&self, key: NodeKey, ports: usize, max: usize) -> ParamState<F> {
        match self.ranges(key) {
            Some(cells) if ports > 0 => ParamState::new(ports, max, cells.readers(), cells.lanes()),
            _ => ParamState::default(),
        }
    }

    /// The mode state of the unit `plan` places at `key`: the key's cell,
    /// made by [`stage_cells`](Self::stage_cells).
    pub(super) fn mode_state(&self, key: NodeKey, plan: &Plan) -> ModeState<F> {
        let cell = self.mode(key).expect("a placed unit has its cells");
        let shape = &plan.unit(key).expect("a placed unit is in its plan").shape;
        ModeState::new(cell.clone(), shape, plan.prepare.max_block().get())
    }

    /// Writes each kept unit's mix range from `values`, and drops the cells
    /// of every key `plan` does not place.
    pub(super) fn keep_placed(&mut self, plan: &Plan, values: &GraphValues) {
        for (&key, e) in &mut self.0 {
            if plan.unit(key).is_some() {
                if let Some(cell) = &e.mode {
                    cell.store_range(values.range(mix_at(key)));
                }
            } else {
                e.mode = None;
                e.ranges = None;
            }
        }
    }
}

impl<F: Float> std::fmt::Debug for Entries<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(k, e)| {
                (
                    k,
                    (
                        e.next_gen,
                        e.unit.is_some(),
                        e.fade,
                        e.latency_cut,
                        e.leaving,
                        e.ranges.is_some(),
                    ),
                )
            }))
            .finish()
    }
}
