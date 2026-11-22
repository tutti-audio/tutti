//! [`HarmonyNode`]: a sequencer's chord and scale lanes as a graph node,
//! sending [`Harmony`] events out of an event port,
//! for a node that follows them (a hosted VST3 plugin's chord and scale
//! events).
//!
//! It walks each block's transport as a MIDI clip does (the shared walk,
//! `walk.rs`): every change on its frame, loop wraps and mid-block starts
//! included, with no play cursor. Chords and scales are **context**, not
//! notes — each holds until the next — so where playback jumps (a start, a
//! seek, a loop wrap) or the lanes are replaced, the node re-states the chord
//! and the scale in force there, on the jump's frame. A change on that frame
//! is part of that context and sent once, so no chord it replaces goes out
//! before it. A consumer joining mid-lane therefore always has the context.
//!
//! A loop wrap inside a block re-states only what differs from what the node
//! last sent: a receiver holds the context until the next change, so a short
//! loop whose passes start in the context they end in sends only its
//! changes, not two events a pass. Those changes are still bounded per block
//! by [`HARMONY_EVENT_DENSITY`] (a loop of a few frames with a change in it
//! sends one a pass); past it the port refuses, and the executor counts it.
//!
//! The lanes are [`HarmonyChanges`] published to the node with [`Publisher`]
//! ([`HarmonyControls::set`]); the node reads them once per block.

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use tutti_buffers::Float;
use tutti_rt::LockExt;

use tutti_graph::{
    Cx, Event, EventDensity, EventWriter, ForkCause, ForkMode, ForkSource, Forked, Harmony,
    HarmonyKind, IntoNode, Io, MIN_EVENT_PORT_CAPACITY, Node, NodeParts, Offset, Prepare, Shape,
    Status,
};
use tutti_rt::{Publisher, Reader};
use tutti_units::Beat;

use super::walk::{Beated, Visit, Walk, lands_on_first_frame};

/// The events per frame a [`HarmonyNode`]'s port is sized for, which with its
/// block makes its declared event capacity: two a frame, the most a walk
/// sends (a loop wraps at most once a frame, and a wrap re-states a chord and
/// a scale), between 256 events for a short block and the cap
/// ([`EventDensity::capacity`]). The cost is 8 KB at 128 frames and 64 KB at
/// 1,024. Past it the rest are refused and counted
/// (`Editor::take_event_losses`); a chord or scale is context, not a note, so
/// a refusal leaves no note hanging and the next jump or wrap re-states it.
pub const HARMONY_EVENT_DENSITY: EventDensity = EventDensity::per_frame(2);

/// A chord or scale taking effect at a beat.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimedHarmony {
    /// Where it takes effect.
    pub beat: Beat,
    /// The chord or scale.
    pub harmony: Harmony,
}

impl TimedHarmony {
    /// `harmony` from `beat`.
    #[must_use]
    pub const fn new(beat: Beat, harmony: Harmony) -> Self {
        Self { beat, harmony }
    }
}

impl Beated for TimedHarmony {
    fn beat(&self) -> Beat {
        self.beat
    }
}

/// A sequencer's chord and scale lanes as they are published: the changes
/// sorted by beat, each at a finite beat of its own. Build them from
/// [`TimedHarmony`]s in any order with [`new`](Self::new), which refuses what
/// the lanes cannot hold. Two are equal when their changes are.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HarmonyChanges {
    changes: Box<[TimedHarmony]>,
}

/// What [`HarmonyChanges::new`] refuses.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub enum HarmonyChangesError {
    /// The change at `index` of the input has a NaN or infinite beat.
    #[error("the harmony change at index {index} has a beat that is not finite")]
    NonFiniteBeat {
        /// Its position in the input.
        index: usize,
    },
    /// Two changes of one `kind` take effect at `beat`: which of them holds
    /// is not a choice the lanes make. (A chord and a scale at one beat are
    /// two lanes, not a clash.)
    #[error("two harmony changes of one kind ({kind:?}) take effect at beat {}", .beat.get())]
    SameBeat {
        /// The beat both name.
        beat: Beat,
        /// What both are, a chord or a scale.
        kind: HarmonyKind,
    },
}

impl HarmonyChanges {
    /// The lanes of `changes`, in any order, sorted by beat.
    ///
    /// # Errors
    ///
    /// [`HarmonyChangesError::NonFiniteBeat`] for a change at a NaN or
    /// infinite beat (the first one, by input position), and
    /// [`HarmonyChangesError::SameBeat`] for two chords, or two scales, at one
    /// beat. A chord and a scale at one beat are both kept: each holds in its
    /// own lane.
    ///
    /// ```
    /// use tutti_graph::{Harmony, HarmonyKind};
    /// use tutti_midi_runtime::{HarmonyChanges, HarmonyChangesError, TimedHarmony};
    /// use tutti_units::Beat;
    ///
    /// let c_major = Harmony::chord(60, 60, 0b1001_0001);
    /// let at = |beat| TimedHarmony::new(Beat::new(beat), c_major);
    ///
    /// let lanes = HarmonyChanges::new([at(4.0), at(0.0)]).unwrap();
    /// assert_eq!(lanes.as_slice()[0].beat, Beat::new(0.0));
    /// assert_eq!(
    ///     HarmonyChanges::new([at(0.0), at(f64::NAN)]),
    ///     Err(HarmonyChangesError::NonFiniteBeat { index: 1 }),
    /// );
    /// assert_eq!(
    ///     HarmonyChanges::new([at(1.0), at(1.0)]),
    ///     Err(HarmonyChangesError::SameBeat {
    ///         beat: Beat::new(1.0),
    ///         kind: HarmonyKind::Chord,
    ///     }),
    /// );
    /// ```
    pub fn new(
        changes: impl IntoIterator<Item = TimedHarmony>,
    ) -> Result<Self, HarmonyChangesError> {
        let mut v: Vec<TimedHarmony> = changes.into_iter().collect();
        if let Some(index) = v.iter().position(|c| Beat::finite(c.beat.get()).is_none()) {
            return Err(HarmonyChangesError::NonFiniteBeat { index });
        }
        v.sort_by(|a, b| a.beat.get().total_cmp(&b.beat.get()));
        // Sorted, so the changes at one beat are neighbours, and a beat holds
        // at most one chord and one scale.
        for (i, a) in v.iter().enumerate() {
            let clash = v[i + 1..]
                .iter()
                .take_while(|b| b.beat == a.beat)
                .any(|b| b.harmony.kind() == a.harmony.kind());
            if clash {
                return Err(HarmonyChangesError::SameBeat {
                    beat: a.beat,
                    kind: a.harmony.kind(),
                });
            }
        }
        Ok(Self {
            changes: v.into_boxed_slice(),
        })
    }

    /// The changes, in beat order.
    #[must_use]
    pub fn as_slice(&self) -> &[TimedHarmony] {
        &self.changes
    }

    /// How many changes the lanes hold.
    #[must_use]
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Whether the lanes hold no change.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// A sequencer's chord and scale lanes as a graph node with one event output.
/// See the module docs.
///
/// ```
/// use tutti_buffers::MaxBlock;
/// use tutti_units::{Beat, SampleRate, Samples};
/// use tutti_graph::NodeKey;
/// use tutti_graph::{Editor, Harmony, Prepare};
/// use tutti_midi_runtime::{HarmonyChanges, HarmonyNode, TimedHarmony};
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(512))));
/// let c_major = Harmony::chord(60, 60, 0b1001_0001);
/// let changes = HarmonyChanges::new([TimedHarmony::new(Beat::new(0.0), c_major)]).unwrap();
/// let node = HarmonyNode::new(changes);
/// let controls = editor.insert(NodeKey(1), "harmony", node).controls;
/// assert_eq!(controls.len(), 1);
/// ```
pub struct HarmonyNode {
    /// The cell `lanes` reads, which the controls publish to; the node never
    /// touches it.
    cell: Publisher<HarmonyChanges>,
    /// A reader of `cell`, registered in [`over`](Self::over): a fork's node
    /// is built over its own cell and registers there.
    lanes: Reader<HarmonyChanges>,
    /// The publish number of the lanes this node last walked. A lend may return
    /// number `N` while the value is already `N + 1`, so a replacement can be
    /// seen one block late, never missed and never seen twice.
    last_seq: u64,
    walk: Walk,
    /// The chord and the scale this node last sent: a loop wrap re-states
    /// only what differs.
    sent: Sent,
    /// The declared event capacity, from the block the node was prepared for.
    capacity: NonZeroU32,
}

impl std::fmt::Debug for HarmonyNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarmonyNode")
            .field("lanes", &self.lanes)
            .field("last_seq", &self.last_seq)
            .field("walk", &self.walk)
            .field("sent", &self.sent)
            .finish_non_exhaustive()
    }
}

impl HarmonyNode {
    /// A node sending `changes`.
    #[must_use]
    pub fn new(changes: HarmonyChanges) -> Self {
        Self::over(Publisher::new(changes))
    }

    /// A node over `cell`, reading it through a reader registered here.
    /// Control thread: registering may allocate.
    fn over(cell: Publisher<HarmonyChanges>) -> Self {
        Self {
            lanes: cell.reader(),
            last_seq: cell.seq(),
            cell,
            walk: Walk::default(),
            sent: Sent::default(),
            capacity: MIN_EVENT_PORT_CAPACITY,
        }
    }
}

/// The host's handle on a [`HarmonyNode`] in a graph: replace the lanes it
/// sends. Cheap to clone; every clone reaches the same node.
#[derive(Clone)]
pub struct HarmonyControls {
    cell: Publisher<HarmonyChanges>,
    /// Serializes `set`'s compare and publish (`Publisher` has no
    /// compare-and-publish), so two control threads cannot each publish
    /// against a stale read. The node never takes it.
    serial: Arc<Mutex<()>>,
}

impl std::fmt::Debug for HarmonyControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarmonyControls").finish_non_exhaustive()
    }
}

impl HarmonyControls {
    /// Sends `changes` from the next block on, the node re-stating the context
    /// in force on that block's first frame. Changes equal to the lanes in
    /// force change nothing. Control thread.
    pub fn set(&self, changes: impl Into<Arc<HarmonyChanges>>) {
        let changes = changes.into();
        let _serial = self.serial.lock_recover();
        let current = self.cell.current();
        if Arc::ptr_eq(&current, &changes) || *current == *changes {
            return;
        }
        self.cell.publish(changes);
    }

    /// The lanes in force: the ones the node sends from its next block.
    #[must_use]
    pub fn current(&self) -> Arc<HarmonyChanges> {
        self.cell.current()
    }

    /// How many changes the lanes hold.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cell.current().len()
    }

    /// Whether the lanes hold no change.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The chord and the scale a node last sent, by [`HarmonyKind`]: `None`
/// before it sent one, and after a reset or a replacement of its lanes.
#[derive(Clone, Copy, Debug, Default)]
struct Sent([Option<Harmony>; 2]);

impl Sent {
    fn slot(&mut self, kind: HarmonyKind) -> &mut Option<Harmony> {
        &mut self.0[usize::from(kind == HarmonyKind::Scale)]
    }
}

/// The harmony side of the walk: each change on its frame, at a jump the
/// chord and the scale in force there, and at a loop wrap those of them that
/// differ from what was sent.
struct HarmonyWriter<'a, 'w> {
    out: &'a mut EventWriter<'w>,
    sent: &'a mut Sent,
    /// The frame this block last re-stated the context on: the changes the
    /// walk visits on it are already part of what was re-stated.
    restated: Option<Offset>,
}

impl HarmonyWriter<'_, '_> {
    /// Send `h` at `at`, recording it when the port takes it.
    fn send(&mut self, at: Offset, h: Harmony) {
        // Refused past the capacity: counted by the executor.
        if self.out.push(Event::harmony(at, h)).is_ok() {
            *self.sent.slot(h.kind()) = Some(h);
        }
    }

    /// Send, at `at`, the chord and the scale in force on that frame, where
    /// playback is at `beat`: changes on the frame included, so no chord
    /// goes out that the frame's own change replaces. With `changed_only`,
    /// only those that differ from what was last sent.
    fn restate(
        &mut self,
        at: Option<Offset>,
        beat: Beat,
        frames_per_beat: f64,
        changes: &[TimedHarmony],
        changed_only: bool,
    ) {
        let Some(at) = at else { return };
        self.restated = Some(at);
        for kind in [HarmonyKind::Chord, HarmonyKind::Scale] {
            if let Some(h) = Self::in_force(changes, beat, frames_per_beat, kind)
                && (!changed_only || *self.sent.slot(kind) != Some(h))
            {
                self.send(at, h);
            }
        }
    }

    /// The latest change of `kind` that lands on or before the frame
    /// playback is on at `beat`.
    fn in_force(
        changes: &[TimedHarmony],
        beat: Beat,
        frames_per_beat: f64,
        kind: HarmonyKind,
    ) -> Option<Harmony> {
        let upto = changes.partition_point(|c| lands_on_first_frame(c.beat, beat, frames_per_beat));
        changes[..upto]
            .iter()
            .rev()
            .find(|c| c.harmony.kind() == kind)
            .map(|c| c.harmony)
    }
}

impl Visit<TimedHarmony> for HarmonyWriter<'_, '_> {
    fn stop(&mut self, _at: Option<Offset>) {}

    /// Re-state the context in force on the jump's frame.
    fn jump(&mut self, at: Option<Offset>, beat: Beat, fpb: f64, changes: &[TimedHarmony]) {
        self.restate(at, beat, fpb, changes, false);
    }

    /// A loop pass re-states only what differs from what was last sent: a
    /// receiver holds the context, so a pass that starts in the context the
    /// last one ended in sends nothing here.
    fn wrap(&mut self, at: Option<Offset>, beat: Beat, fpb: f64, changes: &[TimedHarmony]) {
        self.restate(at, beat, fpb, changes, true);
    }

    fn event(&mut self, at: Offset, c: &TimedHarmony) {
        if self.restated != Some(at) {
            self.send(at, c.harmony);
        }
    }
}

impl<F: Float> Node<F> for HarmonyNode {
    /// No audio, one event output of [`HARMONY_EVENT_DENSITY`] per frame of
    /// the block it was prepared for.
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .events(0, 1)
            .event_capacity(self.capacity)
            // Event-only: the clip node's walk over its changes, nothing per
            // frame.
            .cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.capacity = HARMONY_EVENT_DENSITY.capacity(p);
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let lanes = self.lanes.lend();
        let seq = lanes.seq();
        if std::mem::replace(&mut self.last_seq, seq) != seq {
            self.walk.forget();
            self.sent = Sent::default();
        }
        let mut writer = HarmonyWriter {
            out: io.event_out(0),
            sent: &mut self.sent,
            restated: None,
        };
        for (start, seg) in cx.env.segments() {
            self.walk
                .segment(&lanes.changes, cx.env, start, &seg, &mut writer);
        }
        Status::Modified
    }

    /// Start over: the next block re-states the context.
    fn reset(&mut self) {
        self.walk.forget();
        self.sent = Sent::default();
    }
}

/// A fork of a [`HarmonyNode`]: its lanes as they stand, its own cell.
struct HarmonyFork {
    cell: Publisher<HarmonyChanges>,
}

impl<F: Float> ForkSource<F> for HarmonyFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked<F>, ForkCause> {
        let cell = Publisher::from_arc(self.cell.current());
        Ok(Forked::new(Box::new(HarmonyNode::over(cell))))
    }
}

impl<F: Float> IntoNode<F> for HarmonyNode {
    type Controls = HarmonyControls;

    fn into_parts(self) -> NodeParts<HarmonyControls, F> {
        let controls = HarmonyControls {
            cell: self.cell.clone(),
            serial: Arc::default(),
        };
        let fork = HarmonyFork {
            cell: self.cell.clone(),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

/// Frees what the control side retired and no block holds, without publishing
/// (the lanes a replacement left behind); control thread, from the owner's tick.
impl tutti_rt::Reclaim for HarmonyControls {
    fn reclaim(&self) -> usize {
        self.cell.reclaim()
    }
}

#[cfg(test)]
mod tests;
