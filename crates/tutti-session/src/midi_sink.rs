//! A host's own MIDI sink (feature `midi`): [`Session::open_midi_sink`] opens
//! a [`MidiOut`] the app implements and returns a [`MidiSink`].
//!
//! A sink is an output as a `MidiOutput` port is: a
//! [`MidiOutNode`](tutti_midi_runtime::MidiOutNode) of its own, fed by the
//! session's clock, whose events the session sends to the sink each update at
//! the time its frame has on the OS clock (the same drain, the same timing, as a
//! device's). A sink needs no device, so it is always there while a clone of its
//! handle lives. The last clone's drop closes it: its node leaves the graph at
//! the next [`update`](Session::update), or at the next open, whichever comes
//! first.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tutti_graph::{EventTapReader, Losses, NodeKey};
use tutti_midi_runtime::MidiOut;

use crate::Session;
use crate::midi::{
    Claim, Held, LossReaders, LossWriters, OutputTiming, Stamping, UmpOut, drain_events, loss_cells,
};

// --- The shared record ---

/// What the session reads and the handle writes and reads of one sink.
#[derive(Debug)]
pub(crate) struct SinkShared {
    /// Whether the sink is sent JR Timestamps.
    jr: AtomicBool,
    /// What its node's tap had no room for, and what the sink refused or was
    /// scheduled too far ahead to send.
    losses: LossReaders,
}

impl SinkShared {
    fn stamping(&self) -> Stamping {
        if self.jr.load(Ordering::Acquire) {
            Stamping::Jr
        } else {
            Stamping::Off
        }
    }
}

// --- The handle ---

/// A host's sink: [`Session::open_midi_sink`].
///
/// `Clone` and `Send`. Clones share the sink, and it closes when the last clone
/// is dropped.
#[derive(Clone, Debug)]
pub struct MidiSink {
    claim: Arc<Claim<SinkShared>>,
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

    /// Sets whether the sink is sent JR Timestamps. It gates timestamps, not
    /// delivery: with it off the events still go out, unstamped. A sink opens
    /// unstamped, and a change applies from the next drain.
    pub fn set_stamping(&self, stamping: Stamping) {
        self.claim
            .shared
            .jr
            .store(stamping == Stamping::Jr, Ordering::Release);
    }

    /// Whether the sink is sent JR Timestamps.
    #[must_use]
    pub fn stamping(&self) -> Stamping {
        self.claim.shared.stamping()
    }
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// Opens `sink` as an output, unstamped ([`MidiSink::set_stamping`]): wire
    /// what it should receive to its [`node`](MidiSink::node). It closes when
    /// every clone of the handle is dropped, at the next
    /// [`update`](Self::update) or open.
    pub fn open_midi_sink(&mut self, sink: Arc<dyn MidiOut>) -> MidiSink {
        self.midi().open_sink(sink)
    }
}

// --- The session's table of sinks ---

/// One sink as the session keeps it.
struct SinkPort {
    claim: Held<SinkShared>,
    shared: Arc<SinkShared>,
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
    /// Opens the sink `sink`: its output node joins the graph, fed by the
    /// session's clock.
    pub(crate) fn open_sink(&mut self, sink: Arc<dyn MidiOut>) -> MidiSink {
        self.close_dropped();
        let rate = self.graph.prepared().sample_rate();
        let (node, tap) = self.add_output_node();
        let out = UmpOut::new(sink, rate);
        let (losses, readers) = loss_cells();
        let shared = Arc::new(SinkShared {
            jr: AtomicBool::new(false),
            losses: readers,
        });
        let (claim, held) = Held::new(Arc::clone(&shared), node);
        self.state.sinks.ports.push(SinkPort {
            claim: held,
            shared,
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

    /// Each held sink's tap, read dry and sent to its sink, and the sinks
    /// whose handles were all dropped closed. Whether the graph was edited.
    pub(crate) fn drain<F: tutti_buffers::Float>(
        &mut self,
        graph: &mut crate::graph::Graph<F>,
        timing: Option<&OutputTiming>,
    ) -> bool {
        for port in self.ports.iter_mut().filter(|port| port.claim.is_held()) {
            port.drain(timing);
        }
        self.close_dropped(graph, timing)
    }

    /// The sinks whose handles were all dropped, closed: what their taps held
    /// is sent, and the nodes leave the graph. Whether the graph was edited.
    pub(crate) fn close_dropped<F: tutti_buffers::Float>(
        &mut self,
        graph: &mut crate::graph::Graph<F>,
        timing: Option<&OutputTiming>,
    ) -> bool {
        let mut edited = false;
        self.ports.retain_mut(|port| {
            let held = port.claim.is_held();
            if !held {
                port.drain(timing);
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
    /// Reads the tap dry to the sink with the stamping the handles set, and
    /// counts what did not go.
    fn drain(&mut self, timing: Option<&OutputTiming>) {
        self.out.set_stamping(self.shared.stamping());
        drain_events(&mut self.tap, Some(&mut self.out), &mut self.losses, timing);
    }
}
