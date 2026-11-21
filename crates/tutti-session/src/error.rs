//! The session's aggregate error.

use thiserror::Error;

/// Why [`Session::open_heard_tap`](crate::Session::open_heard_tap) or
/// [`Session::close_heard_tap`](crate::Session::close_heard_tap) refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Error)]
#[non_exhaustive]
pub enum SessionHeardTapError {
    /// The session has no engine, so nothing renders to tap (a detached
    /// session; a plan tap, [`Taps::open_ports`](crate::Taps::open_ports), is
    /// fed by its `render_frame`).
    #[error("this session has no engine to tap")]
    Detached,
    /// The engine's tap refused: a reader is alive, or none is open.
    #[error(transparent)]
    Tap(#[from] tutti_engine::OutputTapError),
}

/// Why a declaration was refused: a value declared with a wire or a target
/// ([`Connection::gain`](crate::Connection::gain),
/// [`Mods::range`](crate::Mods::range)). Nothing is declared. The value
/// declared is the declaration's initial one, and later changes to it are
/// writes to its target.
#[derive(Clone, Copy, Debug, PartialEq, Error)]
#[non_exhaustive]
pub enum DeclarationError {
    /// A declared number is NaN or infinite: a weight or a bound must be a
    /// number the lane can multiply by.
    #[error("a declared value must be finite")]
    NotFinite,
    /// A range's start is above its end.
    #[error("the range is backwards: min {min} is above max {max}")]
    Backwards {
        /// The lower bound given.
        min: f64,
        /// The upper bound given.
        max: f64,
    },
}

/// Why [`Graph::remove`](crate::Graph::remove) (or
/// [`Session::remove`](crate::Session::remove)) refused: what was not there.
/// Nothing is changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RemoveError {
    /// The node is not in the graph: removed already, or never added.
    #[error("{0:?} is not in the graph")]
    NoSuchNode(tutti_graph::NodeKey),
}

/// The session refused to set the project tempo. Nothing is published.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Error)]
#[non_exhaustive]
pub enum TempoError {
    /// The session follows a live tempo (`Midi::follow_tempo`, feature
    /// `midi`), which owns the published map until `unfollow_tempo`: a map set now would be
    /// overwritten by the next live step, and unfollowing restores the map in
    /// force when following began.
    #[error("the session follows a live tempo; unfollow_tempo first")]
    Following,
}

/// A query or a control named a node the graph does not have: removed already,
/// or never added. Nothing is changed or read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("{0:?} is not in the graph")]
pub struct NoSuchNode(tutti_graph::NodeKey);

impl NoSuchNode {
    pub(crate) fn new(node: tutti_graph::NodeKey) -> Self {
        Self(node)
    }

    /// The key no node has.
    pub fn node(self) -> tutti_graph::NodeKey {
        self.0
    }
}

impl<E: Into<tutti_timeline::TransportCommand> + std::fmt::Debug>
    From<tutti_timeline::TimedError<E>> for Error
{
    fn from(error: tutti_timeline::TimedError<E>) -> Self {
        Self::Timed(error.map(Into::into))
    }
}

/// The disk streamer's own failure (its thread would not start), as the
/// session's sampler error.
#[cfg(feature = "sampler")]
impl From<tutti_sampler::Error> for Error {
    fn from(error: tutti_sampler::Error) -> Self {
        Self::Sampler(crate::SamplerError::Streamer(error))
    }
}

impl From<NoSuchNode> for ConnectError {
    fn from(e: NoSuchNode) -> Self {
        Self::NoSuchNode(e.node())
    }
}

/// Why [`Graph::add_inlet`](crate::Graph::add_inlet) refused.
#[derive(Clone, Copy, Debug, PartialEq, Error)]
#[non_exhaustive]
pub enum InletError {
    /// The producer runs at another rate than the graph. An inlet does not
    /// resample, so the signal would play at the wrong speed (and drift
    /// besides); open the producer at the graph's rate.
    #[error(
        "the inlet's producer runs at {producer:?} but the graph at {graph:?}; \
         an inlet does not resample"
    )]
    RateMismatch {
        /// The rate the producer fills the ring at.
        producer: tutti_units::SampleRate,
        /// The graph's rate when the inlet was opened.
        graph: tutti_units::SampleRate,
    },
}

/// Why [`Graph::connect`](crate::Graph::connect),
/// [`Graph::set_sources`](crate::Graph::set_sources) or
/// [`Graph::disconnect`](crate::Graph::disconnect) refused; nothing is stored.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// A node is not in the graph.
    #[error("{0:?} is not in the graph")]
    NoSuchNode(tutti_graph::NodeKey),
    /// An output port that the node (or the graph's input) does not have.
    #[error("no such output port: {0:?}")]
    NoSuchOutPort(crate::OutPort),
    /// An input port that the node (or the graph's output) does not have.
    #[error("no such input port: {0:?}")]
    NoSuchInPort(crate::InPort),
    /// An event output that the node does not have.
    #[error("no such event output: {0:?}")]
    NoSuchEventOut(crate::EventOutPort),
    /// An event input that the node does not have.
    #[error("no such event input: {0:?}")]
    NoSuchEventIn(crate::EventInPort),
    /// A node feeds itself: a node cannot be its own source.
    #[error("{0:?} cannot feed itself")]
    Itself(tutti_graph::NodeKey),
    /// The source declares no main output bus, or the sink no main input
    /// bus, to connect channel by channel.
    #[error("{0:?} has no main bus on that side")]
    NoMainBus(tutti_graph::NodeKey),
    /// The two main buses are not the same width. A connection never
    /// truncates or zero-fills: a width change is a bus edge
    /// ([`Graph::connect`](crate::Graph::connect)).
    #[error(
        "{from:?}'s main output is {from_width} wide and {to:?}'s main input {to_width}; \
         a width change is a bus edge"
    )]
    Width {
        /// The source node.
        from: tutti_graph::NodeKey,
        /// Its main output's width.
        from_width: usize,
        /// The sink node.
        to: tutti_graph::NodeKey,
        /// Its main input's width.
        to_width: usize,
    },
    /// A main bus is wider than the root it connects to: a connection never
    /// truncates.
    #[error(
        "{from:?}'s main output is {from_width} wide and the root {root_width}; \
         a width change is a bus edge"
    )]
    RootNarrow {
        /// The source node.
        from: tutti_graph::NodeKey,
        /// Its main output's width.
        from_width: usize,
        /// The root's width.
        root_width: usize,
    },
    /// A main bus narrower than the root has no placement on the root's
    /// channels.
    #[error("{from:?}'s main output has no place on the root: {cause}")]
    RootFold {
        /// The source node.
        from: tutti_graph::NodeKey,
        /// Why its topology does not fold onto the root's.
        cause: tutti_buffers::FoldError,
    },
    /// A main bus narrower than the root folds onto it only with weights (a
    /// centre into a root with none): a weighted fold is a bus edge.
    #[error("{from:?}'s main output folds onto the root only with weights; that is a bus edge")]
    RootWeighted {
        /// The source node.
        from: tutti_graph::NodeKey,
    },
    /// A bus edge was refused.
    #[error(transparent)]
    Bus(#[from] tutti_graph::BusError),
}

/// Why a MIDI call of the session failed: one error for the area, each cause
/// carried as its source. `tutti::midi::MidiError` in the umbrella; the session's
/// own [`Error::Midi`] holds it, and every cause below converts into both with
/// `?`.
#[cfg(feature = "midi")]
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum MidiError {
    /// There is no route rule with this id ([`Midi::unroute`](crate::Midi::unroute)):
    /// removed already, or never added.
    #[error("no MIDI route {0:?}")]
    NoSuchRoute(crate::MidiRouteId),

    /// A MIDI call named a target that is not in the graph, or takes no
    /// MIDI (it has no event input): `Midi::play_clip`, `Midi::input`,
    /// `Midi::retarget`, `Midi::tap`.
    #[error("{0:?} takes no MIDI: it is not in the graph, or has no event input")]
    NoEventInput(tutti_graph::NodeKey),

    /// A tap on a MIDI port could not be opened: the port is not in the graph.
    #[error("MIDI tap")]
    Tap(#[source] tutti_graph::MissingSource),

    /// A follower named a node that is not the input node of an input port
    /// (`MidiInput::node`).
    #[error("{0:?} is not the input node of a MIDI input port")]
    NotAnInput(tutti_graph::NodeKey),

    /// `Midi::retarget` named a node that is not a clip of the session's.
    #[error("{0:?} is not a clip of this session's")]
    NotSessionMidi(tutti_graph::NodeKey),

    /// `Midi::input` found the keys of the target's live input handed out
    /// already: they are the inlet's one producer.
    #[error("the live input of {0:?} has been taken")]
    InputTaken(tutti_graph::NodeKey),

    /// `Midi::input` was asked for the live input of an instrument made by
    /// `Session::add_instrument` (or `Session::instrument`): it is played
    /// through its [`InstrumentHandle`](crate::InstrumentHandle)'s keys.
    #[error("{0:?} is an instrument: play it through its handle's keys")]
    InstrumentKeys(tutti_graph::NodeKey),

    /// `Midi::stop_clip` named a target with no clip.
    #[error("{0:?} has no clip")]
    NoClip(tutti_graph::NodeKey),

    /// A clip's events could not be written through its target: the clip's
    /// node was taken out of the graph directly (`Graph::remove`), or the call
    /// was made from inside a `modify` closure.
    #[error("a clip's events could not be written")]
    ClipEvents(#[source] crate::ValueErrorKind),

    /// A beat that is not finite (NaN or infinite) names no frame: a clip's
    /// start, or an event moved by it.
    #[error("a clip beat must be finite, not {0}")]
    BeatNotFinite(tutti_units::Beat),

    /// `Midi::retarget_input` named a target with no live input.
    #[error("{0:?} has no live input")]
    NoLiveInput(tutti_graph::NodeKey),

    /// `Midi::follow_tempo` named a Group the MIDI input does not hear
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    #[error("the MIDI input does not hear Group {}", .0.get())]
    GroupNotHeard(tutti_ump::MidiGroup),

    /// `Midi::unfollow_tempo` with no tempo being followed.
    #[error("the session is not following a tempo")]
    NotFollowing,

    /// `Midi::unfollow_clock` with no clock being followed.
    #[error("the session is not following a MIDI clock")]
    NotFollowingClock,

    /// `Midi::unfollow_machine_control` with no Machine Control being followed.
    #[error("the session is not following MIDI Machine Control")]
    NotFollowingMachineControl,

    /// A tempo has one source: `Midi::follow_tempo` and a Beat Clock
    /// `Midi::follow_clock` each publish the tempo map, so one is refused while
    /// the other is followed.
    #[error("another follower already publishes the tempo; unfollow it first")]
    TempoSourceTaken,

    /// The hardware input's routes have one declarer: the session's own
    /// (`Midi::route`, `Midi::set_route_fallback`) or a host's
    /// (`Midi::set_routes`), not both at once.
    #[error(
        "the MIDI routes are declared by {0}; clear those before declaring \
         them the other way"
    )]
    TwoRouteDeclarers(RouteDeclarer),
}

/// Who holds the hardware input's routes when the other declarer is refused
/// ([`MidiError::TwoRouteDeclarers`]).
#[cfg(feature = "midi")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteDeclarer {
    /// The session's own rules: `Midi::route` and `Midi::set_route_fallback`.
    Session,
    /// A host's whole table: `Midi::set_routes`.
    Host,
}

#[cfg(feature = "midi")]
impl std::fmt::Display for RouteDeclarer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Session => "the session's own `route`",
            Self::Host => "a host's `set_routes`",
        })
    }
}

/// The work tutti keeps pace with on its own thread, which a target that starts none
/// (the web) cannot run: what a build or a call names in [`Error::NeedsThreads`] and
/// [`SamplerError::NeedsThreads`](crate::SamplerError::NeedsThreads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BackgroundWork {
    /// The system clock's timer thread
    /// ([`SessionBuilder::system_clock`](crate::SessionBuilder::system_clock)).
    Timer,
    /// The audio device's callback.
    Device,
    /// The OS MIDI layer's input pumps and device watch.
    MidiHardware,
    /// The disk streamer that feeds an open stream.
    Streamer,
}

impl std::fmt::Display for BackgroundWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Timer => "the system clock's timer",
            Self::Device => "the audio device",
            Self::MidiHardware => "the MIDI hardware",
            Self::Streamer => "the disk streamer",
        })
    }
}

/// Every subsystem's error, so `?` propagates across crate boundaries.
///
/// Each variant wraps a subsystem error via `#[from]`. Device failures arrive
/// as one `Device` variant (feature `device`) rather than a per-CPAL-call
/// spread: `tutti_cpal::DeviceError` owns that distinction.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The graph engine refused its graph: wraps
    /// `tutti_engine::GraphEngineError`.
    #[error("graph engine: {0}")]
    Graph(#[from] tutti_engine::GraphEngineError),

    /// The root's declaration was refused by the engine's fold table, other
    /// than for a fold: its id is not past the published roots, or too many
    /// published roots are waiting for a plan to carry them (the stream is not
    /// rendering). Wraps `tutti_graph::RootError`.
    #[error(transparent)]
    Root(tutti_graph::RootError),

    /// The graph root cannot be placed on the output: the device has no
    /// channels, or the root is wider than a device at least one of whose
    /// widths has no channel order (a 6-wide root on a 5-channel device would
    /// lose a channel). Wraps `tutti_buffers::FoldError`.
    #[error("the graph root cannot be placed on the output: {0}")]
    Output(#[from] tutti_buffers::FoldError),

    /// The graph root has no place on the output device a restart opened: the
    /// root in force at `width` channels has no fold into the device's
    /// `device` channels as they are stated or default (a root with a channel
    /// no speaker names, or one wider than a device where either width has no
    /// order). Nothing moved, and the stream is left stopped
    /// (`LiveSession::restart`).
    #[error("root {root} ({width:?} wide) has no place on a {device:?} device: {source}")]
    Unplaceable {
        /// The root generation in force.
        root: tutti_graph::RootId,
        /// The root's channels.
        width: tutti_buffers::ChannelLayout,
        /// The device's channels.
        device: tutti_buffers::ChannelLayout,
        /// Why the fold is refused.
        source: tutti_buffers::FoldError,
    },

    /// The output policy's map has no place on the device a restart opened: it
    /// names a channel the device does not have, or no longer fits what is
    /// heard. Nothing moved, and the map is kept, never reset to the
    /// identity: set a new one ([`Session::policy`](crate::Session::policy)).
    #[error("the output map {map:?} has no place on a {device:?} device: {source}")]
    MapUnplaceable {
        /// The map in force.
        map: tutti_engine::OutputMap,
        /// The device's channels.
        device: tutti_buffers::ChannelLayout,
        /// What the map broke on.
        source: tutti_engine::RoutingError,
    },

    /// A clock switch failed after the old stream stopped, and starting the old
    /// clock again failed too: the session is **stopped**, on the clock it was
    /// on. Once what broke is fixed (a graph the re-prepare poisoned, a device
    /// that is gone), restart it with `Session::restart` on a card; on the
    /// system clock there is no restart, and the switch to a card
    /// (`Session::into_device`) starts the stopped stream again.
    #[error("the clock switch failed ({switch}), and so did starting the old clock again ({undo})")]
    SwitchNotUndone {
        /// Why the switch failed.
        switch: Box<Error>,
        /// Why the old clock could not be started again.
        undo: Box<Error>,
    },

    /// An inlet refused its producer's ring ([`Graph::add_inlet`](crate::Graph::add_inlet)).
    #[error(transparent)]
    Inlet(#[from] InletError),

    /// A swap of one node for another was refused
    /// ([`Graph::replace`](crate::Graph::replace)).
    #[error(transparent)]
    Replace(#[from] crate::ReplaceRefused),

    /// The graph refused a re-prepare: a device restart asked for a block
    /// past the engine's capacity, came while another re-prepare was between
    /// its halves, or met a graph that no longer compiles at the new block.
    /// Wraps `tutti_graph::CommitError`.
    #[error("graph re-prepare: {0}")]
    Reprepare(#[from] tutti_graph::CommitError),

    /// A modulation declaration refused when it was made: wraps
    /// [`RouteError`](crate::RouteError).
    #[error(transparent)]
    Modulation(#[from] crate::RouteError),

    /// The system clock's timer thread did not start
    /// ([`SessionBuilder::system_clock`](crate::SessionBuilder::system_clock)).
    #[error(transparent)]
    Clock(#[from] tutti_engine::StartError<tutti_engine::TimerError>),

    /// Audio device failure: enumeration, stream construction, or playback.
    /// Wraps `tutti_cpal::DeviceError`, which owns every CPAL concern.
    #[cfg(feature = "device")]
    #[error(transparent)]
    Device(#[from] tutti_cpal::DeviceError),

    /// A MIDI failure of any kind: the OS port layer, a file, a send, a
    /// route, or a target that takes no MIDI. One variant for the area;
    /// [`MidiError`] says which, each with its own error as the source.
    #[cfg(feature = "midi")]
    #[error(transparent)]
    Midi(#[from] MidiError),

    /// The OS MIDI layer could not start
    /// ([`SessionBuilder::midi_hardware`](crate::SessionBuilder::midi_hardware)):
    /// enumeration or the device watch failed.
    #[cfg(feature = "midi-hardware")]
    #[error("MIDI hardware: {0}")]
    MidiHardware(tutti_midi_hardware::MidiHardwareError),

    /// [`Session::instrument`](crate::Session::instrument) (or
    /// `add_instrument`) was given a node that cannot be played as an
    /// instrument.
    #[cfg(feature = "midi")]
    #[error("{node:?} is not an instrument: {why}")]
    NotInstrument {
        /// The node.
        node: tutti_graph::NodeKey,
        /// Why not.
        why: crate::NotInstrument,
    },

    /// A sampler call or open did not go through ([`SamplerError`](crate::SamplerError)).
    #[cfg(feature = "sampler")]
    #[error(transparent)]
    Sampler(#[from] crate::SamplerError),

    /// Plugin-host bridge failure (VST2/VST3/CLAP). Wraps
    /// `tutti_plugin_types::IpcError`.
    #[cfg(feature = "plugin")]
    #[error("Plugin: {0}")]
    Plugin(#[from] tutti_plugin_types::IpcError),

    /// A whole-bus connection was refused ([`Graph::connect`](crate::Graph::connect)).
    #[error(transparent)]
    Connect(#[from] ConnectError),

    /// A declaration was refused ([`Mods::range`](crate::Mods::range),
    /// [`Connection::gain`](crate::Connection::gain)).
    #[error(transparent)]
    Declaration(#[from] DeclarationError),

    /// A bus edge was refused ([`Graph::connect`](crate::Graph::connect)).
    #[error(transparent)]
    Bus(#[from] tutti_graph::BusError),

    /// A transport command was refused ([`Session::play`](crate::Session::play),
    /// [`Session::schedule`](crate::Session::schedule) and the other verbs):
    /// the queue was full, or the command cannot fire. The command is handed
    /// back in it.
    #[error(transparent)]
    Timed(tutti_timeline::TimedError<tutti_timeline::TransportCommand>),

    /// A file could not be decoded (`read_wave`, `FileIn`).
    #[error(transparent)]
    Decode(#[from] tutti_codec::DecodeError),

    /// A wave cannot be placed into a sampler voice's channels.
    #[cfg(feature = "sampler")]
    #[error(transparent)]
    Place(#[from] tutti_sampler::PlaceError),

    /// A bounce could not start or finish (a cancel included:
    /// [`BounceError::Cancelled`](crate::BounceError::Cancelled)).
    #[cfg(feature = "export")]
    #[error(transparent)]
    Bounce(#[from] crate::BounceError),

    /// A take could not be armed or did not end cleanly
    /// ([`Session::record`](crate::Session::record)).
    #[cfg(feature = "capture")]
    #[error(transparent)]
    Take(#[from] crate::TakeError),

    /// The session's bell tick task did not start: the OS refused its thread
    /// ([`SessionBuilder::build`](crate::SessionBuilder)).
    #[error("the session's bell tick did not start: {0}")]
    Bells(std::io::Error),

    /// The build needs work that keeps pace with the hardware or the audio thread, and
    /// this target starts no thread to run it (the web): an offline build with no
    /// hardware does not.
    #[error("{0} needs a thread of tutti's, and this target starts none")]
    NeedsThreads(BackgroundWork),

    /// Standard I/O error encountered while reading or writing audio data.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// `Result<T, `[`enum@Error`]`>`.
pub type Result<T> = core::result::Result<T, Error>;

impl From<tutti_graph::RootError> for Error {
    /// A fold refusal is [`Error::Output`]; the rest are [`Error::Root`].
    fn from(e: tutti_graph::RootError) -> Self {
        match e {
            tutti_graph::RootError::Fold(fold) => Self::Output(fold),
            other => Self::Root(other),
        }
    }
}
