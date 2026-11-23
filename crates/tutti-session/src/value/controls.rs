//! Targets on the graph's controls: a node's [`NodeMode`] and [`Mix`], and an
//! edge's gain.
//!
//! The graph keeps these in its editor, which the verbs cannot reach, so each
//! target holds the value in a cell of its own: a write checks the value
//! against what the node or edge can take, stores it (a read answers it at
//! once) and queues it, and the session hands the queue to the editor
//! ([`Graph::apply_controls`](crate::Graph)) when the update commits. The
//! running node reads it from the block after, with no commit and no edit.

use std::sync::Arc;

use tutti_graph::{NodeKey, NodeMode, PassError};
use tutti_rt::Mutex;
use tutti_units::{Gain, Mix};

use super::error::SetRefusal;
use super::slot::{Slot, SlotCell, SlotId, keyed_slot_target};
use super::state::Params;
use super::target::{
    Effective, Modulatable, ScalarTarget, Target, TargetId, sealed, target_builders,
};
use crate::ChannelEdge;

/// A control written and not yet handed to the editor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Write {
    Mode(NodeKey, NodeMode),
    Mix(NodeKey, Mix),
    Gain(ChannelEdge, Gain),
}

impl Write {
    /// Whether `other` writes the same control: a later write replaces it.
    fn same_control(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Mode(a, _), Self::Mode(b, _)) | (Self::Mix(a, _), Self::Mix(b, _)) => a == b,
            (Self::Gain(a, _), Self::Gain(b, _)) => a == b,
            _ => false,
        }
    }
}

/// The controls written since the last update, one write per control.
#[derive(Debug, Default)]
pub(crate) struct Queue(Mutex<Vec<Write>>);

impl Queue {
    fn push(&self, write: Write) {
        let mut writes = self.0.lock();
        match writes.iter_mut().find(|w| w.same_control(&write)) {
            Some(held) => *held = write,
            None => writes.push(write),
        }
    }

    /// Drops the writes to `node`'s mode and mix.
    pub(crate) fn forget_node(&self, node: NodeKey) {
        self.0.lock().retain(|w| match w {
            Write::Mode(n, _) | Write::Mix(n, _) => *n != node,
            Write::Gain(..) => true,
        });
    }

    /// Drops the writes to `edge`'s gain.
    pub(crate) fn forget_edge(&self, edge: ChannelEdge) {
        self.0
            .lock()
            .retain(|w| !matches!(w, Write::Gain(e, _) if *e == edge));
    }

    /// Every write since the last call, in the order each control was first
    /// written.
    pub(crate) fn take(&self) -> Vec<Write> {
        std::mem::take(&mut *self.0.lock())
    }
}

/// What a node's mode and mix take: whether it has a pass.
#[derive(Clone, Copy, Debug)]
struct NodeState {
    mode: NodeMode,
    mix: Mix,
    /// Why the node has no pass, when it has none: where its input goes when
    /// it does not run is a guess.
    no_pass: Option<PassError>,
    /// Whether the node bypasses itself (a plugin with a bypass parameter),
    /// which needs no pass.
    own_bypass: bool,
}

/// One node's mode and mix, shared by its two targets.
#[derive(Debug)]
pub(crate) struct NodeControl {
    node: NodeKey,
    state: Mutex<NodeState>,
    queue: Arc<Queue>,
}

impl NodeControl {
    pub(crate) fn new(
        node: NodeKey,
        no_pass: Option<PassError>,
        own_bypass: bool,
        queue: Arc<Queue>,
    ) -> Arc<Self> {
        Arc::new(Self {
            node,
            state: Mutex::new(NodeState {
                mode: NodeMode::Active,
                mix: Mix::WET,
                no_pass,
                own_bypass,
            }),
            queue,
        })
    }

    /// The node's pass, as its shape stands now.
    pub(crate) fn note_pass(&self, no_pass: Option<PassError>, own_bypass: bool) {
        let mut state = self.state.lock();
        state.no_pass = no_pass;
        state.own_bypass = own_bypass;
    }
}

/// A node's mode: the [`Slot`] of its [`ModeTarget`].
pub(crate) struct ModeSlot(pub(crate) Arc<NodeControl>);

impl Slot<NodeMode> for ModeSlot {
    fn load(&self) -> NodeMode {
        self.0.state.lock().mode
    }

    fn check(&self, value: &NodeMode) -> Result<(), SetRefusal> {
        let state = *self.0.state.lock();
        match state.no_pass {
            Some(error) if *value == NodeMode::Bypassed && !state.own_bypass => {
                Err(SetRefusal::Unplaced(error))
            }
            _ => Ok(()),
        }
    }

    fn store(&self, value: &NodeMode) {
        self.0.state.lock().mode = *value;
        self.0.queue.push(Write::Mode(self.0.node, *value));
    }
}

/// A node's mix: the [`Slot`] of its [`MixTarget`].
pub(crate) struct MixSlot(pub(crate) Arc<NodeControl>);

impl Slot<Mix> for MixSlot {
    fn load(&self) -> Mix {
        self.0.state.lock().mix
    }

    fn check(&self, value: &Mix) -> Result<(), SetRefusal> {
        if !value.get().is_finite() {
            return Err(SetRefusal::NotFinite);
        }
        if !(0.0..=1.0).contains(&value.get()) {
            return Err(SetRefusal::OutOfRange);
        }
        match self.0.state.lock().no_pass {
            Some(error) if *value != Mix::WET => Err(SetRefusal::Unplaced(error)),
            _ => Ok(()),
        }
    }

    fn store(&self, value: &Mix) {
        self.0.state.lock().mix = *value;
        self.0.queue.push(Write::Mix(self.0.node, *value));
    }
}

/// One edge's gain lane: the [`Slot`] of its [`GainTarget`].
pub(crate) struct GainSlot {
    edge: ChannelEdge,
    value: Mutex<Gain>,
    queue: Arc<Queue>,
}

impl GainSlot {
    pub(crate) fn new(edge: ChannelEdge, seed: Gain, queue: Arc<Queue>) -> Arc<Self> {
        Arc::new(Self {
            edge,
            value: Mutex::new(seed),
            queue,
        })
    }
}

impl Slot<Gain> for GainSlot {
    fn load(&self) -> Gain {
        *self.value.lock()
    }

    fn check(&self, value: &Gain) -> Result<(), SetRefusal> {
        if !value.get().is_finite() {
            return Err(SetRefusal::NotFinite);
        }
        Ok(())
    }

    fn store(&self, value: &Gain) {
        *self.value.lock() = *value;
        self.queue.push(Write::Gain(self.edge, *value));
    }
}

keyed_slot_target!(
    /// A node's [`NodeMode`], as [`Key::mode`](crate::Key::mode) names it:
    /// [`Active`](NodeMode::Active) runs the node,
    /// [`Bypassed`](NodeMode::Bypassed) passes its input through, delayed by
    /// its declared latency, and [`Muted`](NodeMode::Muted) silences its
    /// outputs while it keeps running. A change fades over
    /// [`DECLICK`](tutti_graph::DECLICK).
    ///
    /// A write is stored at once (a read answers it) and reaches the node
    /// with the commit of the next [`Session::update`](crate::Session::update),
    /// as a control: the graph is not recompiled and nothing is marked
    /// edited. A bypass of a node with no pass
    /// ([`PassError`]: its buses do not say where its input goes) is refused
    /// with [`SetRefusal::Unplaced`].
    ModeTarget,
    NodeMode,
    NodeKey,
    Mode
);

keyed_slot_target!(
    /// A node's [`Mix`], its blend with its own aligned input
    /// (`out = y * m + x̂ * (1 - m)`), as [`Key::mix`](crate::Key::mix) names
    /// it: [`Mix::WET`] is the node alone and [`Mix::DRY`] its input alone,
    /// with the node still running. The mode wins: a bypassed or muted node
    /// ignores it. A route onto the mix ([`MixRef`](crate::MixRef)) modulates
    /// around this value.
    ///
    /// Stored and landed as [`ModeTarget`] is. NaN and infinity are refused
    /// with [`SetRefusal::NotFinite`], a mix outside `0..=1` with
    /// [`SetRefusal::OutOfRange`], and one short of [`Mix::WET`] on a node
    /// with no pass with [`SetRefusal::Unplaced`].
    MixTarget,
    Mix,
    NodeKey,
    Mix
);

impl<N: ?Sized> crate::Key<N> {
    /// The node's mode.
    #[must_use]
    pub const fn mode(self) -> ModeTarget {
        ModeTarget::new(self.erase())
    }

    /// The node's mix.
    #[must_use]
    pub const fn mix(self) -> MixTarget {
        MixTarget::new(self.erase())
    }
}

impl ModeTarget {
    /// The mode of `node`, whose key a plugin or an erased handle holds. The
    /// target is `NoSuchTarget` once the node is not in the graph.
    #[must_use]
    pub fn of(node: impl Into<NodeKey>) -> Self {
        Self::new(node.into())
    }

    /// The node whose mode it is.
    #[must_use]
    pub const fn node(self) -> NodeKey {
        self.0
    }
}

impl MixTarget {
    /// The mix of `node`, whose key a plugin or an erased handle holds. The
    /// target is `NoSuchTarget` once the node is not in the graph.
    #[must_use]
    pub fn of(node: impl Into<NodeKey>) -> Self {
        Self::new(node.into())
    }

    /// The node whose mix it is.
    #[must_use]
    pub const fn node(self) -> NodeKey {
        self.0
    }
}

keyed_slot_target!(
    /// The gain of one audio edge, a weight of the sum its sink holds, as
    /// [`Graph::gain_target`](crate::Graph::gain_target) names it: `1.0` is the plain sum, `0.0` mutes the
    /// edge and a negative gain inverts it. A route onto the edge
    /// ([`ChannelEdge`]) moves the gain around this value.
    ///
    /// Stored and landed as [`ModeTarget`] is, and a move ramps over one
    /// block. NaN and infinity are refused with [`SetRefusal::NotFinite`].
    /// The edge has the target while it has a gain lane; a call that names
    /// an edge whose lane is gone fails with `NoSuchTarget`.
    GainTarget,
    Gain,
    ChannelEdge,
    Gain
);

impl Modulatable for GainTarget {}
impl ScalarTarget for GainTarget {
    type Unit = Gain;
}

impl sealed::Lane for GainTarget {
    fn delivered(self, params: &Params) -> Option<Gain> {
        params.gain_lane(self.0).map(Gain::new)
    }
}

impl GainTarget {
    /// The value the audio thread uses, which a route onto the edge may have
    /// moved from the base.
    pub fn effective(self) -> Effective<Self> {
        Effective { target: self }
    }

    /// The edge the gain is of.
    #[must_use]
    pub const fn edge(self) -> ChannelEdge {
        self.0
    }
}

/// Why [`Graph::gain_target`](crate::Graph::gain_target) named no gain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GainTargetError {
    /// The edge is not in the graph, or has no gain lane: a lane comes from
    /// the wire's declaration ([`Connection::gain`](crate::Connection::gain))
    /// and is never added by a write.
    #[error("no gain lane on {0:?}")]
    NoLane(ChannelEdge),
    /// The edge is a lane a bus edge lowers to
    /// ([`Graph::connect`](crate::Graph::connect) between buses): its weight is
    /// the fold's, and a bus edge carries no gain of its own.
    #[error("the edge is a lane of a bus edge: {0:?}")]
    BusLane(ChannelEdge),
}
