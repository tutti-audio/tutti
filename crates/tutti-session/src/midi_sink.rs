//! A host's own MIDI sink as a pool entry (feature `midi`): [`midi_sink`]
//! declares a [`MidiOut`] the app implements, and [`Session::midi_sink`] reads
//! it back as a [`MidiSink`].
//!
//! A sink is an output as a `midi_output` port is: a
//! [`MidiOutNode`](tutti_midi_runtime::MidiOutNode) of its own, fed by the
//! session's clock, whose events the session sends to the sink each update at
//! the time its frame has on the OS clock (the same drain, the same timing, as a
//! device's). A sink is there for as long as its entry is declared: it needs no
//! device, so it reads [`Presence::Live`] until the entry is released or
//! swapped.

use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use tutti_graph::{EventTapReader, Losses, NodeKey};
use tutti_midi_runtime::MidiOut;

use crate::midi::{OutDrops, OutputTiming, UmpOut, drain_events, route_batch};
use crate::pool::{Declared, HandleError, Life, Load, PoolEntry};
use crate::{Peer, Presence, Resource, ResourceId, Session};

// --- The entry ---

/// A host's own sink: [`midi_sink`]'s entry. Its identity is the sink itself (the
/// one `Arc`) and whether it is JR-stamped.
#[derive(Clone)]
pub struct MidiSinkEntry {
    sink: Arc<dyn MidiOut>,
    stamping: bool,
}

impl std::fmt::Debug for MidiSinkEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiSinkEntry")
            .field("stamping", &self.stamping)
            .finish_non_exhaustive()
    }
}

/// Declares `sink` under `key` as an output: wire what it should receive to
/// its [`node`](MidiSink::node). It is sent unstamped; `.stamping(true)` on the
/// declared entry sets JR Timestamps on.
///
/// Declaring the key again with a clone of the same `Arc` keeps the entry; a
/// different sink loads another.
pub fn midi_sink<K>(key: K, sink: Arc<dyn MidiOut>) -> Declared<K, MidiSinkEntry> {
    Declared::new(
        key,
        MidiSinkEntry {
            sink,
            stamping: false,
        },
    )
}

impl<K> Declared<K, MidiSinkEntry> {
    /// This sink, JR-stamped or not: it is part of its identity. A stamped sink
    /// is sent each event with the JR Timestamp that keeps its spacing, and the
    /// JR Clock (M2-104-UM §7.2.2.1). It gates timestamps, not delivery.
    pub fn stamping(mut self, stamping: bool) -> Self {
        self.entry_mut().stamping = stamping;
        self
    }
}

impl PoolEntry for MidiSinkEntry {
    /// The sink's address, and whether it stamps.
    type Identity = (usize, bool);
    type Loaded = SinkLoaded;
    type Handle = MidiSink;

    fn identity(&self) -> (usize, bool) {
        (Arc::as_ptr(&self.sink).addr(), self.stamping)
    }

    fn load(&self) -> Load<SinkLoaded> {
        let sink = Arc::clone(&self.sink);
        let stamping = self.stamping;
        Load::on_session(move |cx| {
            let id = cx.mint_resource_id();
            Ok(cx.midi().open_sink(id, sink, stamping))
        })
    }

    fn handle(loaded: &SinkLoaded, life: &Life) -> MidiSink {
        MidiSink {
            shared: Arc::clone(&loaded.shared),
            node: loaded.node,
            life: life.clone(),
        }
    }
}

// --- The shared record ---

/// What the session writes and the handle reads of one sink.
struct SinkShared {
    id: ResourceId,
    info: Arc<MidiSinkInfo>,
    /// Events its node's tap had no room for.
    lost: AtomicU64,
    /// Events the sink refused, or that were scheduled too far ahead to send.
    refused: AtomicU64,
}

/// What the pool holds of a loaded sink. Read it through the [`MidiSink`]
/// handle.
#[derive(Debug)]
pub struct SinkLoaded {
    shared: Arc<SinkShared>,
    node: NodeKey,
    /// Dropped with the loaded value: the entry was released or swapped.
    _token: Arc<()>,
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
    /// Whether its events are sent with JR Timestamps.
    pub stamping: bool,
}

// --- The handle ---

/// A host's sink, read by key: [`Session::midi_sink`].
///
/// `Clone` and `Send`, and it owns nothing: dropping it ends nothing, and the
/// sink ends when the pool no longer names its entry, after which it reads
/// [`Presence::Closed`].
#[derive(Clone, Debug)]
pub struct MidiSink {
    shared: Arc<SinkShared>,
    node: NodeKey,
    life: Life,
}

impl MidiSink {
    /// The sink's output node: the node to wire to. What is wired to its event
    /// input (`Graph::set_sources`) is sent to the sink each update, beside the
    /// session's MIDI clock, which feeds it as a feed of its own
    /// ([`Feeder::Clock`](crate::Feeder::Clock)) that a host's `set_sources` does
    /// not replace. The node is in the graph while the entry is loaded.
    #[must_use]
    pub const fn node(&self) -> NodeKey {
        self.node
    }

    /// What this sink did not get, cumulative: [`lost`](Losses::lost) are events
    /// its node's tap had no room for (1 024 between two updates), and
    /// [`refused`](Losses::refused) those the sink refused or that were
    /// scheduled more than [`MAX_OUTPUT_AHEAD`](tutti_midi_runtime::MAX_OUTPUT_AHEAD)
    /// ahead. A JR Timestamp the sink refuses is not counted: the event it
    /// stamps is.
    #[must_use]
    pub fn losses(&self) -> Losses {
        Losses::lost(self.shared.lost.load(Ordering::Relaxed))
            + Losses::refused(self.shared.refused.load(Ordering::Relaxed))
    }
}

impl crate::peer::sealed::Sealed for MidiSink {}

impl Resource for MidiSink {
    fn id(&self) -> ResourceId {
        self.shared.id
    }
}

impl Peer for MidiSink {
    type Info = MidiSinkInfo;
    /// A sink is never lost: only its entry's end closes it.
    type Cause = Infallible;

    fn info(&self) -> Arc<MidiSinkInfo> {
        Arc::clone(&self.shared.info)
    }

    fn presence(&self) -> Presence<Infallible> {
        self.life.presence(|| Presence::Live)
    }
}

impl<C: crate::Clock> Session<C> {
    /// The sink of the `midi_sink` entry under `key`.
    ///
    /// # Errors
    ///
    /// [`HandleError::NotPooled`] when the key is not declared (never, or
    /// released); [`HandleError::WrongKind`] when it is another kind of entry;
    /// [`HandleError::NotLoaded`] before the load lands.
    pub fn midi_sink<K>(&self, key: &K) -> Result<MidiSink, HandleError<K>>
    where
        K: Ord + Clone + std::fmt::Debug + Send + Sync + 'static,
    {
        self.pool.handle::<K, MidiSinkEntry>(key)
    }
}

// --- The session's table of sinks ---

/// One sink as the session keeps it.
struct SinkPort {
    /// Upgrades while the entry's loaded value lives.
    token: Weak<()>,
    shared: Arc<SinkShared>,
    node: NodeKey,
    tap: EventTapReader,
    out: UmpOut,
}

/// The sinks of the session.
#[derive(Default)]
pub(crate) struct Sinks {
    ports: Vec<SinkPort>,
}

impl crate::Midi<'_> {
    /// Opens the sink `sink` as `id`: its output node joins the graph, fed by
    /// the session's clock.
    pub(crate) fn open_sink(
        &mut self,
        id: ResourceId,
        sink: Arc<dyn MidiOut>,
        stamping: bool,
    ) -> SinkLoaded {
        let rate = self.graph.prepared().sample_rate();
        let (node, tap) = self.add_output_node();
        let mut out = UmpOut::new(sink, rate);
        out.set_stamping(stamping);
        let shared = Arc::new(SinkShared {
            id,
            info: Arc::new(MidiSinkInfo { stamping }),
            lost: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        });
        let token = Arc::new(());
        self.state.sinks.ports.push(SinkPort {
            token: Arc::downgrade(&token),
            shared: Arc::clone(&shared),
            node,
            tap,
            out,
        });
        SinkLoaded {
            shared,
            node,
            _token: token,
        }
    }
}

impl Sinks {
    /// Each sink's tap on one scale of frames, the rate it was opened at: where
    /// the graph has been re-prepared at another, drains the old reader and
    /// reopens that one tap. Whether any reopened; the graph is then edited and
    /// the next commit compiles the new tap.
    pub(crate) fn reopen(
        &mut self,
        graph: &mut crate::graph::Graph,
        drops: &OutDrops,
        timing: Option<&OutputTiming>,
    ) -> bool {
        let mut edited = false;
        for port in &mut self.ports {
            if port.tap.rate() != graph.prepared().sample_rate() {
                port.drain(drops, timing);
                if let Some(tap) = graph.taps().reopen_event(port.tap.id()) {
                    port.tap = tap;
                    edited = true;
                }
            }
        }
        edited
    }

    /// Each sink's tap, read dry and sent to its sink, and the sinks whose
    /// entry ended taken out of the graph after what their taps held was sent.
    /// Whether the graph was edited.
    pub(crate) fn drain(
        &mut self,
        graph: &mut crate::graph::Graph,
        drops: &OutDrops,
        timing: Option<&OutputTiming>,
    ) -> bool {
        let mut edited = false;
        self.ports.retain_mut(|port| {
            port.drain(drops, timing);
            let held = port.token.strong_count() > 0;
            if !held {
                // A host that removed the node itself leaves nothing to take.
                let _ = graph.remove(port.node);
                edited = true;
            }
            held
        });
        edited
    }

    /// Sends `batch` to every sink, each event on its frame. Whether there is
    /// one.
    pub(crate) fn route(
        &mut self,
        batch: &[(tutti_types::Frame, tutti_ump::Ump)],
        drops: &OutDrops,
        timing: Option<&OutputTiming>,
    ) -> bool {
        for port in &mut self.ports {
            let refused = route_batch(Some(&mut port.out), drops, timing, batch);
            port.shared.refused.fetch_add(refused, Ordering::Relaxed);
        }
        !self.ports.is_empty()
    }
}

impl SinkPort {
    /// Reads the tap dry to the sink, and counts what did not go.
    fn drain(&mut self, drops: &OutDrops, timing: Option<&OutputTiming>) {
        let losses = drain_events(&mut self.tap, Some(&mut self.out), drops, timing);
        self.shared.lost.fetch_add(losses.lost, Ordering::Relaxed);
        self.shared
            .refused
            .fetch_add(losses.refused, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
