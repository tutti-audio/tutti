//! What the executor holds for a plan: its units, delay rings and
//! injections, and the state a plan's ops run against.

use tutti_types::Tail;

use super::commit::{Crossfade, NodeBox};
use super::node_op::{CONSTANT, SILENT};
use crate::NodeKey;
use crate::arena::Arena;
use crate::event::Event;
use crate::event_tap::EventTapWriter;
use crate::gain::SumLane;
use crate::held::{Keys, Tables};
use crate::kernels::{AudioRing, EventFifo};
use crate::meter::MeterWriter;
use crate::mode::ModeState;
use crate::par::Par;
use crate::param::ParamState;
use crate::tap::TapWriter;

pub(crate) struct Unit {
    pub(super) generation: u32,
    pub(super) node: NodeBox,
    /// Consecutive frames of fully silent input, saturating.
    pub(super) quiet: u64,
    /// Whether the last call (or skip) left every output silent and wrote no
    /// event.
    pub(super) last_quiet: bool,
    /// Whether the last call returned [`Status::Idle`](crate::Status::Idle).
    pub(super) last_idle: bool,
    /// The tail its last call narrowed its ceiling to
    /// ([`Io::report_tail`](crate::Io::report_tail), clamped to the
    /// ceiling); `None` is the ceiling. Written only by the task that runs
    /// the unit, so the skip it decides is a function of the unit's own
    /// inputs and reports.
    pub(super) reported: Option<Tail>,
    /// The crossfade running here: `node` is its incoming unit.
    pub(super) fade: Option<Box<Crossfade>>,
    /// A crossfade waiting for `fade` to end.
    pub(super) queued: Option<Box<Crossfade>>,
    /// Its declared params' modulation state and per-frame buffers.
    pub(super) params: ParamState,
    /// The keys its event outputs hold or owe (`held.rs`).
    pub(super) keys: Keys,
    /// Its mode and the state that applies it (`mode.rs`).
    pub(super) mode: ModeState,
}

impl Unit {
    pub(super) fn new(
        generation: u32,
        node: NodeBox,
        params: ParamState,
        mode: ModeState,
        keys: Keys,
    ) -> Self {
        Self {
            generation,
            node,
            quiet: 0,
            last_quiet: false,
            last_idle: false,
            reported: None,
            fade: None,
            queued: None,
            params,
            keys,
            mode,
        }
    }
}

/// What a commit takes away from the keys held at a node: the tables of the
/// keys retired or replaced, and the keys replaced.
#[derive(Default)]
pub(super) struct Gone {
    pub(super) held: Vec<(NodeKey, Tables)>,
    /// Emptied tables to free on the control side.
    pub(super) spare: Vec<Tables>,
    pub(super) replaced: Vec<NodeKey>,
    /// What each replaced node wrote for its successor (`Node::chase`):
    /// the key, its span of `chase_events` and what its buffer refused.
    pub(super) chased: Vec<(NodeKey, std::ops::Range<usize>, u32)>,
    /// The executor's chase buffer, lent for the commit.
    pub(super) chase_events: Vec<Event>,
    /// What a chase held for a successor behind a running fade refused, by
    /// key, counted once the new plan says where the node is.
    pub(super) held_refused: Vec<(NodeKey, u32)>,
}

pub(crate) enum Ring {
    Audio(AudioRing),
    Event(EventFifo),
}

/// Events flushed from a delay that went away, waiting for their sink's next
/// call, at their offsets relative to the earliest (see the module docs).
/// Built on the control side with room for everything that can flow in
/// (`stage.rs`); `live` is whether anything was flushed into it.
pub(crate) struct Inject {
    pub(crate) unit: u32,
    pub(super) port: u16,
    pub(super) events: Vec<Event>,
    /// Where `events` and the sink's own slot events are merged, sized when
    /// applying so the merge never grows.
    pub(super) merged: Vec<Event>,
    pub(super) live: bool,
    /// While a commit's flush is written: how many of `events` came before
    /// the closes of the edges it cut.
    pub(super) closed_from: usize,
    /// What the edges this commit cut held and `events` had no room for: one
    /// table for each edge, built with the commit. Drained into the sink's
    /// events at the end of each block, as much as the block takes, until
    /// empty; a table never overflows, so no note off is lost.
    pub(super) owed: Vec<crate::held::Held>,
}

impl Inject {
    /// Whether closes are still owed to the sink.
    pub(super) fn draining(&self) -> bool {
        self.owed.iter().any(|h| !h.is_empty())
    }

    /// Appends what is owed to `events`, as far as it takes, at the offset of
    /// the last event it holds: after everything flushed to the sink, and
    /// ahead of what its sources write at that offset, as the closes of a cut
    /// edge are (a close never passes the note it closes).
    pub(super) fn drain_owed(&mut self) {
        let end = self
            .events
            .last()
            .map_or(crate::time::Offset::ZERO, |e| e.offset);
        let start = self.events.len();
        for h in &mut self.owed {
            h.drain_into(&mut self.events);
        }
        for e in &mut self.events[start..] {
            e.offset = end;
        }
    }

    /// What the sink's next call has consumed: the flushed events go, the
    /// sink stays live while closes are owed.
    pub(super) fn consumed(&mut self) {
        self.events.clear();
        self.merged.clear();
        self.live = self.draining();
    }

    /// The sink is parked: what was flushed and owed is discarded with the
    /// events it would have been sent.
    pub(super) fn discard(&mut self) {
        self.owed.iter_mut().for_each(crate::held::Held::clear);
        self.consumed();
    }
}

/// Everything the executor rebuilds when applying: the arenas, the delay and
/// feedback state, and pending injections. Retired into the commit box
/// whole, so the old one is freed on the control side.
pub(super) struct State {
    pub(super) arena: Arena,
    /// Per audio slot, what is known about its block: [`SILENT`] and
    /// [`CONSTANT`] bits. One byte per slot, so a node's input masks are
    /// built with a load and two shifts per channel. The parallel executor
    /// runs on a shared atomic copy (`par.rs`).
    pub(super) flags: Vec<u8>,
    pub(super) events: Vec<Vec<Event>>,
    pub(super) rings: Vec<Option<Ring>>,
    pub(super) audio_fb: Vec<Option<AudioRing>>,
    pub(super) event_fb: Vec<Option<EventFifo>>,
    /// One producer per planned tap lane (`None`: nobody opened it).
    pub(super) taps: Vec<Option<TapWriter>>,
    /// One producer per planned event tap (`None`: nobody opened it).
    pub(super) event_taps: Vec<Option<EventTapWriter>>,
    /// One set of cells per planned meter (`None`: nobody opened it).
    pub(super) meters: Vec<Option<MeterWriter>>,
    /// One state per entry of `Plan::sum_lanes`: a weighted lane's weight
    /// across blocks, or a plain addend's none.
    pub(super) lanes: Vec<SumLane>,
    pub(super) inject: Vec<Inject>,
    pub(super) has_inject: Vec<bool>,
    /// Plan units with a scheduled command landing this block.
    pub(super) has_due: Vec<bool>,
    /// Where a node's scheduled events are merged into its inputs; sized
    /// when applying so the merge never grows (`command::overlay_capacity`).
    pub(super) overlay: Vec<Event>,
    /// The parallel executor's state for this plan, when the executor has a
    /// pool and the plan has tasks to spread (`par.rs`).
    pub(super) par: Option<Par>,
}

impl State {
    pub(super) fn empty(max_block: usize) -> Self {
        Self {
            arena: Arena::new(1, max_block),
            flags: vec![SILENT | CONSTANT],
            events: Vec::new(),
            rings: Vec::new(),
            audio_fb: Vec::new(),
            event_fb: Vec::new(),
            taps: Vec::new(),
            event_taps: Vec::new(),
            meters: Vec::new(),
            lanes: Vec::new(),
            inject: Vec::new(),
            has_inject: Vec::new(),
            has_due: Vec::new(),
            overlay: Vec::new(),
            par: None,
        }
    }
}
