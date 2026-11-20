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
use std::sync::{Arc, Weak};

use tutti_graph::{EventTapReader, Losses, NodeKey};
use tutti_midi_runtime::MidiOut;

use crate::midi::{
    LossReaders, LossWriters, OutputTiming, Stamping, UmpOut, drain_events, loss_cells,
};
use crate::pool::{Declared, HandleError, Life, PoolEntry, PoolLoad};
use crate::{Peer, Presence, Resource, ResourceId, Session};

// --- The entry ---

/// What a sink is sent with. Part of the entry's identity: declaring the sink
/// again with other options reopens it.
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

/// A host's own sink: [`midi_sink`]'s entry. Its identity is the sink itself (the
/// one `Arc`) and the options.
#[derive(Clone)]
pub struct MidiSinkEntry {
    sink: Arc<dyn MidiOut>,
    options: SinkOptions,
}

impl std::fmt::Debug for MidiSinkEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiSinkEntry")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

/// Declares `sink` under `key` as an output: wire what it should receive to
/// its [`node`](MidiSink::node). It is opened with no options (`.options(..)`
/// on the declared entry sets them).
///
/// Declaring the key again with a clone of the same `Arc` keeps the entry; a
/// different sink loads another.
pub fn midi_sink<K>(key: K, sink: Arc<dyn MidiOut>) -> Declared<K, MidiSinkEntry> {
    Declared::new(
        key,
        MidiSinkEntry {
            sink,
            options: SinkOptions::new(),
        },
    )
}

impl<K> Declared<K, MidiSinkEntry> {
    /// This sink, opened with `options`: they are part of its identity.
    pub fn options(mut self, options: SinkOptions) -> Self {
        self.entry_mut().options = options;
        self
    }
}

impl<S: tutti_buffers::Float> PoolEntry<S> for MidiSinkEntry {
    /// The sink's address, and the options.
    type Identity = (usize, SinkOptions);
    type Loaded = SinkLoaded;
    type Handle = MidiSink;

    fn identity(&self) -> (usize, SinkOptions) {
        (Arc::as_ptr(&self.sink).addr(), self.options)
    }

    fn load(&self) -> PoolLoad<SinkLoaded, S> {
        let sink = Arc::clone(&self.sink);
        let options = self.options;
        PoolLoad::on_session(move |cx| {
            let id = cx.mint_resource_id();
            Ok(cx.midi().open_sink(id, sink, options))
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
    /// What its node's tap had no room for, and what the sink refused or was
    /// scheduled too far ahead to send.
    losses: LossReaders,
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
    /// What it was opened with.
    pub options: SinkOptions,
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
        self.shared.losses.get()
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

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// The sink of the `midi_sink` entry under `key`.
    ///
    /// # Errors
    ///
    /// [`HandleError::NotPooled`] when the key is not declared (never, or
    /// released); [`HandleError::WrongKind`] when it is another kind of entry;
    /// [`HandleError::NotLoaded`] before the load lands.
    pub fn midi_sink<P>(&self, key: &P) -> Result<MidiSink, HandleError<P>>
    where
        P: crate::AppKey,
    {
        self.pool.entries.handle::<P, MidiSinkEntry>(key)
    }
}

// --- The session's table of sinks ---

/// One sink as the session keeps it.
struct SinkPort {
    /// Upgrades while the entry's loaded value lives.
    token: Weak<()>,
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
    ) -> SinkLoaded {
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
        let token = Arc::new(());
        self.state.sinks.ports.push(SinkPort {
            token: Arc::downgrade(&token),
            node,
            tap,
            out,
            losses,
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
    /// entry ended taken out of the graph after what their taps held was sent.
    /// Whether the graph was edited.
    pub(crate) fn drain<F: tutti_buffers::Float>(
        &mut self,
        graph: &mut crate::graph::Graph<F>,
        timing: Option<&OutputTiming>,
    ) -> bool {
        let mut edited = false;
        self.ports.retain_mut(|port| {
            port.drain(timing);
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
}

impl SinkPort {
    /// Reads the tap dry to the sink, and counts what did not go.
    fn drain(&mut self, timing: Option<&OutputTiming>) {
        drain_events(&mut self.tap, Some(&mut self.out), &mut self.losses, timing);
    }
}
