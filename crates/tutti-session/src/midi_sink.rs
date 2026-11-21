//! A host's own MIDI sink (feature `midi`): [`Session::open_midi_sink`] opens
//! a [`MidiOut`] the app implements and returns a [`MidiSink`].
//!
//! A sink is an output as a `MidiOutput` port is: a
//! [`MidiOutNode`](tutti_midi_runtime::MidiOutNode) of its own, fed by the
//! session's clock, whose events the session sends to the sink each update at
//! the time its frame has on the OS clock (the same drain, the same timing, as a
//! device's). A sink needs no device, so it is always there while a clone of its
//! handle lives. The last clone's drop retires it: what was queued to its node
//! before the drop is sent, and the sink ends and its node leaves the graph at
//! the first [`update`](Session::update) after the graph has rendered past the
//! drop.

use std::sync::Arc;

use tutti_graph::{EventTapReader, Losses, NodeKey};
use tutti_midi_runtime::{DeviceGeneration, MidiOut};

use crate::Session;
use crate::midi::{
    Claim, Departure, Held, Leaving, LossReaders, LossWriters, OutputTiming, Retiring, Stamping,
    Sweepable, UmpOut, drain_events, loss_cells, sweep_dropped,
};

// --- The shared record ---

/// What the session writes and the handle reads of one sink.
#[derive(Debug)]
pub(crate) struct SinkShared {
    /// What its node's tap had no room for, and what the sink refused or was
    /// scheduled too far ahead to send.
    losses: LossReaders,
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
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// A request to open `sink` as an output: wire what it should receive to
    /// its [`node`](MidiSink::node). Nothing opens until
    /// [`run`](OpenMidiSink::run) is called, and the sink
    /// ends when every clone of the handle is dropped, once the graph has
    /// rendered past the drop ([`update`](Self::update) ends it).
    pub fn open_midi_sink(&mut self, sink: Arc<dyn MidiOut>) -> OpenMidiSink<'_, K, C> {
        OpenMidiSink {
            session: self,
            sink,
            stamping: Stamping::Off,
        }
    }
}

/// A sink to open, built by [`Session::open_midi_sink`]: it borrows the session
/// until [`run`](Self::run) opens the sink.
#[must_use = "the sink opens when the request runs"]
pub struct OpenMidiSink<'s, K: crate::AppKey, C: crate::Clock> {
    session: &'s mut Session<K, C>,
    sink: Arc<dyn MidiOut>,
    stamping: Stamping,
}

impl<K: crate::AppKey, C: crate::Clock> std::fmt::Debug for OpenMidiSink<'_, K, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenMidiSink")
            .field("stamping", &self.stamping)
            .finish_non_exhaustive()
    }
}

impl<K: crate::AppKey, C: crate::Clock> OpenMidiSink<'_, K, C> {
    /// Whether the sink is sent JR Timestamps. It gates timestamps, not
    /// delivery: with it off (the default) the events still go out, unstamped.
    pub const fn stamping(mut self, stamping: Stamping) -> Self {
        self.stamping = stamping;
        self
    }

    /// Opens the sink: its output node joins the graph (the next
    /// [`update`](Session::update) commits it).
    pub fn run(self) -> MidiSink {
        self.session.midi().open_sink(self.sink, self.stamping)
    }
}

// --- The session's table of sinks ---

/// One sink as the session keeps it.
struct SinkPort {
    claim: Held<SinkShared>,
    node: NodeKey,
    tap: EventTapReader,
    departure: Departure,
    out: UmpOut,
    losses: LossWriters,
}

/// The sinks of the session, and those going.
#[derive(Default)]
pub(crate) struct Sinks {
    ports: Vec<SinkPort>,
    retiring: Vec<Retiring<SinkPort>>,
}

impl<F: tutti_buffers::Float> crate::Midi<'_, F> {
    /// Opens the sink `sink`: its output node joins the graph, fed by the
    /// session's clock.
    pub(crate) fn open_sink(&mut self, sink: Arc<dyn MidiOut>, stamping: Stamping) -> MidiSink {
        self.close_dropped();
        let rate = self.graph.prepared().sample_rate();
        let (node, tap, departure) = self.add_output_node();
        let mut out = UmpOut::new(sink, rate);
        out.set_stamping(stamping);
        let (losses, readers) = loss_cells();
        let shared = Arc::new(SinkShared { losses: readers });
        let (claim, held) = Held::new(Arc::clone(&shared), node);
        self.state.sinks.ports.push(SinkPort {
            claim: held,
            node,
            tap,
            departure,
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
        let going = self.retiring.iter_mut().map(|going| &mut going.port);
        for port in self.ports.iter_mut().chain(going) {
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

    /// The sinks whose handles were all dropped, retired: each is sent what its
    /// node rendered, and ends once the graph has rendered past the drop and its
    /// tap was drained, with its node taken out. Whether the graph was edited.
    pub(crate) fn close_dropped<F: tutti_buffers::Float>(
        &mut self,
        graph: &mut crate::graph::Graph<F>,
        timing: Option<&OutputTiming>,
    ) -> bool {
        sweep_dropped(&mut self.ports, &mut self.retiring, graph, timing)
    }
}

impl Sweepable for SinkPort {
    fn is_held(&self) -> bool {
        self.claim.is_held()
    }

    fn depart(&mut self) -> DeviceGeneration {
        self.departure.depart()
    }
}

impl Leaving for SinkPort {
    fn node(&self) -> NodeKey {
        self.node
    }

    fn drain(&mut self, timing: Option<&OutputTiming>) {
        SinkPort::drain(self, timing);
    }
}

impl SinkPort {
    /// Reads the tap dry to the sink, and counts what did not go.
    fn drain(&mut self, timing: Option<&OutputTiming>) {
        drain_events(&mut self.tap, Some(&mut self.out), &mut self.losses, timing);
    }
}
