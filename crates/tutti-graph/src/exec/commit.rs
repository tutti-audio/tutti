//! What crosses the rings: a commit, a crossfade's outgoing unit, and the
//! rings themselves.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use tutti_buffers::Float;

use tutti_curve::FadeLaw;
use tutti_rt::{CappedVec, ControlDrop, Overflow, PushError, RingReader, RingWriter, ring};

use super::stage::Staged;
use super::state::{Gone, State, Unit};
use super::{EDGE_FADE_CAPACITY, FADE_CAPACITY, QUEUE_CAPACITY};
use crate::NodeKey;
use crate::arena::Arena;
use crate::event::Event;
use crate::event_tap::EventTapWriter;
use crate::gain::GainCell;
use crate::held::{Keys, Tables};
use crate::loss::EventLoss;
use crate::meter::MeterWriter;
use crate::mode::{BypassLine, ModeState};
use crate::node::{Node, Prepare};
use crate::param::ParamState;
use crate::plan::{Delta, Plan, UnitIdx};
use crate::play::PlayReader;
use crate::tap::TapWriter;
use crate::wake::Wake;
use crate::{EdgeFade, EdgeId, EventTapId, MeterId, TapId};

/// A unit, owned. Crate-private, so the only code that can replace or drop
/// the box is this crate's; its drop is checked against the audio-thread
/// marker (a [`ControlDrop`] field). This crate must *run* its
/// units, so the box hands `&mut` out.
///
/// It carries the unit's [`Wake`]: the executor's clone of the flag lives and
/// dies with the unit, so its drop is the control side's too, and the audio
/// thread is never the last owner of the flag's storage.
pub(crate) struct NodeBox<F: Float = f32> {
    _control: ControlDrop<Self>,
    node: Box<dyn Node<F>>,
    wake: Wake,
}

impl<F: Float> NodeBox<F> {
    pub(crate) fn new(node: Box<dyn Node<F>>, wake: Wake) -> Self {
        Self {
            _control: ControlDrop::new(),
            node,
            wake,
        }
    }

    /// Whether the unit's [`Wake`] was raised since the last call, clearing
    /// it.
    #[inline]
    pub(crate) fn take_wake(&self) -> bool {
        self.wake.take()
    }

    /// The unit, out of its box — control side, to re-prepare it.
    pub(crate) fn into_inner(self) -> Box<dyn Node<F>> {
        self.node
    }
}

impl<F: Float> Deref for NodeBox<F> {
    type Target = dyn Node<F>;
    fn deref(&self) -> &(dyn Node<F> + 'static) {
        &*self.node
    }
}

impl<F: Float> DerefMut for NodeBox<F> {
    fn deref_mut(&mut self) -> &mut (dyn Node<F> + 'static) {
        &mut *self.node
    }
}

/// One crossfade's own state (see the `fade` module, `src/fade.rs`). Built on
/// the control side with the commit that asks for it, so its scratch is
/// allocated there; it travels back in a commit box, so it is freed there
/// too.
pub(crate) struct Crossfade<F: Float = f32> {
    _control: ControlDrop<Self>,
    key: NodeKey,
    /// The unit fading out, once the fade has started.
    pub(super) out: Option<NodeBox<F>>,
    /// The unit fading in, while this fade waits behind the one running at
    /// its key. It moves into the store when this fade starts.
    pub(super) waiting: Option<NodeBox<F>>,
    /// The outgoing unit's outputs, one aligned slot per channel.
    pub(super) scratch: Arena<F>,
    /// Frames of the fade rendered so far.
    pub(super) done: usize,
    pub(super) len: usize,
    pub(super) law: FadeLaw,
    /// While this fade waits behind the one running at its key: what the unit
    /// fading in wrote for the unit that waits (`Node::chase`), delivered to
    /// it when this fade starts. Empty otherwise.
    pub(super) chase: CappedVec<Event>,
}

impl<F: Float> Crossfade<F> {
    /// Items its chase list refused when it was applied: leaked, not freed.
    pub(crate) fn refused(&self) -> usize {
        self.chase.refused()
    }

    /// The units it carries back: the outgoing one, a waiting one that
    /// never ran, or both after a re-prepare's cut.
    pub(crate) fn units(&self) -> usize {
        usize::from(self.out.is_some()) + usize::from(self.waiting.is_some())
    }

    /// Its key.
    pub(crate) fn key(&self) -> NodeKey {
        self.key
    }
}

/// A unit a commit places, with everything built for it on the control
/// side.
pub(super) struct Incoming<F: Float = f32> {
    pub(super) key: NodeKey,
    pub(super) idx: UnitIdx,
    pub(super) generation: u32,
    pub(super) node: NodeBox<F>,
    /// The crossfade it arrives with, over the unit at its key.
    pub(super) fade: Option<Box<Crossfade<F>>>,
    pub(super) params: ParamState<F>,
    pub(super) mode: ModeState<F>,
    /// The held-key tables of a unit placed now, sized for its shape.
    pub(super) keys: Keys,
    /// The tables it writes into while it waits behind a running fade
    /// (`Keys::wait`); empty for a unit with no event outputs or no fade.
    pub(super) staging: Tables,
}

/// A kept unit's rebuilt bypass line.
pub(super) struct Relined<F: Float = f32> {
    pub(super) idx: UnitIdx,
    pub(super) latency: usize,
    pub(super) line: Option<BypassLine<F>>,
}

/// A plan and the unit changes that go with it — what travels to the
/// executor and back. Crate-private: nobody outside the pair ever holds one.
///
/// Before it is applied it holds the new plan and the incoming units; after,
/// everything the executor replaced. Dropping it on the audio thread panics
/// in a debug build.
pub(crate) struct Commit<F: Float = f32> {
    _control: ControlDrop<Self>,
    /// The editor's count of commits sent, this one included: a scheduled
    /// command checked against this commit waits until it is applied.
    pub(super) seq: u64,
    /// A re-prepare's first half: check every unit out and adopt this
    /// `Prepare` (see `Editor::reprepare`). Carries no plan.
    pub(super) suspend: Option<Prepare>,
    pub(super) plan: Option<Arc<Plan>>,
    pub(super) delta: Delta,
    pub(super) incoming: Vec<Incoming<F>>,
    /// Kept units whose declared latency changed, each with its rebuilt
    /// bypass line (`Editor::set_latency`).
    pub(super) relined: Vec<Relined<F>>,
    pub(super) retired: CappedVec<(NodeKey, NodeBox<F>)>,
    /// Param state the executor let go of, carried back to be freed on the
    /// control side: a retired unit's, and one built for an incoming unit
    /// that crossfades in over one already at its key, which keeps its own
    /// (the port's modulation runs on across a replace).
    pub(super) spare_params: CappedVec<ParamState<F>>,
    /// Mode state the executor let go of (a retired unit's, an unused
    /// incoming one's), freed on the control side.
    pub(super) spare_modes: CappedVec<ModeState<F>>,
    /// Bypass lines a rebuilt one replaced.
    pub(super) spare_lines: CappedVec<BypassLine<F>>,
    /// Held-key tables the executor let go of, freed on the control side.
    pub(super) spare_held: CappedVec<Tables>,
    pub(super) old_state: Option<State<F>>,
    /// The state the plan needs, built on the control side (`stage.rs`);
    /// after applying, what applying did not use.
    pub(super) staged: Option<Box<Staged<F>>>,
    /// The lists applying fills, with room reserved here.
    pub(super) gone: Gone,
    /// A re-prepare's first half: where the units' held-key tables wait for
    /// the resume (`Executor::parked`), with room for every unit.
    pub(super) parked: CappedVec<(NodeKey, Keys)>,
    /// Tap producers and meter cells the editor opened since the last commit.
    pub(super) opened: Opened<F>,
}

/// What the editor hands the executor with a commit: what it opened since its
/// last one (a tap's producers and a meter's cells), and the cell of every
/// weighted lane the plan lists (`Plan::sum_lanes`).
#[derive(Debug)]
pub(crate) struct Opened<F: Float = f32> {
    /// The live base of each weighted lane of the plan, by edge. The editor
    /// owns the cells and keeps one per edge across commits.
    pub(crate) gains: BTreeMap<EdgeId, GainCell>,
    pub(crate) taps: Vec<(TapId, TapWriter<F>)>,
    pub(crate) event_taps: Vec<(EventTapId, EventTapWriter)>,
    pub(crate) meters: Vec<(MeterId, MeterWriter)>,
}

impl<F: Float> Default for Opened<F> {
    fn default() -> Self {
        Self {
            gains: BTreeMap::new(),
            taps: Vec::new(),
            event_taps: Vec::new(),
            meters: Vec::new(),
        }
    }
}

impl<F: Float> Commit<F> {
    /// `params` builds each incoming unit's param state from its key and
    /// declared port count (the editor's range cells).
    pub(crate) fn build(
        seq: u64,
        plan: Arc<Plan>,
        delta: Delta,
        mut units: BTreeMap<NodeKey, Box<dyn Node<F>>>,
        mut params: impl FnMut(NodeKey, usize, usize) -> ParamState<F>,
        mut modes: impl FnMut(NodeKey) -> ModeState<F>,
        mut wakes: impl FnMut(NodeKey) -> Wake,
        opened: Opened<F>,
    ) -> Box<Commit<F>> {
        let wanted = delta
            .insert
            .iter()
            .copied()
            .chain(delta.replace.iter().map(|&(_, new)| new));
        let max_block = plan.prepare.max_block().get();
        let incoming = wanted
            .map(|p| {
                let unit = units
                    .remove(&p.key)
                    .unwrap_or_else(|| panic!("no unit supplied for node {}", p.key.0));
                // A fade's scratch holds the outgoing unit's outputs, which
                // have the incoming unit's width (`verify_fades`).
                let fade = delta
                    .fades
                    .iter()
                    .find(|(k, f)| *k == p.key && !f.is_cut())
                    .map(|&(key, f)| {
                        let width = plan.unit(key).map_or(0, |u| u.shape.audio_out().count());
                        Box::new(Crossfade {
                            _control: ControlDrop::new(),
                            key,
                            out: None,
                            waiting: None,
                            scratch: Arena::new(width as usize, max_block),
                            done: 0,
                            len: f.duration.get(),
                            law: f.law,
                            chase: CappedVec::with_capacity(0),
                        })
                    });
                // The unit's param state, sized here so the audio thread
                // never allocates it (see the `param` module).
                let params = params(
                    p.key,
                    plan.unit(p.key).map_or(0, |u| u.shape.params.len()),
                    max_block,
                );
                Incoming {
                    key: p.key,
                    idx: p.idx,
                    generation: p.generation,
                    node: NodeBox::new(unit, wakes(p.key)),
                    fade,
                    params,
                    mode: modes(p.key),
                    keys: Keys::default(),
                    staging: Tables::new(),
                }
            })
            .collect::<Vec<_>>();
        // A kept unit whose declared latency changed gets the line its new
        // latency needs, built here (`Editor::set_latency` cuts it).
        let relined = delta
            .cuts
            .iter()
            .filter_map(|p| {
                let shape = &plan.unit(p.key)?.shape;
                Some(Relined {
                    idx: p.idx,
                    latency: shape.latency.samples().get(),
                    line: BypassLine::for_shape(shape),
                })
            })
            .collect::<Vec<_>>();
        assert!(
            units.is_empty(),
            "units supplied for nodes the delta does not place: {:?}",
            units.keys().collect::<Vec<_>>()
        );
        // Reserved here so applying moves retirees in without growing.
        let retired = CappedVec::with_capacity(delta.retire.len() + delta.replace.len());
        // Every incoming unit's may come back unused, and every unit leaving
        // brings its own.
        let spare_params =
            CappedVec::with_capacity(incoming.len() + delta.retire.len() + delta.replace.len());
        let delta_units = delta.retire.len() + delta.replace.len();
        let spare_modes = CappedVec::with_capacity(incoming.len() + delta_units);
        let spare_lines = CappedVec::with_capacity(relined.len());
        Box::new(Self {
            _control: ControlDrop::new(),
            seq,
            suspend: None,
            plan: Some(plan),
            delta,
            incoming,
            relined,
            retired,
            spare_params,
            spare_modes,
            spare_lines,
            spare_held: CappedVec::with_capacity(0),
            old_state: None,
            staged: None,
            gone: Gone::default(),
            parked: CappedVec::with_capacity(0),
            opened,
        })
    }

    /// A re-prepare's first half: when applied, every unit leaves the store
    /// and comes back in this box, and the executor adopts `prepare`.
    /// `units` is how many the running plan has, reserved here so applying
    /// moves them in without growing.
    pub(crate) fn suspend(seq: u64, prepare: Prepare, units: usize) -> Box<Commit<F>> {
        Box::new(Self {
            _control: ControlDrop::new(),
            seq,
            suspend: Some(prepare),
            plan: None,
            delta: Delta::default(),
            incoming: Vec::new(),
            relined: Vec::new(),
            retired: CappedVec::with_capacity(units),
            spare_params: CappedVec::with_capacity(units),
            spare_modes: CappedVec::with_capacity(units),
            spare_lines: CappedVec::with_capacity(0),
            spare_held: CappedVec::with_capacity(units),
            old_state: None,
            staged: None,
            gone: Gone::default(),
            parked: CappedVec::with_capacity(units),
            opened: Opened::default(),
        })
    }

    /// Whether this commit crossfades `key` in.
    pub(super) fn fades_in(&self, key: NodeKey) -> bool {
        self.incoming
            .iter()
            .any(|i| i.fade.as_ref().is_some_and(|x| x.key == key))
    }

    /// Items the lists this box carries back refused when it was applied:
    /// each was leaked, not freed on the audio thread. Control side, once
    /// the box is back.
    pub(crate) fn refused(&self) -> usize {
        [
            self.retired.refused(),
            self.spare_params.refused(),
            self.spare_modes.refused(),
            self.spare_lines.refused(),
            self.spare_held.refused(),
            self.parked.refused(),
            self.gone.refused(),
            self.staged.as_ref().map_or(0, |s| s.refused()),
        ]
        .into_iter()
        .fold(0, usize::saturating_add)
    }

    /// Whether this box is a re-prepare's first half.
    pub(crate) fn is_suspend(&self) -> bool {
        self.suspend.is_some()
    }

    /// The units applying removed, by key.
    pub(crate) fn retired(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.retired.iter().map(|(k, _)| *k)
    }

    /// The units applying removed, out of their boxes (control side).
    pub(crate) fn take_retired(&mut self) -> Vec<(NodeKey, Box<dyn Node<F>>)> {
        self.retired
            .drain(..)
            .map(|(k, b)| (k, b.into_inner()))
            .collect()
    }
}

/// Send an applied box back to the editor. The push cannot fail by the
/// credit count (module docs); were it ever full, or the editor gone (nothing
/// will collect it), leak rather than free on the audio thread.
pub(super) fn send_back<F: Float>(back: &mut RingWriter<Box<Commit<F>>>, commit: Box<Commit<F>>) {
    if let Err(refused) = back.push(commit) {
        debug_assert!(
            refused.error == PushError::Disconnected,
            "the return ring is full"
        );
        std::mem::forget(refused);
    }
}

/// Send a crossfade that ended, or was cut, superseded or never started,
/// back to the editor on the fade-return ring, with the units it carries.
/// The push cannot fail: the editor reserved this crossfade's slot before
/// sending the commit that built it (module docs). Were it ever full, or the
/// editor gone, leak rather than free on the audio thread.
pub(super) fn retire_fade<F: Float>(
    fade_back: &mut RingWriter<Box<Crossfade<F>>>,
    x: Box<Crossfade<F>>,
) {
    if let Err(refused) = fade_back.push(x) {
        debug_assert!(
            refused.error == PushError::Disconnected,
            "the fade-return ring is full"
        );
        std::mem::forget(refused);
    }
}

/// Cut `u`'s crossfades: the newest unit at the key (a waiting one, if
/// any — the one the plan's generation names) becomes `u`'s unit, and the
/// others go back on the fade-return ring with their crossfades.
///
/// Returns the keys of the unit that stopped writing, if the cut displaced
/// one (`Keys::displace`).
pub(super) fn cut_fades<F: Float>(
    u: &mut Unit<F>,
    fade_back: &mut RingWriter<Box<Crossfade<F>>>,
) -> Option<Tables> {
    let mut stale = None;
    if let Some(mut q) = u.queued.take() {
        let newest = q.waiting.take().expect("a queued fade holds its unit");
        q.out = Some(std::mem::replace(&mut u.node, newest));
        retire_fade(fade_back, q);
        stale = Some(u.keys.displace());
    }
    if let Some(f) = u.fade.take() {
        retire_fade(fade_back, f);
        // A cut is a step: never skip the next call on stale flags.
        u.quiet = 0;
        u.last_quiet = false;
        u.last_idle = false;
        u.reported = None;
    }
    stale
}

/// The queue pair between an editor and its executor.
pub(crate) struct Channels<F: Float = f32> {
    pub(crate) to_executor: RingWriter<Box<Commit<F>>>,
    pub(crate) returned: RingReader<Box<Commit<F>>>,
    /// Crossfades coming back (see the module docs, "Crossfades").
    pub(crate) faded: RingReader<Box<Crossfade<F>>>,
    /// The executor's play clock, as it last published it.
    pub(crate) play: PlayReader,
    /// Where the executor says which ports lost events.
    pub(crate) losses: RingReader<EventLoss>,
    /// Refusals the log and the ring could not name.
    pub(crate) unreported: Arc<AtomicU64>,
    /// The edges whose declick fade ended (`EdgeFade`), by the kind that did.
    pub(crate) edge_fades: RingReader<(EdgeId, EdgeFade)>,
    /// The executor's pool participants, and the blocks it has rendered
    /// (`Stager`).
    pub(crate) pool: Arc<AtomicUsize>,
    pub(crate) rendered: Arc<AtomicU64>,
    /// The `seq` of the commit whose root crossfade the executor last
    /// finished (`Editor::finish_fades`).
    pub(crate) root_fade_ended: Arc<AtomicU64>,
}

/// The executor's ends of the rings.
pub(crate) struct ExecutorEnds<F: Float = f32> {
    pub(super) queue: RingReader<Box<Commit<F>>>,
    pub(super) back: RingWriter<Box<Commit<F>>>,
    pub(super) fade_back: RingWriter<Box<Crossfade<F>>>,
    pub(super) play: Arc<AtomicU64>,
    pub(super) losses: RingWriter<EventLoss>,
    pub(super) unreported: Arc<AtomicU64>,
    pub(super) edge_fades: RingWriter<(EdgeId, EdgeFade)>,
    pub(super) pool: Arc<AtomicUsize>,
    pub(super) rendered: Arc<AtomicU64>,
    pub(super) root_fade_ended: Arc<AtomicU64>,
}

/// Build the rings: the editor's ends and the executor's.
pub(crate) fn channels<F: Float>() -> (Channels<F>, ExecutorEnds<F>) {
    let (tx, rx) = ring::<Box<Commit<F>>>(QUEUE_CAPACITY, Overflow::Refuse);
    let (back_tx, back_rx) = ring::<Box<Commit<F>>>(QUEUE_CAPACITY + 1, Overflow::Refuse);
    let (fade_tx, fade_rx) = ring::<Box<Crossfade<F>>>(FADE_CAPACITY, Overflow::Refuse);
    let play = Arc::new(AtomicU64::new(0));
    let unreported = Arc::new(AtomicU64::new(0));
    let (loss_tx, loss_rx) = ring::<EventLoss>(crate::loss::RING_CAPACITY, Overflow::Refuse);
    let (fade_end_tx, fade_end_rx) =
        ring::<(EdgeId, EdgeFade)>(EDGE_FADE_CAPACITY, Overflow::Refuse);
    let pool = Arc::new(AtomicUsize::new(0));
    let rendered = Arc::new(AtomicU64::new(0));
    let root_fade_ended = Arc::new(AtomicU64::new(0));
    (
        Channels {
            to_executor: tx,
            returned: back_rx,
            faded: fade_rx,
            play: PlayReader(Arc::clone(&play)),
            losses: loss_rx,
            unreported: Arc::clone(&unreported),
            edge_fades: fade_end_rx,
            pool: Arc::clone(&pool),
            rendered: Arc::clone(&rendered),
            root_fade_ended: Arc::clone(&root_fade_ended),
        },
        ExecutorEnds {
            queue: rx,
            back: back_tx,
            fade_back: fade_tx,
            play,
            losses: loss_tx,
            unreported,
            edge_fades: fade_end_tx,
            pool,
            rendered,
            root_fade_ended,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::MaxBlock;
    use tutti_units::{SampleRate, Samples};

    /// A box's `refused` is the sum over every list it carries: the commit's
    /// own, those of `gone`, and none for a box nothing was refused in.
    #[test]
    fn a_commits_refused_sums_every_list_it_carries() {
        let prepare = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64)));
        let mut c = Commit::<f32>::suspend(1, prepare, 0);
        assert_eq!(c.refused(), 0);
        c.spare_held.mark_overflowed();
        c.spare_held.mark_overflowed();
        c.parked.mark_overflowed();
        c.gone.spare.mark_overflowed();
        c.gone.replaced.mark_overflowed();
        assert_eq!(c.refused(), 5);
    }
}
