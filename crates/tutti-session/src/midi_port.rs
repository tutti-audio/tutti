//! MIDI ports as pool entries (feature `midi-hardware`): [`midi_input`] and
//! [`midi_output`] declare a saved device, [`Session::midi_input`] and
//! [`Session::midi_output`] read the port back as a [`MidiInput`] or
//! [`MidiOutput`].
//!
//! A port needs no device to exist. An entry whose saved device is absent
//! loads at once and reads [`Presence::Lost`] with [`PortCause::Unplugged`];
//! the session opens it in place, under the same key and the same [`Life`],
//! when the device arrives and is the one a load would bind (surely the saved
//! device, or the only present one of its name). Only a saved device
//! that several present devices could be fails the load, with the candidates
//! ([`MidiPortError::Ambiguous`]).
//!
//! An input is fed to the graph through an ingest node of its own, made when
//! the entry loads: it releases the notes its device holds before it goes, and
//! survives an unplug and a replug, so what is wired to it stays wired. An
//! output is a [`MidiOutNode`](tutti_midi_runtime::MidiOutNode) of its own,
//! whose events the session sends to the device each update.

use std::sync::{Arc, Mutex, PoisonError, Weak};

use tutti_graph::{EventTapReader, Losses, NodeKey};
use tutti_midi_device::{DeviceId, DeviceInfo, Learned, Match, Resolved, SavedDevice};
use tutti_midi_hardware::{
    CiPeerInfo, DeviceStream, MidiSession, PeerProfiles, ProfileError, ProfileSwitch, ReportAsk,
    ReportError, ReportPeer,
};
use tutti_midi_runtime::{
    DeviceEpochs, InputReaders, InputWriters, JrStream, MidiIngestControls, MidiInputPorts,
};
use tutti_rt::{Counter, Gauge};
use tutti_types::{Resyncs, Seconds};
use tutti_ump::StreamConfig;

use crate::midi::{LossReaders, LossWriters, UmpOut, loss_cells};
use crate::pool::{Declared, HandleError, Life, Load, LoadError, PoolEntry};
use crate::{
    EventInPort, EventOutPort, MidiReport, Peer, PeerError, Presence, Resource, ResourceFact,
    ResourceId, Session, SessionEvent,
};

// --- Vocabulary ---

/// Why a MIDI port is not live.
///
/// The reason a [`MidiInput`] or a [`MidiOutput`] reads [`Presence::Lost`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PortCause {
    /// The saved device is not present: it was unplugged, or it was never seen
    /// this run. The port opens by itself when the device arrives and is the one a
    /// load would bind.
    Unplugged,
    /// The device returned and is the one a load would bind, but opening it failed.
    /// It is tried again when its device next appears in the device list.
    ReopenFailed,
    /// The reader of an open input died while its device stayed listed. The
    /// session reopens it at once, and the port reads this only when that
    /// failed; it is tried again when its device next appears in the device
    /// list.
    ReaderFailed,
}

/// What an output port is opened with. Part of the entry's identity: declaring
/// the port again with other options reopens it.
///
/// ```
/// use tutti_session::OutputOptions;
///
/// let options = OutputOptions::new().active_sensing(true);
/// assert_ne!(options, OutputOptions::new());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct OutputOptions {
    /// Whether the port sends Active Sensing: see [`active_sensing`](Self::active_sensing).
    pub active_sensing: bool,
}

impl OutputOptions {
    /// The options of an output opened with none.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            active_sensing: false,
        }
    }

    /// Whether the port sends Active Sensing (`0xFE`) for its whole life: from
    /// its own thread, whenever it has been silent for 270 ms
    /// ([`ACTIVE_SENSING_INTERVAL`](tutti_midi_runtime::ACTIVE_SENSING_INTERVAL)),
    /// until the port closes. Re-declaring the port's node or a stalled
    /// [`update`](crate::Session::update) does not stop it, and a receiver that
    /// has seen one releases its notes when they stop (MIDI 1.0 Detailed
    /// Specification p. 32).
    ///
    /// A port whose device is in the MIDI 2.0 Protocol sends none: MIDI 2.0
    /// has no Active Sensing. It starts when a negotiation moves the device to
    /// the MIDI 1.0 Protocol.
    #[must_use]
    pub const fn active_sensing(mut self, on: bool) -> Self {
        self.active_sensing = on;
        self
    }
}

/// Why a MIDI port did not load.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MidiPortError {
    /// The session has no OS MIDI layer
    /// ([`SessionBuilder::midi_hardware`](crate::SessionBuilder::midi_hardware)
    /// was not asked for).
    #[error("the session has no OS MIDI layer; build it with `midi_hardware(options)`")]
    NoHardware,
    /// Several present devices could be the saved one, or its ordinal cannot be
    /// trusted across a restart: these are the candidates, ordered by id, for
    /// the app to ask about.
    #[error("{} present devices could be the saved one", .0.len())]
    Ambiguous(Vec<DeviceInfo>),
    /// Another port of this session holds the device open already.
    #[error("another port of this session holds {0:?} open")]
    InUse(DeviceId),
    /// The device's previous connection still holds the port's cells (timing and
    /// losses), so the new one would report nothing.
    #[error("the previous connection of {0:?} still holds the port's cells")]
    CellsHeld(DeviceId),
    /// The OS layer refused to open the device.
    #[error("MIDI hardware: {0}")]
    Hardware(Arc<tutti_midi_hardware::Error>),
}

/// What a [`MidiInput`] says of itself: one snapshot, as of the last update
/// that saw a change.
///
/// What an unplugged input reads is what it last had.
#[derive(Clone, Debug)]
pub struct MidiInputInfo {
    device: DeviceInfo,
    stream: Option<DeviceStream>,
    learned: Learned,
    peers: Arc<[CiPeerInfo]>,
    profiles: Arc<[PeerProfiles]>,
    report_peers: Arc<[ReportPeer]>,
}

impl MidiInputInfo {
    /// The device as the OS listed it when the port last opened it (the saved
    /// device while it never has).
    #[must_use]
    pub const fn device(&self) -> &DeviceInfo {
        &self.device
    }

    /// The UMP Stream the device settled (M2-104 §7.1.6): the Endpoint it
    /// declared or the OS's description of it, the Protocol and JR Timestamps in
    /// force, and what the settle rests on. `None` until the negotiation has
    /// settled, which takes the first seconds after the port opens.
    #[must_use]
    pub const fn stream(&self) -> Option<&DeviceStream> {
        self.stream.as_ref()
    }

    /// What the device has said about itself so far: its Device Identity and
    /// Product Instance Id. A device identity names a model and a Product
    /// Instance Id one unit (M2-101 §5.8.3.1).
    #[must_use]
    pub const fn learned(&self) -> &Learned {
        &self.learned
    }

    /// The MIDI-CI peers found on the input, by Group then MUID: what a device
    /// that answered the session's Discovery declared.
    #[must_use]
    pub fn peers(&self) -> &[CiPeerInfo] {
        &self.peers
    }

    /// The Profiles of the peers that declared Profile Configuration, by Group
    /// then MUID (M2-101 section 7). A [`ProfileSwitch`] is made from a listed
    /// Profile of these.
    #[must_use]
    pub fn profiles(&self) -> &[PeerProfiles] {
        &self.profiles
    }

    /// The peers that declared Process Inquiry and support MIDI Message Report
    /// (M2-101 section 9): a [`ReportAsk`] is made from one.
    #[must_use]
    pub fn report_peers(&self) -> &[ReportPeer] {
        &self.report_peers
    }

    /// The device as an app keeps it: the listing's row with what it has said,
    /// for the app's own file (with the `serde` feature of `tutti-midi-device`).
    #[must_use]
    pub fn saved(&self) -> SavedDevice {
        SavedDevice::new(self.device.clone(), self.learned)
    }

    /// Compares the input with the device `saved` describes, by what it has said:
    /// [`Match::SameUnit`] only on an equal Product Instance Id, never on a
    /// Device Identity; [`Match::Different`] when something known on both sides
    /// differs. Until the device has spoken it is [`Match::Unknown`].
    #[must_use]
    pub fn confirm(&self, saved: &SavedDevice) -> Match {
        saved.confirm(&self.learned)
    }
}

/// What a [`MidiOutput`] says of itself: one snapshot.
#[derive(Clone, Debug)]
pub struct MidiOutputInfo {
    device: DeviceInfo,
    config: StreamConfig,
}

impl MidiOutputInfo {
    /// The device as the OS listed it when the port last opened it.
    #[must_use]
    pub const fn device(&self) -> &DeviceInfo {
        &self.device
    }

    /// The Protocol and JR Timestamps the output sends in: the OS's description
    /// of the device until its negotiation has settled.
    #[must_use]
    pub const fn config(&self) -> StreamConfig {
        self.config
    }
}

// --- The shared record ---

/// What the session writes and the handles read: the last known snapshot and
/// presence of one port.
struct Cell<I> {
    info: Arc<I>,
    presence: Presence<PortCause>,
}

/// An input port's record: shared by its entry, its handles and the session's
/// table.
pub(crate) struct InputShared {
    id: ResourceId,
    cell: Mutex<Cell<MidiInputInfo>>,
    requests: Mutex<Vec<InputRequest>>,
    /// The cells a connection's device input writes: the latest correction the
    /// port's clock map made, its re-anchors and the events its inlet had no
    /// room for. Made once with the entry; a connection writes them and closes
    /// them when it ends, and the counts are cumulative across connections.
    cells: InputReaders,
    /// The MIDI-CI messages the device's output refused, cumulative across
    /// connections: the port owns the CI conversation, so the refusal is its.
    ci_refused: Counter<u64>,
    /// What the port's ingest node lost: translated events its mailbox had no
    /// room for. The node outlives every connection.
    ingest: MidiIngestControls,
}

/// What a handle asked of the device, for the session to send at its next poll.
enum InputRequest {
    Profile(ProfileSwitch),
    Report(ReportAsk),
}

impl InputShared {
    fn read<R>(&self, read: impl FnOnce(&Cell<MidiInputInfo>) -> R) -> R {
        read(&self.cell.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// An output port's record.
pub(crate) struct OutputShared {
    id: ResourceId,
    cell: Mutex<Cell<MidiOutputInfo>>,
    /// What the port did not send, counted by the session.
    losses: LossReaders,
}

impl OutputShared {
    fn read<R>(&self, read: impl FnOnce(&Cell<MidiOutputInfo>) -> R) -> R {
        read(&self.cell.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

// --- The entries ---

/// What the pool holds of a loaded input: the record, and the claim the
/// session's table watches for the entry's end. Read it through the
/// [`MidiInput`] handle.
#[derive(Debug)]
pub struct InputLoaded {
    shared: Arc<InputShared>,
    node: NodeKey,
    /// Dropped with the loaded value: the entry was released or swapped.
    _token: Arc<()>,
}

/// What the pool holds of a loaded output. Read it through the [`MidiOutput`]
/// handle.
#[derive(Debug)]
pub struct OutputLoaded {
    shared: Arc<OutputShared>,
    node: NodeKey,
    _token: Arc<()>,
}

/// An input port: [`midi_input`]'s entry. Its identity is the saved device.
#[derive(Clone, Debug)]
pub struct MidiInputEntry {
    saved: DeviceInfo,
}

/// Declares the input port of the saved device `saved` under `key`.
///
/// The entry loads in [`Session::update`], on the session's thread. It
/// binds a device that is surely the saved one, or the only present one of its
/// name; an absent one loads with the port reading
/// [`Lost`](Presence::Lost)([`PortCause::Unplugged`]) and opens when it
/// arrives.
pub fn midi_input<K>(key: K, saved: DeviceInfo) -> Declared<K, MidiInputEntry> {
    Declared::new(key, MidiInputEntry { saved })
}

impl PoolEntry for MidiInputEntry {
    type Identity = DeviceInfo;
    type Loaded = InputLoaded;
    type Handle = MidiInput;

    fn identity(&self) -> DeviceInfo {
        self.saved.clone()
    }

    fn load(&self) -> Load<InputLoaded> {
        let saved = self.saved.clone();
        Load::on_session(move |cx| {
            let id = cx.mint_resource_id();
            cx.midi()
                .open_input_port(id, saved)
                .map_err(|error| LoadError::Midi(Arc::new(error)))
        })
    }

    fn handle(loaded: &InputLoaded, life: &Life) -> MidiInput {
        MidiInput {
            shared: Arc::clone(&loaded.shared),
            node: loaded.node,
            life: life.clone(),
        }
    }
}

/// An output port: [`midi_output`]'s entry. Its identity is the saved device
/// and the options.
#[derive(Clone, Debug)]
pub struct MidiOutputEntry {
    saved: DeviceInfo,
    options: OutputOptions,
}

/// Declares the output port of the saved device `saved` under `key`, opened
/// with no options (`.options(..)` on the declared entry sets them).
pub fn midi_output<K>(key: K, saved: DeviceInfo) -> Declared<K, MidiOutputEntry> {
    Declared::new(
        key,
        MidiOutputEntry {
            saved,
            options: OutputOptions::new(),
        },
    )
}

impl<K> Declared<K, MidiOutputEntry> {
    /// This port, opened with `options`: they are part of its identity.
    pub fn options(mut self, options: OutputOptions) -> Self {
        self.entry_mut().options = options;
        self
    }
}

impl PoolEntry for MidiOutputEntry {
    type Identity = (DeviceInfo, OutputOptions);
    type Loaded = OutputLoaded;
    type Handle = MidiOutput;

    fn identity(&self) -> (DeviceInfo, OutputOptions) {
        (self.saved.clone(), self.options.clone())
    }

    fn load(&self) -> Load<OutputLoaded> {
        let saved = self.saved.clone();
        let options = self.options.clone();
        Load::on_session(move |cx| {
            let id = cx.mint_resource_id();
            cx.midi()
                .open_output_port(id, saved, options)
                .map_err(|error| LoadError::Midi(Arc::new(error)))
        })
    }

    fn handle(loaded: &OutputLoaded, life: &Life) -> MidiOutput {
        MidiOutput {
            shared: Arc::clone(&loaded.shared),
            node: loaded.node,
            life: life.clone(),
        }
    }
}

// --- The handles ---

/// An input port, read by key: [`Session::midi_input`].
///
/// `Clone` and `Send`, and it owns nothing: dropping it ends nothing, and the
/// port ends when the pool no longer names its entry. It follows its entry
/// across an unplug and a replug; once the entry is released or reloaded with
/// another identity it reads [`Presence::Closed`].
#[derive(Clone, Debug)]
pub struct MidiInput {
    shared: Arc<InputShared>,
    node: NodeKey,
    life: Life,
}

impl std::fmt::Debug for InputShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputShared")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl MidiInput {
    /// The port's ingest node: the node to wire from and to follow. Its event
    /// outputs are one per Group and channel
    /// ([`MidiNodes::input_ports`](crate::MidiNodes::input_ports)), which a route
    /// rule, a follower or a tap reads. The node is in the graph while the entry
    /// is loaded, and the same node serves the port across an unplug and a
    /// replug.
    ///
    /// Its one event input is the session's: the device's inlet is wired to it
    /// at every open, so a host `set_sources` onto it would cut the device off
    /// until the next replug. The graph has no way to refuse an edge into a node
    /// by who owns it, so it is not refused; do not wire into it.
    #[must_use]
    pub const fn node(&self) -> NodeKey {
        self.node
    }

    /// The latest correction the port's clock map made to place the device's
    /// events on the graph's frames, signed in seconds: positive when a reading
    /// came late. A steady input reads a small figure, and one whose timing is
    /// unsteady jumps.
    ///
    /// The gauge belongs to the entry, not to a connection: it reads the same
    /// cell across an unplug and a replug, and reads 0 until a map first makes
    /// a correction. While the device is away it holds the last correction
    /// ([`is_closed`](Gauge::is_closed) says so); a replug does not reset it,
    /// and the new connection's first map writes over it.
    #[must_use]
    pub fn timing(&self) -> Gauge<Seconds> {
        self.shared.cells.timing.clone()
    }

    /// How many times the port's clock map re-anchored (a reading too far from
    /// its prediction, or one the map refused and was rebuilt on), so an app can
    /// show an input whose timing is unsteady. A map that was only rescaled for
    /// another rate is not one, and neither is the first map of a connection.
    ///
    /// Cumulative for the entry: the count carries across an unplug and a
    /// replug on the same cell. It is meaningful only against a real-time
    /// clock: an offline session's readings re-anchor often.
    #[must_use]
    pub fn resyncs(&self) -> Counter<Resyncs> {
        self.shared.cells.resyncs.clone()
    }

    /// What this port did not take in, cumulative over its life and across an
    /// unplug and a replug: [`lost`](Losses::lost) are events its device's inlet
    /// had no room for (a stalled audio callback with a busy controller), and
    /// [`refused`](Losses::refused) are translated events its ingest node's
    /// mailbox had no room for in one block, and the MIDI-CI messages its
    /// conversation sent that the device's output refused (a refused message is
    /// not sent again, so the count is how many a peer may never have heard).
    /// The CI count is the input's because the input owns the conversation, its
    /// peers and Profiles ([`MidiInputInfo::profiles`]); the session's MIDI
    /// output never carries it.
    #[must_use]
    pub fn losses(&self) -> Losses {
        Losses::lost(self.shared.cells.lost.get())
            + self.shared.ingest.losses()
            + Losses::refused(self.shared.ci_refused.get())
    }

    /// Asks a peer to enable or disable a Profile it lists
    /// ([`MidiInputInfo::profiles`]): the request goes out at the next update,
    /// and its end is a [`MidiReport::CiProfiles`].
    ///
    /// # Errors
    ///
    /// [`PeerError::Released`] once the entry is released or swapped;
    /// [`PeerError::Disconnected`] while the port is lost;
    /// [`PeerError::Refused`] with [`ProfileError::NotOpen`] for a switch made
    /// from another input, [`ProfileError::NoSuchPeer`] when the peer is gone
    /// from the port's peers and [`ProfileError::NotListed`] when it no longer
    /// lists the Profile there, as the port's info reads now. A peer that goes
    /// between the call and the next update ends the request as a
    /// [`ProfilesChange::NotQueued`](tutti_midi_hardware::ProfilesChange::NotQueued)
    /// event.
    pub fn set_profile(&self, switch: ProfileSwitch) -> Result<(), PeerError<ProfileError>> {
        let info = self.sendable()?;
        if switch.device() != &info.device.id {
            return Err(PeerError::Refused(ProfileError::NotOpen));
        }
        switch.check(&info.profiles).map_err(PeerError::Refused)?;
        self.enqueue(InputRequest::Profile(switch));
        Ok(())
    }

    /// Asks a peer for a MIDI Message Report ([`MidiInputInfo::report_peers`]):
    /// the request goes out at the next update, and its end is a
    /// [`MidiReport::CiProcess`].
    ///
    /// # Errors
    ///
    /// As [`set_profile`](Self::set_profile), with [`PeerError::Refused`] of
    /// [`ReportError::NotOpen`] for a request made from another input, and
    /// [`ReportError::NoSuchPeer`] when the peer is gone or declared no Process
    /// Inquiry with MIDI Message Report. A peer that goes between the call and
    /// the next update ends the request as a
    /// [`ProcessChange::NotQueued`](tutti_midi_hardware::ProcessChange::NotQueued)
    /// event.
    pub fn report(&self, ask: ReportAsk) -> Result<(), PeerError<ReportError>> {
        let info = self.sendable()?;
        if ask.device() != &info.device.id {
            return Err(PeerError::Refused(ReportError::NotOpen));
        }
        ask.check(&info.report_peers).map_err(PeerError::Refused)?;
        self.enqueue(InputRequest::Report(ask));
        Ok(())
    }

    /// The info a request is checked against, when the port can take one.
    fn sendable<E>(&self) -> Result<Arc<MidiInputInfo>, PeerError<E>> {
        self.life.check()?;
        let (info, presence) = self
            .shared
            .read(|cell| (Arc::clone(&cell.info), cell.presence));
        if presence != Presence::Live {
            return Err(PeerError::Disconnected);
        }
        Ok(info)
    }

    fn enqueue(&self, request: InputRequest) {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request);
    }
}

impl crate::peer::sealed::Sealed for MidiInput {}

impl Resource for MidiInput {
    fn id(&self) -> ResourceId {
        self.shared.id
    }
}

impl Peer for MidiInput {
    type Info = MidiInputInfo;
    type Cause = PortCause;

    fn info(&self) -> Arc<MidiInputInfo> {
        self.shared.read(|cell| Arc::clone(&cell.info))
    }

    fn presence(&self) -> Presence<PortCause> {
        self.life
            .presence(|| self.shared.read(|cell| cell.presence))
    }
}

/// An output port, read by key: [`Session::midi_output`]. As [`MidiInput`],
/// for the device an output sends to.
#[derive(Clone, Debug)]
pub struct MidiOutput {
    shared: Arc<OutputShared>,
    node: NodeKey,
    life: Life,
}

impl std::fmt::Debug for OutputShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputShared")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl MidiOutput {
    /// The port's output node: the node to wire to. What is wired to its event
    /// input (`Graph::set_sources`) is sent to the device each update, beside the
    /// session's MIDI clock, which feeds it as a feed of its own
    /// ([`Feeder::Clock`](crate::Feeder::Clock)) that a host's `set_sources` does not replace. The node is
    /// in the graph while the entry is loaded, and the same node serves the
    /// port across an unplug and a replug.
    #[must_use]
    pub const fn node(&self) -> NodeKey {
        self.node
    }

    /// What this port did not send, cumulative over its life and across an
    /// unplug and a replug: [`lost`](Losses::lost) are events its node's tap had
    /// no room for (1 024 between two updates, the session's MIDI clock
    /// included), and [`refused`](Losses::refused) those the device would not
    /// take, that were scheduled more than
    /// [`MAX_OUTPUT_AHEAD`](tutti_midi_runtime::MAX_OUTPUT_AHEAD) ahead, or that
    /// had no device to go to.
    #[must_use]
    pub fn losses(&self) -> Losses {
        self.shared.losses.get()
    }
}

impl crate::peer::sealed::Sealed for MidiOutput {}

impl Resource for MidiOutput {
    fn id(&self) -> ResourceId {
        self.shared.id
    }
}

impl Peer for MidiOutput {
    type Info = MidiOutputInfo;
    type Cause = PortCause;

    fn info(&self) -> Arc<MidiOutputInfo> {
        self.shared.read(|cell| Arc::clone(&cell.info))
    }

    fn presence(&self) -> Presence<PortCause> {
        self.life
            .presence(|| self.shared.read(|cell| cell.presence))
    }
}

impl<C: crate::Clock> Session<C> {
    /// The input port of the `midi_input` entry under `key`.
    ///
    /// # Errors
    ///
    /// [`HandleError::NotPooled`] when the key is not declared (never, or
    /// released); [`HandleError::WrongKind`] when it is another kind of entry;
    /// [`HandleError::NotLoaded`] before the load lands, or after it failed (an
    /// ambiguous saved device: the state carries the candidates).
    pub fn midi_input<K>(&self, key: &K) -> Result<MidiInput, HandleError<K>>
    where
        K: Ord + Clone + std::fmt::Debug + Send + Sync + 'static,
    {
        self.pool.handle::<K, MidiInputEntry>(key)
    }

    /// The output port of the `midi_output` entry under `key`.
    ///
    /// # Errors
    ///
    /// As [`midi_input`](Self::midi_input).
    pub fn midi_output<K>(&self, key: &K) -> Result<MidiOutput, HandleError<K>>
    where
        K: Ord + Clone + std::fmt::Debug + Send + Sync + 'static,
    {
        self.pool.handle::<K, MidiOutputEntry>(key)
    }
}

// --- The session's table of ports ---

/// One input port as the session keeps it.
struct InputPort {
    /// Upgrades while the entry's loaded value lives.
    token: Weak<()>,
    shared: Arc<InputShared>,
    node: NodeKey,
    controls: MidiIngestControls,
    epochs: DeviceEpochs,
    /// The inlet node of the open connection, in the graph.
    inlet: Option<NodeKey>,
    /// What the entry was declared with: what a returning device is matched on.
    saved: DeviceInfo,
    /// The device the port holds open.
    open: Option<DeviceId>,
}

/// One output port as the session keeps it.
struct OutputPort {
    token: Weak<()>,
    shared: Arc<OutputShared>,
    node: NodeKey,
    tap: EventTapReader,
    saved: DeviceInfo,
    /// What the port is opened with: applied to each device output it opens.
    options: OutputOptions,
    losses: LossWriters,
    /// What the open device's output had counted of its own losses when they
    /// were last folded into `losses`.
    device_seen: Losses,
    /// The open device's output with its stamp stream; `None` while the device
    /// is away.
    out: Option<UmpOut>,
    /// The stamp stream of an output that was closed, which the next one carries
    /// on.
    parked: Option<JrStream>,
}

/// An input node that is going: its device's notes are being released.
struct Retiring {
    node: NodeKey,
    epochs: DeviceEpochs,
}

/// The ports of the session, and what they are opened with.
#[derive(Default)]
pub(crate) struct Ports {
    inputs: Vec<InputPort>,
    outputs: Vec<OutputPort>,
    retiring: Vec<Retiring>,
}

impl std::fmt::Debug for Ports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ports")
            .field("inputs", &self.inputs.len())
            .field("outputs", &self.outputs.len())
            .finish_non_exhaustive()
    }
}

/// What the graph edits of a phase were: whether it was edited, and whether the
/// set of input ports changed (the routes are compiled again).
#[derive(Clone, Copy, Default)]
pub(crate) struct Edits {
    pub(crate) graph: bool,
    pub(crate) routes: bool,
}

impl std::ops::BitOrAssign for Edits {
    fn bitor_assign(&mut self, other: Self) {
        self.graph |= other.graph;
        self.routes |= other.routes;
    }
}

fn changed(out: &mut Vec<SessionEvent>, id: ResourceId, fact: ResourceFact) {
    out.push(SessionEvent::ResourceChanged { id, fact });
}

/// Sets a record's presence, saying so once when it changed.
fn set_presence<I>(
    cell: &Mutex<Cell<I>>,
    id: ResourceId,
    presence: Presence<PortCause>,
    out: &mut Vec<SessionEvent>,
) {
    let mut cell = cell.lock().unwrap_or_else(PoisonError::into_inner);
    if cell.presence != presence {
        cell.presence = presence;
        changed(out, id, ResourceFact::Presence);
    }
}

/// The snapshot of the open input `device`, as the OS layer reads it now.
fn input_info(io: &MidiSession, device: &DeviceInfo) -> MidiInputInfo {
    MidiInputInfo {
        device: device.clone(),
        stream: io.stream(&device.id),
        learned: io.learned(&device.id),
        peers: io.ci_peers(&device.id),
        profiles: io.profiles(&device.id),
        report_peers: io.report_peers(&device.id),
    }
}

impl InputPort {
    /// Whether the entry that made this port still holds it.
    fn is_held(&self) -> bool {
        self.token.strong_count() > 0
    }

    /// Takes in what the OS layer reads of the open device, saying so when it
    /// differs from what the handles read.
    fn refresh(&self, io: &MidiSession, out: &mut Vec<SessionEvent>) {
        let Some(id) = &self.open else { return };
        let Some(device) = io.devices().list().into_iter().find(|d| d.id == *id) else {
            return;
        };
        let info = input_info(io, &device);
        let mut cell = self
            .shared
            .cell
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let same = cell.info.device == info.device
            && cell.info.stream == info.stream
            && cell.info.learned == info.learned
            && *cell.info.peers == *info.peers
            && *cell.info.profiles == *info.profiles
            && *cell.info.report_peers == *info.report_peers;
        if !same {
            cell.info = Arc::new(info);
            changed(out, self.shared.id, ResourceFact::Info);
        }
    }
}

impl Ports {
    /// Every input port ingests classic MPE in `mode` from the next block.
    pub(crate) fn set_mpe_mode(&self, mode: tutti_midi_protocol::MpeMode) {
        for port in &self.inputs {
            port.controls.set_mpe_mode(mode);
        }
    }

    /// The ingest nodes of the ports, and of those going, for the routes to read.
    pub(crate) fn input_nodes(&self) -> Vec<NodeKey> {
        // A node going keeps its routes until it is taken out: its release goes
        // out through them.
        self.inputs
            .iter()
            .map(|port| port.node)
            .chain(self.retiring.iter().map(|going| going.node))
            .collect()
    }

    /// Whether `node` is the ingest node of an input port.
    pub(crate) fn is_input_node(&self, node: NodeKey) -> bool {
        self.inputs.iter().any(|port| port.node == node)
    }

    /// Takes the MPE Profile zones the peers of `device` have enabled (M2-120) as
    /// what its ingest node follows: the zones of every peer on a Group
    /// together, or none where no peer has the Profile enabled.
    fn sync_mpe_profiles(&self, io: &MidiSession, device: &DeviceId) {
        let Some(port) = self.inputs.iter().find(|p| p.open.as_ref() == Some(device)) else {
            return;
        };
        let mut zones = [None; tutti_ump::MidiGroup::COUNT as usize];
        for peer in io.profiles(device).iter() {
            let Some(enabled) = peer.mpe_zones() else {
                continue;
            };
            let held: &mut Option<tutti_midi_protocol::MpeZones> = &mut zones[peer.group().index()];
            *held = match *held {
                None => Some(enabled),
                // A zone that shares a Channel with one held is left out (M2-120
                // section 3.4).
                Some(held) => Some(
                    enabled
                        .zones()
                        .fold(held, |all, zone| all.with(*zone).unwrap_or(all)),
                ),
            };
        }
        for group in tutti_ump::MidiGroup::all() {
            port.controls.set_mpe_profile(group, zones[group.index()]);
        }
    }

    /// Opens `info` for the input port at `at` (the port is closed): the inlet
    /// joins the graph, wired to the ingest node, and a device arrives there.
    fn connect_input(
        &mut self,
        at: usize,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph,
        info: &DeviceInfo,
    ) -> Result<(), MidiPortError> {
        let port = &mut self.inputs[at];
        // The previous connection's input has dropped with its device being
        // closed; one that still holds the cells is refused, never connected
        // without readings.
        let clock = InputWriters::reopen(&port.shared.cells)
            .ok_or_else(|| MidiPortError::CellsHeld(info.id.clone()))?;
        let ci_refused = port
            .shared
            .ci_refused
            .reopen()
            .ok_or_else(|| MidiPortError::CellsHeld(info.id.clone()))?;
        match io
            .connect_input_with(&info.id, clock, ci_refused)
            .map_err(|e| MidiPortError::Hardware(Arc::new(e)))?
        {
            tutti_midi_hardware::Connected::Opened(inlet) => {
                let key = graph.add_event_inlet(inlet).erase();
                // The ingest node has the one event input.
                let _ =
                    graph.set_sources(EventInPort::new(port.node, 0), [EventOutPort::new(key, 0)]);
                port.inlet = Some(key);
                port.epochs.arrive();
                port.open = Some(info.id.clone());
                Ok(())
            }
            tutti_midi_hardware::Connected::AlreadyOpen => {
                Err(MidiPortError::InUse(info.id.clone()))
            }
        }
    }

    /// Closes the open input port at `at`: the device's notes are released by
    /// the ingest node, and the inlet leaves the graph.
    fn disconnect_input(
        &mut self,
        at: usize,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph,
    ) {
        let port = &mut self.inputs[at];
        if let Some(id) = port.open.take() {
            io.disconnect_input(&id);
        }
        if let Some(key) = port.inlet.take() {
            // A host that removed the inlet itself leaves nothing to take.
            let _ = graph.remove(key);
        }
        port.epochs.depart();
        port.controls.clear_mpe_profiles();
    }

    /// The step of the update's `Phase::Poll` for the ports: retires those whose
    /// entry ended, takes in a device that went or came, sends what the handles
    /// asked, and reads what the devices said. What was edited.
    pub(crate) fn poll(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph,
        rate: tutti_types::SampleRate,
        events: Vec<tutti_midi_device::DeviceEvent>,
        now: std::time::Instant,
        out: &mut Vec<SessionEvent>,
    ) -> Edits {
        let mut edits = Edits::default();
        edits |= self.retire(io, graph);
        for event in events {
            out.push(MidiReport::of_device(event.clone()).into());
            match event {
                tutti_midi_device::DeviceEvent::Removed(device) => {
                    edits |= self.lose(io, graph, &device, out);
                }
                tutti_midi_device::DeviceEvent::Added(device) => {
                    edits |= self.restore(io, graph, rate, &device, out);
                }
                _ => {}
            }
        }
        self.send_requests(io, out);
        edits |= self.step(io, graph, now, out);
        edits
    }

    /// Ends the ports whose entry was released or swapped, and takes out the
    /// ingest nodes that have released what their devices held.
    fn retire(&mut self, io: &mut MidiSession, graph: &mut crate::graph::Graph) -> Edits {
        let mut edits = Edits::default();
        let mut at = 0;
        while at < self.inputs.len() {
            if self.inputs[at].is_held() {
                at += 1;
                continue;
            }
            self.disconnect_input(at, io, graph);
            let port = self.inputs.remove(at);
            self.retiring.push(Retiring {
                node: port.node,
                epochs: port.epochs,
            });
            edits.graph = true;
            edits.routes = true;
        }
        let before = self.outputs.len();
        self.outputs.retain_mut(|port| {
            let held = port.token.strong_count() > 0;
            if !held {
                // A host that removed the node itself leaves nothing to take.
                let _ = graph.remove(port.node);
            }
            held
        });
        edits.graph |= self.outputs.len() != before;
        self.retiring.retain(|going| {
            // The node's block that released the notes has run.
            let done = going.epochs.is_settled();
            if done {
                let _ = graph.remove(going.node);
                edits.graph = true;
                edits.routes = true;
            }
            !done
        });
        edits
    }

    /// The device `device` left the list: the ports that held it close and read
    /// [`PortCause::Unplugged`].
    fn lose(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph,
        device: &DeviceInfo,
        out: &mut Vec<SessionEvent>,
    ) -> Edits {
        let mut edits = Edits::default();
        for at in 0..self.inputs.len() {
            if self.inputs[at].open.as_ref() != Some(&device.id) {
                continue;
            }
            self.disconnect_input(at, io, graph);
            let shared = &self.inputs[at].shared;
            set_presence(
                &shared.cell,
                shared.id,
                Presence::Lost(PortCause::Unplugged),
                out,
            );
            edits.graph = true;
        }
        for port in &mut self.outputs {
            let held = port.out.as_ref().and_then(UmpOut::device);
            if held.is_some_and(|open| *open.id() == device.id) {
                port.fold_device_losses();
                // The stamp stream is kept for the device's return.
                port.parked = port.out.take().map(UmpOut::into_stream);
                port.device_seen = Losses::NONE;
                set_presence(
                    &port.shared.cell,
                    port.shared.id,
                    Presence::Lost(PortCause::Unplugged),
                    out,
                );
            }
        }
        edits
    }

    /// `arrived` was added to the list: the lost ports it is open again (it is
    /// what a load would bind: surely the saved device, or the only one of its
    /// name), and a port that fails to open reads [`PortCause::ReopenFailed`] and
    /// is tried again when a device next arrives.
    fn restore(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph,
        rate: tutti_types::SampleRate,
        arrived: &DeviceInfo,
        out: &mut Vec<SessionEvent>,
    ) -> Edits {
        let mut edits = Edits::default();
        // The matcher a load binds by: the device that is surely the saved one,
        // or the only present one of its name. Several that could be it stay
        // for the app to choose.
        let is_arrived = |io: &MidiSession, saved: &DeviceInfo| {
            matches!(
                io.devices().resolve(saved),
                Resolved::Exact(found) | Resolved::SameName(found) if found.id == arrived.id
            )
        };
        for at in 0..self.inputs.len() {
            let port = &self.inputs[at];
            if port.open.is_some() || !arrived.direction.is_input() || !is_arrived(io, &port.saved)
            {
                continue;
            }
            let result = self.connect_input(at, io, graph, arrived);
            let shared = &self.inputs[at].shared;
            let presence = match result {
                Ok(()) => {
                    let info = input_info(io, arrived);
                    shared
                        .cell
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .info = Arc::new(info);
                    changed(out, shared.id, ResourceFact::Info);
                    Presence::Live
                }
                Err(_) => Presence::Lost(PortCause::ReopenFailed),
            };
            set_presence(&shared.cell, shared.id, presence, out);
            edits.graph = true;
        }
        for port in &mut self.outputs {
            if port.out.is_some() || !arrived.direction.is_output() || !is_arrived(io, &port.saved)
            {
                continue;
            }
            let presence = match open_device_output(io, &arrived.id, &port.options) {
                Ok(device) => {
                    port.out = Some(match port.parked.take() {
                        Some(stream) => UmpOut::resumed(device, stream),
                        None => UmpOut::of_device(device, rate),
                    });
                    port.refresh(out);
                    Presence::Live
                }
                Err(_) => Presence::Lost(PortCause::ReopenFailed),
            };
            set_presence(&port.shared.cell, port.shared.id, presence, out);
        }
        edits
    }

    /// Sends what the handles asked of the peers.
    fn send_requests(&mut self, io: &mut MidiSession, out: &mut Vec<SessionEvent>) {
        for port in &self.inputs {
            let requests = std::mem::take(
                &mut *port
                    .shared
                    .requests
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            );
            for request in requests {
                // The call checked the port's info; what went since is the
                // request's end, as an event.
                match request {
                    InputRequest::Profile(switch) => {
                        let (device, peer) = (switch.device().clone(), switch.peer());
                        if let Err(error) = io.set_profile(switch) {
                            out.push(
                                MidiReport::CiProfiles {
                                    device,
                                    peer,
                                    change: tutti_midi_hardware::ProfilesChange::NotQueued(error),
                                }
                                .into(),
                            );
                        }
                    }
                    InputRequest::Report(ask) => {
                        let (device, peer) = (ask.device().clone(), ask.peer());
                        if let Err(error) = io.report(ask) {
                            out.push(
                                MidiReport::CiProcess {
                                    device,
                                    peer,
                                    change: tutti_midi_hardware::ProcessChange::NotQueued(error),
                                }
                                .into(),
                            );
                        }
                    }
                }
            }
        }
    }

    /// Steps the negotiations and the MIDI-CI of the open inputs, follows what
    /// they settled, reopens an input whose reader died, and says what changed.
    fn step(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph,
        now: std::time::Instant,
        out: &mut Vec<SessionEvent>,
    ) -> Edits {
        let mut edits = Edits::default();
        let mut touched: Vec<DeviceId> = Vec::new();
        let mut profile_devices: Vec<DeviceId> = Vec::new();
        for polled in io.poll(now) {
            // The output follows what its device settled, at once: the
            // `MidiOut` phase that drains next stamps by it.
            for port in &mut self.outputs {
                if let Some(device) = port.out.as_mut().and_then(UmpOut::device_mut) {
                    device.follow(&polled);
                }
            }
            let device = match &polled {
                tutti_midi_hardware::Polled::Stream(device, _)
                | tutti_midi_hardware::Polled::Ci(device, ..) => device.clone(),
                tutti_midi_hardware::Polled::Profile(device, peer, change) => {
                    if !profile_devices.contains(device) {
                        profile_devices.push(device.clone());
                    }
                    out.push(
                        MidiReport::CiProfiles {
                            device: device.clone(),
                            peer: *peer,
                            change: change.clone(),
                        }
                        .into(),
                    );
                    device.clone()
                }
                tutti_midi_hardware::Polled::Process(device, peer, change) => {
                    out.push(
                        MidiReport::CiProcess {
                            device: device.clone(),
                            peer: *peer,
                            change: change.clone(),
                        }
                        .into(),
                    );
                    device.clone()
                }
                // A kind of report this session does not know is not one.
                _ => continue,
            };
            if !touched.contains(&device) {
                touched.push(device);
            }
        }
        for device in &profile_devices {
            self.sync_mpe_profiles(io, device);
        }
        for port in &self.inputs {
            if port.open.as_ref().is_some_and(|id| touched.contains(id)) {
                port.refresh(io, out);
            }
        }
        for port in &mut self.outputs {
            port.refresh(out);
        }
        // An input whose reader ended on an error while its device stays
        // listed is silent: it is reopened, and reads why when that fails.
        for at in 0..self.inputs.len() {
            let Some(id) = self.inputs[at].open.clone() else {
                continue;
            };
            if io.is_input_connected(&id) {
                continue;
            }
            let Some(device) = io.devices().list().into_iter().find(|d| d.id == id) else {
                continue;
            };
            self.disconnect_input(at, io, graph);
            let result = self.connect_input(at, io, graph, &device);
            let shared = &self.inputs[at].shared;
            if result.is_err() {
                set_presence(
                    &shared.cell,
                    shared.id,
                    Presence::Lost(PortCause::ReaderFailed),
                    out,
                );
            }
            edits.graph = true;
        }
        edits
    }
}

impl OutputPort {
    /// Counts what the device's output refused or lost after taking since the
    /// last call: a device counts its own, which a send's return does not carry.
    fn fold_device_losses(&mut self) {
        let Some(device) = self.out.as_ref().and_then(UmpOut::device) else {
            return;
        };
        let now = device.losses();
        self.losses
            .lost(now.lost.saturating_sub(self.device_seen.lost));
        self.losses
            .refused(now.refused.saturating_sub(self.device_seen.refused));
        self.device_seen = now;
    }

    /// Takes in the output's Protocol and JR Timestamps as they are now.
    fn refresh(&self, out: &mut Vec<SessionEvent>) {
        let Some(device) = self.out.as_ref().and_then(UmpOut::device) else {
            return;
        };
        let config = device.config();
        let mut cell = self
            .shared
            .cell
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if cell.info.config != config || cell.info.device.id != *device.id() {
            cell.info = Arc::new(MidiOutputInfo {
                device: cell.info.device.clone(),
                config,
            });
            changed(out, self.shared.id, ResourceFact::Info);
        }
    }
}

// --- Opening ---

/// The output of device `id`, with the port's options applied.
fn open_device_output(
    io: &MidiSession,
    id: &DeviceId,
    options: &OutputOptions,
) -> Result<tutti_midi_hardware::OpenOutput, MidiPortError> {
    let mut device = io
        .connect_output(id)
        .map_err(|e| MidiPortError::Hardware(Arc::new(e)))?;
    if options.active_sensing {
        device
            .active_sensing()
            .map_err(|e| MidiPortError::Hardware(Arc::new(e)))?;
    }
    Ok(device)
}

impl crate::Midi<'_> {
    /// Opens the input port of `saved` as `id`: the ingest node joins the graph
    /// and the device, when it is present, opens into it.
    pub(crate) fn open_input_port(
        &mut self,
        id: ResourceId,
        saved: DeviceInfo,
    ) -> Result<InputLoaded, MidiPortError> {
        let io = self.io.as_deref_mut().ok_or(MidiPortError::NoHardware)?;
        // A present device is bound when it is surely the saved one, or the one
        // of its name; one of several that could be is for the app to choose.
        let found = match io.devices().resolve(&saved) {
            Resolved::Exact(info) | Resolved::SameName(info) => Some(info),
            Resolved::Ambiguous(candidates) => return Err(MidiPortError::Ambiguous(candidates)),
            Resolved::Missing => None,
        };
        let found = found.filter(|info| info.direction.is_input());
        let epochs = DeviceEpochs::new();
        let layout = self
            .nodes
            .map_or(MidiInputPorts::FIRST_GROUP, |n| n.input_ports);
        let node = tutti_midi_runtime::MidiIngestNode::new()
            .hearing(layout)
            .with_mpe(*self.state.mpe())
            .following(&epochs);
        let (key, controls) = self.graph.add(node);
        let node = key.erase();
        let token = Arc::new(());
        let info = found.clone().unwrap_or_else(|| saved.clone());
        // The cells start closed: a connection reopens them.
        let (writers, cells) = InputWriters::new();
        drop(writers);
        let (ci_writer, ci_refused) = tutti_rt::counter();
        drop(ci_writer);
        let shared = Arc::new(InputShared {
            id,
            cell: Mutex::new(Cell {
                info: Arc::new(input_info(io, &info)),
                presence: Presence::Lost(PortCause::Unplugged),
            }),
            requests: Mutex::new(Vec::new()),
            cells,
            ci_refused,
            ingest: controls.clone(),
        });
        self.state.ports.inputs.push(InputPort {
            token: Arc::downgrade(&token),
            shared: Arc::clone(&shared),
            node,
            controls,
            epochs,
            inlet: None,
            saved,
            open: None,
        });
        let at = self.state.ports.inputs.len() - 1;
        if let Some(info) = found {
            let io = self.io.as_deref_mut().ok_or(MidiPortError::NoHardware)?;
            if let Err(error) = self.state.ports.connect_input(at, io, self.graph, &info) {
                // The port was never loaded: it leaves with the entry's failure.
                self.state.ports.inputs.remove(at);
                let _ = self.graph.remove(node);
                return Err(error);
            }
            let io = self.io.as_deref().ok_or(MidiPortError::NoHardware)?;
            let mut cell = shared.cell.lock().unwrap_or_else(PoisonError::into_inner);
            cell.info = Arc::new(input_info(io, &info));
            cell.presence = Presence::Live;
        }
        self.compile_session_routes();
        Ok(InputLoaded {
            shared,
            node,
            _token: token,
        })
    }

    /// Opens the output port of `saved` as `id`: the output node joins the graph
    /// ([`add_output_node`](Self::add_output_node)) and the device, when it is
    /// present, opens into it.
    pub(crate) fn open_output_port(
        &mut self,
        id: ResourceId,
        saved: DeviceInfo,
        options: OutputOptions,
    ) -> Result<OutputLoaded, MidiPortError> {
        let io = self.io.as_deref().ok_or(MidiPortError::NoHardware)?;
        let found = match io.devices().resolve(&saved) {
            Resolved::Exact(info) | Resolved::SameName(info) => Some(info),
            Resolved::Ambiguous(candidates) => return Err(MidiPortError::Ambiguous(candidates)),
            Resolved::Missing => None,
        };
        let found = found.filter(|info| info.direction.is_output());
        let device = match &found {
            Some(info) => Some(open_device_output(io, &info.id, &options)?),
            None => None,
        };
        let rate = self.graph.prepared().sample_rate();
        let (node, tap) = self.add_output_node();
        let config = device.as_ref().map(tutti_midi_hardware::OpenOutput::config);
        let info = found.clone().unwrap_or_else(|| saved.clone());
        let token = Arc::new(());
        let live = device.is_some();
        let (losses, readers) = loss_cells();
        let shared = Arc::new(OutputShared {
            id,
            losses: readers,
            cell: Mutex::new(Cell {
                info: Arc::new(MidiOutputInfo {
                    device: info,
                    config: config.unwrap_or(StreamConfig {
                        protocol: tutti_ump::Protocol::Midi2,
                        rx_jr: false,
                        tx_jr: false,
                    }),
                }),
                presence: if live {
                    Presence::Live
                } else {
                    Presence::Lost(PortCause::Unplugged)
                },
            }),
        });
        self.state.ports.outputs.push(OutputPort {
            token: Arc::downgrade(&token),
            shared: Arc::clone(&shared),
            node,
            tap,
            saved,
            options,
            losses,
            device_seen: Losses::NONE,
            out: device.map(|device| UmpOut::of_device(device, rate)),
            parked: None,
        });
        Ok(OutputLoaded {
            shared,
            node,
            _token: token,
        })
    }
}

// --- Draining the outputs ---

impl Ports {
    /// Each output port's tap on one scale of frames, the rate it was opened
    /// at: where the graph has been re-prepared at another, drains the old
    /// reader and reopens that one tap. Whether any reopened.
    pub(crate) fn reopen(
        &mut self,
        graph: &mut crate::graph::Graph,
        timing: Option<&crate::midi::OutputTiming>,
    ) -> bool {
        let mut edited = false;
        for port in &mut self.outputs {
            if port.tap.rate() != graph.prepared().sample_rate() {
                crate::midi::drain_events(
                    &mut port.tap,
                    port.out.as_mut(),
                    &mut port.losses,
                    timing,
                );
                if let Some(tap) = graph.taps().reopen_event(port.tap.id()) {
                    port.tap = tap;
                    let rate = graph.prepared().sample_rate();
                    if let Some(out) = port.out.as_mut() {
                        out.set_rate(rate);
                    }
                    port.parked = port.parked.map(|stream| stream.at_rate(rate));
                    edited = true;
                }
            }
        }
        edited
    }

    /// Each output port's tap, read dry and routed to its device.
    pub(crate) fn drain_outputs(&mut self, timing: Option<&crate::midi::OutputTiming>) {
        for port in &mut self.outputs {
            crate::midi::drain_events(&mut port.tap, port.out.as_mut(), &mut port.losses, timing);
            port.fold_device_losses();
        }
    }

    /// Sends `batch` to every output port, each event on its frame; a port with
    /// no device counts them refused.
    pub(crate) fn route(
        &mut self,
        batch: &[(tutti_types::Frame, tutti_ump::Ump)],
        timing: Option<&crate::midi::OutputTiming>,
    ) {
        for port in &mut self.outputs {
            crate::midi::route_batch(port.out.as_mut(), &mut port.losses, timing, batch);
            port.fold_device_losses();
        }
    }

    /// Counts `n` events as refused by every output port.
    pub(crate) fn refuse(&mut self, n: u64) {
        for port in &mut self.outputs {
            port.losses.refused(n);
        }
    }
}

#[cfg(test)]
mod tests;
