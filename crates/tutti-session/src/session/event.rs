//! [`SessionEvent`], what an [`update`](crate::Session::update) found for the host.

use tutti_buffers::ChannelTopology;
use tutti_engine::StreamFault;
use tutti_graph::{BusSource, CommitError, NodeKey, Wired};
use tutti_units::SampleRate;

use crate::{BusSink, ChannelEdge, EventEdge, MainOutputReader, Problem, ReplaceRefused};

/// Something an [`update`](crate::Session::update) found for the host.
#[derive(Debug)]
#[non_exhaustive]
pub enum SessionEvent {
    /// The graph's edits could not be committed: the plan sent last keeps
    /// playing. Each update tries again; a refusal is reported once, until a
    /// commit is sent or it fails another way.
    CommitRefused(CommitError),
    /// A [`replace`](crate::Graph::replace) that waited out a re-prepare
    /// swapped its node in: the key the call returned reaches the incoming
    /// node's values. Sent only for a swap that was deferred; one made at the
    /// call sends nothing.
    Replaced {
        /// The node it swapped behind.
        node: NodeKey,
    },
    /// A replacement that waited out a re-prepare was refused when it was
    /// retried (the graph is poisoned): the incoming node was dropped.
    ReplaceFailed {
        /// The node it was to swap behind.
        node: NodeKey,
        /// Why.
        refused: ReplaceRefused,
    },
    /// A node was removed while a [`replace`](crate::Graph::replace) of it
    /// waited out a re-prepare: the waiting replacement was dropped with its
    /// incoming node, one event for each.
    ReplaceDropped {
        /// The node that was removed.
        node: NodeKey,
    },
    /// A node re-inserted with a narrower shape (a plugin whose ports changed,
    /// a node re-declared at a new width) lost wiring to the ports it no
    /// longer has. Reported once, by the update that commits the re-insert,
    /// per node: the audio edges, the event edges and the bus edges (main output feeds
    /// included) that were dropped, by the ports the app named them with.
    /// Edges to the ports the node still has are kept, gains included. A
    /// refused commit reports nothing: the record waits for the update whose
    /// commit lands.
    ///
    /// A feedback edge is listed as the pair of ports it joined (its delay
    /// is not part of a [`ChannelEdge`]). A modulation route onto a
    /// dropped edge, or a param or edge-gain lane the node's lost event
    /// outputs drove, is not listed: the route stays declared and reads
    /// unresolved (`Problem::Unresolved`) until its target
    /// is wired again.
    Unwired {
        /// The node whose re-insert dropped the wiring.
        node: NodeKey,
        /// The audio edges dropped, into or out of the node (a feedback edge
        /// as the pair of ports it joined).
        edges: Vec<ChannelEdge>,
        /// The event edges dropped, into or out of the node.
        event_edges: Vec<EventEdge>,
        /// The bus edges dropped, into or out of the node, as the app wired
        /// them: the source bus and the sink, a node's input bus or the
        /// main output.
        bus_edges: Vec<(BusSource, BusSink)>,
    },
    /// A node with a bus besides its main one (an aux or sidechain bus, a
    /// plugin's) had its audio wiring changed: a port gained its first source or
    /// consumer, or lost its last (an edge, a global output, a tap or a
    /// meter). Reported once per node by the update that sends the commit
    /// (or the next update, for a commit the host made itself), carrying the
    /// node's first `was` and last `now` of the commits since the last
    /// report; a node whose wiring is as it was reports nothing. It is the
    /// control-side notice a plugin needs to activate or deactivate an aux
    /// bus; the block that applies the commit reads the same answer through
    /// [`Io::wired_inputs`](tutti_graph::Io::wired_inputs). A node with only
    /// a main bus is never reported; it reads its per-block masks.
    Rewired {
        /// The node.
        node: NodeKey,
        /// Its wired ports before.
        was: Wired,
        /// Its wired ports now.
        now: Wired,
    },
    /// A node's event output port lost events since the last update: they
    /// were past its capacity (a note-off only past the room set aside for
    /// note-offs), a PDC or feedback delay it feeds was full, or the node
    /// left out an event because the one it pairs with was refused (a clip's
    /// note-off of a note-on the port refused). One per port per update, the
    /// sum over the blocks of the update. A note-off refused and sent again
    /// later counts as refused.
    ///
    /// `port` is `None` for a loss no output port owns. The graph's total is
    /// [`BlockCounts::dropped_events`](tutti_engine::BlockCounts::dropped_events), exact
    /// where this is per port: a block that loses at more than 64 places, or
    /// a host that does not run `Collect` for long enough to fill a ring of
    /// 256 entries, leaves some out here and not there.
    EventsDropped {
        /// The node.
        node: NodeKey,
        /// Its event output port, `None` for a loss the node had that no
        /// output port owns: events a plugin took in and did not deliver, a
        /// scheduled command that did not fit.
        port: Option<u16>,
        /// How many events were refused or skipped.
        count: u64,
        /// How many of them a node says it then wrote (a note-off it sent
        /// again): `count - recovered` never arrived, summed over updates.
        recovered: u64,
    },
    /// Events the graph lost that no [`EventsDropped`](Self::EventsDropped)
    /// names: a merge's, and those a block that lost at more than 64 places
    /// or a full report ring left out. The count since the last update.
    EventsDroppedElsewhere(u64),
    /// The engine rendered silence for blocks it could not render: an
    /// **audible dropout** (the listener hears a gap, and a heard tap or
    /// recording takes the silence too). The count is the blocks silenced
    /// since the last update that reported, one event per update that saw
    /// any; it is derived on the control side from
    /// [`BlockCounts::silenced`](tutti_engine::BlockCounts::silenced), so
    /// the audio thread does nothing more than count.
    ///
    /// A block is silenced when the graph was held by the control side as
    /// the callback came (an edit applying while the device asked for
    /// audio), when the output buffer's width is one the engine has no fold
    /// for ([`Engine::support_output`](tutti_engine::Engine::support_output)),
    /// when the graph has no output channels, or when the graph's main output width
    /// cannot be placed on the device's. Blocks that merely ran past their
    /// budget are not silenced and have no event: that is the polled
    /// [`BlockCounts::over_budget`](tutti_engine::BlockCounts::over_budget).
    BlocksSilenced(u64),
    /// A staged [`OutputPolicy`](crate::OutputPolicy)'s routing was checked
    /// when it was set, and the engine refused it when it came to apply: it has
    /// no place for a main output on the device the stream renders into. The routing
    /// in force is unchanged and the policy's levels are applied; set a routing
    /// that places the main output.
    OutputRoutingRefused(tutti_engine::RoutingError),
    /// The session's rate changed with its clock: a switch onto a sound card
    /// that runs at another rate, or the card's rate carried to the timer. A
    /// capture a host holds itself (not one the session added) is at the old
    /// rate and is the host's to reopen.
    RateChanged {
        /// The rate the graph ran at.
        from: SampleRate,
        /// The rate it runs at now.
        to: SampleRate,
    },
    /// The stream of a real-time clock reported a fault: a device unplugged
    /// mid-session, a callback that panicked, a timer that fell behind. The
    /// most recent one since the last poll, as
    /// [`crate::Session::faults`] counts them.
    Fault(StreamFault),
    /// The sound card or its devices: a device that came or went, a
    /// restart wanted, done or failed ([`DeviceEvent`](crate::DeviceEvent)).
    #[cfg(feature = "device")]
    Device(crate::DeviceEvent),
    /// A take of the session's: its first frame arrived, it ended, or it
    /// failed ([`TakeEvent`](crate::TakeEvent)).
    #[cfg(feature = "capture")]
    Take(crate::TakeEvent),
    /// A modulation problem (design 015 §4.7), the one type
    /// [`ModState::problems`](crate::ModState::problems) lists. Reported
    /// once: a refusal when it starts or changes (the problems list holds it
    /// while it lasts), a target that has stayed unresolved for the session's
    /// warning time ([`SessionBuilder::unresolved_warning`](crate::SessionBuilder::unresolved_warning),
    /// 4 s by default) of [`crate::PhaseCx::now`], likely a key no node is ever bound to,
    /// once per stretch it stays unresolved.
    Modulation(Problem),
    /// A MIDI event of the session's, by area (design 030, convention 1):
    /// a device came or went, a held input or the output was lost, a
    /// negotiation settled, the device watch ended, or events were dropped.
    /// See [`MidiEvent`](crate::MidiEvent).
    #[cfg(feature = "midi")]
    Midi(crate::MidiEvent),
    /// A hosted plugin's event, by area: what the plugin reported (its own edit of a parameter, a
    /// new tail) and every transition of its life (died, reloading, recovered,
    /// gave up). See [`PluginEvent`](crate::PluginEvent).
    #[cfg(feature = "plugin")]
    Plugin(crate::PluginEvent),
    /// A reading of one of the session's peers changed. See
    /// [`PeerEvent`](crate::PeerEvent).
    Peer(crate::PeerEvent),
    /// A reader of the main output was opened again, because the main output's width is no
    /// longer the one the reader has: a tap from
    /// [`Taps::open_main_output`](crate::Taps::open_main_output), or the main output meter
    /// once a host took it ([`crate::Session::take_main_output_meter`]). `id` names the new
    /// reader, which the session holds until
    /// [`crate::Session::take_reopened`] claims it (an event left unread loses
    /// nothing); `topology` is the main output's now. The old reader reads closed, so
    /// no reader takes twelve channels for two: a tap's reads what its rings
    /// held when the commit that changed the main output was applied and then ends
    /// ([`Pull::Ended`](tutti_buffers::Pull::Ended)), a meter's reads
    /// [`is_closed`](tutti_graph::MeterReader::is_closed). A second reopen
    /// before the claim replaces the held reader, and the earlier one ends.
    ///
    /// The new tap is fed from the first block of the plan that carries the
    /// change, and what the old one holds ends on the block before it: a main output
    /// crossfade fades the engine's output, never what a tap reads, so the
    /// tap switches to the new main output at the commit and never reads the old
    /// main output's channels. The event comes in the update that sends the commit.
    MainOutputReaderReopened {
        /// The reader to claim.
        id: MainOutputReader,
        /// The main output's topology the reader was opened for.
        topology: ChannelTopology,
    },
}
