//! A host's own MIDI sink (feature `midi`): [`Session::open_midi_sink`] opens
//! a [`MidiOut`] the app implements and returns a [`MidiSink`].
//!
//! A sink is an output as a [`MidiOutput`](crate::MidiOutput) port is: a
//! [`MidiOutNode`](tutti_midi_runtime::MidiOutNode) of its own, fed by the
//! session's clock, whose events the session sends to the sink each update at
//! the time its frame has on the OS clock (the same drain, the same timing, as a
//! device's). A sink needs no device, so it reads [`Presence::Live`] for as
//! long as a clone of its handle lives, and closes when the last is dropped.

use std::convert::Infallible;
use std::sync::{Arc, Weak};

use tutti_graph::{EventTapReader, Losses, NodeKey};
use tutti_midi_runtime::MidiOut;

use crate::midi::{
    LossReaders, LossWriters, OutputTiming, Stamping, UmpOut, drain_events, loss_cells,
};
use crate::{Peer, Presence, Resource, ResourceId, Session};

// --- The options ---

/// What a sink is sent with: the options of [`Session::open_midi_sink_with`].
///
/// Like `OutputOptions` for a device's port, these are
/// the options only a host's sink has: whether it is JR-stamped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct SinkOptions {
    /// Whether the sink is sent JR Timestamps.
    pub stamping: Stamping,
}

impl SinkOptions {
    /// The options of a sink opened with none: unstamped.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stamping: Stamping::Off,
        }
    }

    /// These options with `stamping`.
    #[must_use]
    pub const fn stamping(mut self, stamping: Stamping) -> Self {
        self.stamping = stamping;
        self
    }
}

// --- The shared record ---

/// What the session writes and the handle reads of one sink.
struct SinkShared {
    id: ResourceId,
    info: Arc<MidiSinkInfo>,
    /// What its node's tap had no room for, and what the sink refused or was
    /// scheduled too far ahead to send.
    losses: LossReaders,
}

/// What the handles of a sink share: its record and its node. The session
/// watches it by a weak reference, so the sink closes with the last clone of
/// its [`MidiSink`].
#[derive(Debug)]
struct SinkClaim {
    shared: Arc<SinkShared>,
    node: NodeKey,
}

impl std::fmt::Debug for SinkShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SinkShared")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// What a [`MidiSink`] says of itself: one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MidiSinkInfo {
    /// What it was opened with.
    pub options: SinkOptions,
}

// --- The handle ---

/// A host's sink: [`Session::open_midi_sink`].
///
/// `Clone` and `Send`. Clones share the sink, and it closes when the last clone
/// is dropped.
#[derive(Clone, Debug)]
pub struct MidiSink {
    claim: Arc<SinkClaim>,
}

impl MidiSink {
    /// The sink's output node: the node to wire to. What is wired to its event
    /// input (`Graph::set_sources`) is sent to the sink each update, beside the
    /// session's MIDI clock, which feeds it as a feed of its own
    /// ([`Feeder::Clock`](crate::Feeder::Clock)) that a host's `set_sources` does
    /// not replace. The node is in the graph while a handle of the sink lives.
    #[must_use]
    pub fn node(&self) -> NodeKey {
        self.claim.node
    }

    /// What this sink did not get, cumulative: [`lost`](Losses::lost) are events
    /// its node's tap had no room for (1 024 between two updates), and
    /// [`refused`](Losses::refused) those the sink refused or that were
    /// scheduled more than [`MAX_OUTPUT_AHEAD`](tutti_midi_runtime::MAX_OUTPUT_AHEAD)
    /// ahead. A JR Timestamp the sink refuses is not counted: the event it
    /// stamps is.
    #[must_use]
    pub fn losses(&self) -> Losses {
        self.claim.shared.losses.get()
    }
}

impl crate::peer::sealed::Sealed for MidiSink {}

impl Resource for MidiSink {
    fn id(&self) -> ResourceId {
        self.claim.shared.id
    }
}

impl Peer for MidiSink {
    type Info = MidiSinkInfo;
    /// A sink is never lost: only its last handle's drop closes it.
    type Cause = Infallible;

    fn info(&self) -> Arc<MidiSinkInfo> {
        Arc::clone(&self.claim.shared.info)
    }

    fn presence(&self) -> Presence<Infallible> {
        Presence::Live
    }
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// Opens `sink` as an output, unstamped: wire what it should receive to
    /// its [`node`](MidiSink::node). It closes when every clone of the handle
    /// is dropped.
    pub fn open_midi_sink(&mut self, sink: Arc<dyn MidiOut>) -> MidiSink {
        self.open_midi_sink_with(sink, SinkOptions::new())
    }

    /// Opens `sink` with `options`.
    pub fn open_midi_sink_with(
        &mut self,
        sink: Arc<dyn MidiOut>,
        options: SinkOptions,
    ) -> MidiSink {
        let (mut midi, ids) = self.midi_and_ids();
        let id = ids.mint();
        midi.open_sink(id, sink, options)
    }
}

// --- The session's table of sinks ---

/// One sink as the session keeps it.
struct SinkPort {
    /// Upgrades while a handle of the sink lives.
    claim: Weak<SinkClaim>,
    node: NodeKey,
    tap: EventTapReader,
    out: UmpOut,
    losses: LossWriters,
}

/// The sinks of the session.
#[derive(Default)]
pub(crate) struct Sinks {
    ports: Vec<SinkPort>,
}

impl<F: tutti_buffers::Float> crate::Midi<'_, F> {
    /// Opens the sink `sink` as `id`: its output node joins the graph, fed by
    /// the session's clock.
    pub(crate) fn open_sink(
        &mut self,
        id: ResourceId,
        sink: Arc<dyn MidiOut>,
        options: SinkOptions,
    ) -> MidiSink {
        let rate = self.graph.prepared().sample_rate();
        let (node, tap) = self.add_output_node();
        let mut out = UmpOut::new(sink, rate);
        out.set_stamping(options.stamping);
        let (losses, readers) = loss_cells();
        let shared = Arc::new(SinkShared {
            id,
            info: Arc::new(MidiSinkInfo { options }),
            losses: readers,
        });
        let claim = Arc::new(SinkClaim { shared, node });
        self.state.sinks.ports.push(SinkPort {
            claim: Arc::downgrade(&claim),
            node,
            tap,
            out,
            losses,
        });
        MidiSink { claim }
    }
}

impl Sinks {
    /// Each sink's tap on one scale of frames, the rate it was opened at: where
    /// the graph has been re-prepared at another, drains the old reader and
    /// reopens that one tap. Whether any reopened; the graph is then edited and
    /// the next commit compiles the new tap.
    pub(crate) fn reopen<F: tutti_buffers::Float>(
        &mut self,
        graph: &mut crate::graph::Graph<F>,
        timing: Option<&OutputTiming>,
    ) -> bool {
        let mut edited = false;
        for port in &mut self.ports {
            if port.tap.rate() != graph.prepared().sample_rate() {
                port.drain(timing);
                if let Some(tap) = graph.taps().reopen_event(port.tap.id()) {
                    port.tap = tap;
                    port.out.set_rate(graph.prepared().sample_rate());
                    edited = true;
                }
            }
        }
        edited
    }

    /// Each sink's tap, read dry and sent to its sink, and the sinks whose
    /// handles dropped taken out of the graph after what their taps held was sent.
    /// Whether the graph was edited.
    pub(crate) fn drain<F: tutti_buffers::Float>(
        &mut self,
        graph: &mut crate::graph::Graph<F>,
        timing: Option<&OutputTiming>,
    ) -> bool {
        let mut edited = false;
        self.ports.retain_mut(|port| {
            port.drain(timing);
            let held = port.claim.strong_count() > 0;
            if !held {
                // A host that removed the node itself leaves nothing to take.
                let _ = graph.remove(port.node);
                edited = true;
            }
            held
        });
        edited
    }
}

impl SinkPort {
    /// Reads the tap dry to the sink, and counts what did not go.
    fn drain(&mut self, timing: Option<&OutputTiming>) {
        drain_events(&mut self.tap, Some(&mut self.out), &mut self.losses, timing);
    }
}
