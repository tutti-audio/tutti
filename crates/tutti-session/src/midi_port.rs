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
//! An input is fed to the graph through an input node of its own, made when
//! the entry loads: it releases the notes its device holds before it goes, and
//! survives an unplug and a replug, so what is wired to it stays wired. An
//! output is a [`MidiOutNode`](tutti_midi_runtime::MidiOutNode) of its own,
//! whose events the session sends to the device each update.

use std::sync::{Arc, Mutex, Weak};

use tutti_graph::{EventTapReader, Losses, NodeKey};
use tutti_midi_device::{DeviceId, DeviceInfo, Learned, Match, Resolved, SavedDevice};
use tutti_midi_hardware::{
    Answer, DeviceStream, InputFault, MidiSession, PeerProfiles, ProfileSwitch, PropertyAsk,
    PropertyPeer, ReportAsk,
};
use tutti_midi_runtime::{
    DeviceGeneration, GenerationWriter, InputReaders, InputWriters, JrStream, MidiInputControls,
    MidiInputPorts,
};
use tutti_rt::{Counter, Gauge, LockExt};
use tutti_ump::StreamConfig;
use tutti_units::{Resyncs, Seconds};

use crate::midi::{LossReaders, LossWriters, UmpOut, loss_cells};
use crate::midi_ci::{
    EndSubscription, GetProperty, ListedPeer, Pending, PortLink, ProfilePeer, Reply, ReportPeer,
    RequestReport, ResourcePeer, SetProperty, Subscribe, SwitchProfile,
};
use crate::pool::{Declared, HandleError, Life, LoadError, PoolEntry, PoolLoad};
use crate::{
    EventInPort, EventOutPort, MidiReport, Peer, PeerError, Presence, Resource, ResourceFact,
    ResourceId, Session, SessionEvent,
};

/// Why a port whose input could not be reopened is lost: the fault the reader
/// died of, or [`PortCause::ReopenFailed`] when the input was not held and read
/// none.
fn reader_loss(fault: Option<InputFault>) -> PortCause {
    fault.map_or(PortCause::ReopenFailed, PortCause::ReaderFailed)
}

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
    /// The reader of an open input died while its device stayed listed, for
    /// the [`InputFault`] it carries. The session reopens it at once, and the
    /// port reads this only when that failed; it is tried again when its device
    /// next appears in the device list.
    ReaderFailed(InputFault),
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

/// The session has no OS MIDI layer
/// ([`SessionBuilder::midi_hardware`](crate::SessionBuilder::midi_hardware)
/// was not asked for): the reason of [`MidiPortError::NoHardware`] and
/// [`MidiPolicyError::NoHardware`](crate::MidiPolicyError::NoHardware).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("the session has no OS MIDI layer; build it with `midi_hardware(options)`")]
pub struct NoHardware;

/// Why a MIDI port did not load.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MidiPortError {
    /// The session has no OS MIDI layer.
    #[error(transparent)]
    NoHardware(#[from] NoHardware),
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
    Hardware(Arc<tutti_midi_hardware::MidiHardwareError>),
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
    peers: Arc<[ListedPeer]>,
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

    /// The MIDI-CI peers found on the input, by [`PeerId`](tutti_midi_hardware::PeerId) (Group then MUID):
    /// what a device that answered the session's Discovery declared, each with
    /// the handles of what it offers ([`ListedPeer`]).
    #[must_use]
    pub fn peers(&self) -> &[ListedPeer] {
        &self.peers
    }

    /// The Profile tables of the peers that declared Profile Configuration.
    pub(crate) fn profile_tables(&self) -> impl Iterator<Item = &PeerProfiles> {
        self.peers
            .iter()
            .filter_map(|peer| peer.profiles().map(ProfilePeer::profiles))
    }

    /// The peers that support MIDI Message Report.
    pub(crate) fn reporters(&self) -> impl Iterator<Item = &tutti_midi_hardware::Reporter> {
        self.peers
            .iter()
            .filter_map(|peer| peer.report().map(ReportPeer::peer))
    }

    /// The peers that answered Property Exchange Capabilities.
    pub(crate) fn property_peers(&self) -> impl Iterator<Item = &PropertyPeer> {
        self.peers
            .iter()
            .filter_map(|peer| peer.resources().map(ResourcePeer::peer))
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
    requests: Mutex<Inbox>,
    /// What the handles its info lists make requests through.
    link: PortLink,
    /// The cells a connection's device input writes: the latest correction the
    /// port's clock map made, its re-anchors and the events its inlet had no
    /// room for. Made once with the entry; a connection writes them and closes
    /// them when it ends, and the counts are cumulative across connections.
    cells: InputReaders,
    /// What the port's input node lost: translated events its mailbox had no
    /// room for. The node outlives every connection.
    input: MidiInputControls,
}

/// What a handle asked of the device, for the session to send at its next poll,
/// with the ticket of the job that waits for the answer.
pub(crate) enum InputRequest {
    Profile(ProfileSwitch, Reply<SwitchProfile>),
    Report(ReportAsk, Reply<RequestReport>),
    Get(PropertyAsk, Reply<GetProperty>),
    Set(PropertyAsk, Reply<SetProperty>),
    Subscribe(PropertyAsk, Reply<Subscribe>),
    Unsubscribe(PropertyAsk, Reply<EndSubscription>),
}

impl InputRequest {
    /// The request ends as `why`.
    fn end(self, why: Ended) {
        match self {
            Self::Profile(_, ticket) => ticket.land(Err(why.error())),
            Self::Report(_, ticket) => ticket.land(Err(why.error())),
            Self::Get(_, ticket) => ticket.land(Err(why.error())),
            Self::Set(_, ticket) => ticket.land(Err(why.error())),
            Self::Subscribe(_, ticket) => ticket.land(Err(why.error())),
            Self::Unsubscribe(_, ticket) => ticket.land(Err(why.error())),
        }
    }
}

/// Why a request ended without its peer's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// The device is away, or the session is gone.
    Disconnected,
    /// The entry was released or swapped.
    Released,
}

impl Ended {
    /// The error a job of any kind ends with.
    pub(crate) const fn error<E>(self) -> PeerError<E> {
        match self {
            Self::Disconnected => PeerError::Disconnected,
            Self::Released => PeerError::Released,
        }
    }
}

/// Whether the session drains a port's requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Gate {
    /// The device is open: a request is queued for the next poll.
    Open,
    /// The device is away, or the session is gone: a request ends at once.
    #[default]
    Away,
    /// The entry was released or swapped, for good.
    Released,
}

/// The requests the handles queued for the session's next poll, with whether
/// the session will drain them. The gate and the queue share one lock, so a
/// request is either queued where it will be drained or ended at the call.
#[derive(Default)]
struct Inbox {
    gate: Gate,
    queue: Vec<InputRequest>,
}

impl InputShared {
    /// Queues `request` with its `ticket` when the port can take it now and
    /// `lists` says the peer and what is asked of it are still listed in the
    /// port's info; otherwise ends the job at once: [`PeerError::Released`]
    /// once the entry is released or swapped, [`PeerError::Disconnected`] while
    /// the device is away or when `lists` is false, since the peer or the
    /// Profile it lists has gone.
    pub(crate) fn push<J, R, E>(
        &self,
        request: R,
        ticket: Reply<J>,
        lists: impl FnOnce(&MidiInputInfo, &R) -> bool,
        make: impl FnOnce(R, Reply<J>) -> InputRequest,
    ) where
        J: crate::JobKind<Error = PeerError<E>>,
    {
        let mut inbox = self.requests.lock_recover();
        let refused = match inbox.gate {
            Gate::Released => Some(Ended::Released),
            Gate::Away => Some(Ended::Disconnected),
            Gate::Open if !lists(&self.read(|cell| Arc::clone(&cell.info)), &request) => {
                Some(Ended::Disconnected)
            }
            Gate::Open => None,
        };
        match refused {
            Some(why) => {
                // The job lands, and takes the landed queue's lock, with the
                // inbox's lock released.
                drop(inbox);
                ticket.land(Err(why.error()));
            }
            None => inbox.queue.push(make(request, ticket)),
        }
    }

    /// The session takes what the handles queued.
    fn drain(&self) -> Vec<InputRequest> {
        std::mem::take(&mut self.requests.lock_recover().queue)
    }

    /// The device is open: requests are taken again.
    fn open(&self) {
        let mut inbox = self.requests.lock_recover();
        if inbox.gate == Gate::Away {
            inbox.gate = Gate::Open;
        }
    }

    /// Ends what the handles queued as `why`, and refuses what they ask next:
    /// as [`Gate::Released`] for good, or [`Gate::Away`] until the device is open
    /// again.
    fn shut(&self, gate: Gate, why: Ended) {
        let queued = {
            let mut inbox = self.requests.lock_recover();
            if inbox.gate != Gate::Released {
                inbox.gate = gate;
            }
            std::mem::take(&mut inbox.queue)
        };
        for request in queued {
            request.end(why);
        }
    }

    fn read<R>(&self, read: impl FnOnce(&Cell<MidiInputInfo>) -> R) -> R {
        read(&self.cell.lock_recover())
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
        read(&self.cell.lock_recover())
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

impl<S: tutti_buffers::Float> PoolEntry<S> for MidiInputEntry {
    type Identity = DeviceInfo;
    type Loaded = InputLoaded;
    type Handle = MidiInput;

    fn identity(&self) -> DeviceInfo {
        self.saved.clone()
    }

    fn load(&self) -> PoolLoad<InputLoaded, S> {
        let saved = self.saved.clone();
        PoolLoad::on_session(move |cx| {
            let id = cx.mint_resource_id();
            let ids = cx.resource_ids();
            cx.midi()
                .open_input_port(id, ids, saved)
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

impl<S: tutti_buffers::Float> PoolEntry<S> for MidiOutputEntry {
    type Identity = (DeviceInfo, OutputOptions);
    type Loaded = OutputLoaded;
    type Handle = MidiOutput;

    fn identity(&self) -> (DeviceInfo, OutputOptions) {
        (self.saved.clone(), self.options.clone())
    }

    fn load(&self) -> PoolLoad<OutputLoaded, S> {
        let saved = self.saved.clone();
        let options = self.options.clone();
        PoolLoad::on_session(move |cx| {
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
    /// The port's input node: the node to wire from and to follow. Its event
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
    /// had no room for (a stalled audio callback with a busy controller),
    /// [`evicted`](Losses::evicted) are events its inlet dropped from its waiting
    /// list for room, [`late`](Losses::late) are events the inlet emitted at the
    /// first frame of the block that found them, their frame having begun (they
    /// sound, late), and [`refused`](Losses::refused) are events the inlet's
    /// port refused, translated events its input node's mailbox had no room for
    /// in one block, and the MIDI-CI messages its conversation sent that the
    /// device's output refused (a refused message is
    /// not sent again, so the count is how many a peer may never have heard).
    /// The CI count is the input's because the input owns the conversation, its
    /// peers and Profiles ([`MidiInputInfo::peers`]); the session's MIDI
    /// output never carries it.
    #[must_use]
    pub fn losses(&self) -> Losses {
        self.shared.cells.lost.get()
            + self.shared.input.losses()
            + self.shared.cells.inlet.get()
            + self.shared.cells.ci_refused.get()
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

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// The input port of the `midi_input` entry under `key`.
    ///
    /// # Errors
    ///
    /// [`HandleError::NotPooled`] when the key is not declared (never, or
    /// released); [`HandleError::WrongKind`] when it is another kind of entry;
    /// [`HandleError::NotLoaded`] before the load lands, or after it failed (an
    /// ambiguous saved device: the state carries the candidates).
    pub fn midi_input<P>(&self, key: &P) -> Result<MidiInput, HandleError<P>>
    where
        P: crate::AppKey,
    {
        self.pool.entries.handle::<P, MidiInputEntry>(key)
    }

    /// The output port of the `midi_output` entry under `key`.
    ///
    /// # Errors
    ///
    /// As [`midi_input`](Self::midi_input).
    pub fn midi_output<P>(&self, key: &P) -> Result<MidiOutput, HandleError<P>>
    where
        P: crate::AppKey,
    {
        self.pool.entries.handle::<P, MidiOutputEntry>(key)
    }
}

// --- The session's table of ports ---

/// One input port as the session keeps it.
struct InputPort {
    /// Upgrades while the entry's loaded value lives.
    token: Weak<()>,
    shared: Arc<InputShared>,
    node: NodeKey,
    controls: MidiInputControls,
    /// The one writer of the node's generation.
    writer: GenerationWriter,
    /// The node's half of it.
    generation: DeviceGeneration,
    /// The inlet node of the open connection, in the graph.
    inlet: Option<NodeKey>,
    /// What the entry was declared with: what a returning device is matched on.
    saved: DeviceInfo,
    /// The device the port holds open.
    open: Option<DeviceId>,
    /// The requests sent to the device's peers that they have not answered.
    pending: Pending,
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
    generation: DeviceGeneration,
}

/// The ports of the session, and what they are opened with.
#[derive(Default)]
pub(crate) struct Ports {
    inputs: Vec<InputPort>,
    outputs: Vec<OutputPort>,
    retiring: Vec<Retiring>,
    /// The notices of leases whose port went, for the next step to report.
    notices: Vec<SessionEvent>,
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
    let mut cell = cell.lock_recover();
    if cell.presence != presence {
        cell.presence = presence;
        changed(out, id, ResourceFact::Presence);
    }
}

/// The snapshot of the open input `device`, as the OS layer reads it now.
fn input_info(
    io: &MidiSession,
    port: ResourceId,
    device: &DeviceInfo,
    link: &PortLink,
    pending: &Pending,
) -> MidiInputInfo {
    let learned = io.learned(&device.id);
    let profiles = io.profiles(&device.id);
    let reporters = io.report_peers(&device.id);
    let properties = io.property_peers(&device.id);
    let peers = io
        .ci_peers(&device.id)
        .iter()
        .map(|info| {
            let id = info.id();
            ListedPeer::new(
                pending.peer_id(id),
                port,
                SavedDevice::new(device.clone(), learned),
                info.clone(),
                profiles
                    .iter()
                    .find(|p| p.group() == id.group && p.muid() == id.muid)
                    .map(|p| ProfilePeer::new(link.clone(), p.clone())),
                reporters
                    .iter()
                    .find(|p| p.group() == id.group && p.muid() == id.muid)
                    .map(|p| ReportPeer::new(link.clone(), p.clone())),
                properties
                    .iter()
                    .find(|p| p.group() == id.group && p.muid() == id.muid)
                    .map(|p| ResourcePeer::new(link.clone(), p.clone(), pending.bodies(id))),
            )
        })
        .collect();
    MidiInputInfo {
        device: device.clone(),
        stream: io.stream(&device.id),
        learned,
        peers,
    }
}

impl Drop for InputPort {
    /// The session ended: the requests its handles queued or waited on end, and
    /// a handle that outlives it ends what it asks at the call.
    fn drop(&mut self) {
        self.end_requests(Gate::Away, Ended::Disconnected);
    }
}

impl InputPort {
    /// Every request of the port, queued or sent, ends as `why`, and the port
    /// takes none until its device is open again: for good with
    /// [`Gate::Released`].
    fn end_requests(&mut self, gate: Gate, why: Ended) {
        self.shared.shut(gate, why);
        self.pending.end(why);
    }

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
        let info = input_info(
            io,
            self.shared.id,
            &device,
            &self.shared.link,
            &self.pending,
        );
        let mut cell = self.shared.cell.lock_recover();
        let same = cell.info.device == info.device
            && cell.info.stream == info.stream
            && cell.info.learned == info.learned
            && *cell.info.peers == *info.peers;
        if !same {
            cell.info = Arc::new(info);
            changed(out, self.shared.id, ResourceFact::Info);
        }
    }
}

impl Ports {
    /// The input nodes of the ports, and of those going, for the routes to read.
    pub(crate) fn input_nodes(&self) -> Vec<NodeKey> {
        // A node going keeps its routes until it is taken out: its release goes
        // out through them.
        self.inputs
            .iter()
            .map(|port| port.node)
            .chain(self.retiring.iter().map(|going| going.node))
            .collect()
    }

    /// Whether `node` is the input node of an input port.
    pub(crate) fn is_input_node(&self, node: NodeKey) -> bool {
        self.inputs.iter().any(|port| port.node == node)
    }

    /// Takes the MPE Profile zones the peers of `device` have enabled (M2-120) as
    /// what its input node follows: the zones of every peer on a Group
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
    /// joins the graph, wired to the input node, and a device arrives there.
    fn connect_input<F: tutti_buffers::Float>(
        &mut self,
        at: usize,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
        info: &DeviceInfo,
    ) -> Result<(), MidiPortError> {
        let port = &mut self.inputs[at];
        // The previous connection's input has dropped with its device being
        // closed; one that still holds the cells is refused, never connected
        // without readings.
        let clock = InputWriters::reopen(&port.shared.cells)
            .ok_or_else(|| MidiPortError::CellsHeld(info.id.clone()))?;
        match io
            .connect_input_with(&info.id, clock)
            .map_err(|e| MidiPortError::Hardware(Arc::new(e)))?
        {
            tutti_midi_hardware::Connected::Opened(inlet) => {
                let key = graph.add_event_inlet(inlet).erase();
                // The input node has the one event input, and the inlet was just
                // added. A host that removed the input node itself leaves it nothing to feed.
                let _ =
                    graph.set_sources(EventInPort::new(port.node, 0), [EventOutPort::new(key, 0)]);
                port.inlet = Some(key);
                port.writer.arrive();
                port.open = Some(info.id.clone());
                port.shared.open();
                Ok(())
            }
            tutti_midi_hardware::Connected::AlreadyOpen => {
                Err(MidiPortError::InUse(info.id.clone()))
            }
        }
    }

    /// Closes the open input port at `at`: the device's notes are released by
    /// the input node, and the inlet leaves the graph.
    fn disconnect_input<F: tutti_buffers::Float>(
        &mut self,
        at: usize,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
    ) {
        let port = &mut self.inputs[at];
        if let Some(id) = port.open.take() {
            io.disconnect_input(&id);
        }
        if let Some(key) = port.inlet.take() {
            // A host that removed the inlet itself leaves nothing to take.
            let _ = graph.remove(key);
        }
        port.writer.depart();
        port.controls.clear_mpe_profiles();
        // The device's peers went with its connection.
        port.end_requests(Gate::Away, Ended::Disconnected);
    }

    /// The step of the update's `Phase::Poll` for the ports: retires those whose
    /// entry ended, takes in a device that went or came, sends what the handles
    /// asked, and reads what the devices said. What was edited.
    pub(crate) fn poll<F: tutti_buffers::Float>(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
        rate: tutti_units::SampleRate,
        events: Vec<tutti_midi_device::DeviceEvent>,
        now: tutti_platform::Instant,
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
        self.send_requests(io);
        edits |= self.step(io, graph, now, out);
        edits
    }

    /// Ends the ports whose entry was released or swapped, and takes out the
    /// input nodes that have released what their devices held.
    fn retire<F: tutti_buffers::Float>(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
    ) -> Edits {
        let mut edits = Edits::default();
        let mut at = 0;
        while at < self.inputs.len() {
            if self.inputs[at].is_held() {
                at += 1;
                continue;
            }
            self.inputs[at].end_requests(Gate::Released, Ended::Released);
            self.disconnect_input(at, io, graph);
            let mut port = self.inputs.remove(at);
            self.notices.extend(port.pending.take_notices());
            self.retiring.push(Retiring {
                node: port.node,
                generation: port.generation.clone(),
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
            let done = going.generation.is_idle();
            if done {
                // A host that removed the node itself leaves nothing to take.
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
    fn lose<F: tutti_buffers::Float>(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
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
    fn restore<F: tutti_buffers::Float>(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
        rate: tutti_units::SampleRate,
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
                    let info = input_info(
                        io,
                        shared.id,
                        arrived,
                        &shared.link,
                        &self.inputs[at].pending,
                    );
                    shared.cell.lock_recover().info = Arc::new(info);
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

    /// Sends what the handles asked of the peers: a request the device took
    /// waits for its answer, and one it cannot take ends the job.
    fn send_requests(&mut self, io: &mut MidiSession) {
        for port in &mut self.inputs {
            for request in port.shared.drain() {
                match request {
                    InputRequest::Profile(switch, ticket) => {
                        port.pending.switch(io, switch, ticket);
                    }
                    InputRequest::Report(ask, ticket) => port.pending.report(io, ask, ticket),
                    InputRequest::Get(ask, ticket) => port.pending.get(io, ask, ticket),
                    InputRequest::Set(ask, ticket) => port.pending.set(io, ask, ticket),
                    InputRequest::Subscribe(ask, ticket) => {
                        port.pending.subscribe(io, ask, ticket);
                    }
                    InputRequest::Unsubscribe(ask, ticket) => {
                        port.pending.unsubscribe(io, ask, ticket);
                    }
                }
            }
        }
    }

    /// The open input port that holds `device`.
    fn port_of(&mut self, device: &DeviceId) -> Option<&mut InputPort> {
        self.inputs
            .iter_mut()
            .find(|port| port.open.as_ref() == Some(device))
    }

    /// Lands what a peer of `device` answered on the requests waiting for it.
    fn answer(
        &mut self,
        io: &mut MidiSession,
        device: &DeviceId,
        peer: tutti_midi_hardware::PeerId,
        answer: &Answer,
    ) {
        if let Some(port) = self.port_of(device) {
            port.pending.answer(io, &port.shared.link, peer, answer);
        }
    }

    /// Steps the negotiations and the MIDI-CI of the open inputs, follows what
    /// they settled, reopens an input whose reader died, and says what changed.
    fn step<F: tutti_buffers::Float>(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
        now: tutti_platform::Instant,
        out: &mut Vec<SessionEvent>,
    ) -> Edits {
        let mut edits = Edits::default();
        out.append(&mut self.notices);
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
                tutti_midi_hardware::Polled::Answer(device, peer, answer) => {
                    self.answer(io, device, *peer, answer);
                    continue;
                }
                tutti_midi_hardware::Polled::Profile(device, peer, change) => {
                    if matches!(change, tutti_midi_hardware::ProfilesChange::Dropped)
                        && let Some(port) = self.port_of(device)
                    {
                        port.pending.lose_switches(*peer);
                    }
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
                    if matches!(change, tutti_midi_hardware::ProcessChange::Dropped)
                        && let Some(port) = self.port_of(device)
                    {
                        port.pending.lose_reports(*peer);
                    }
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
                tutti_midi_hardware::Polled::Property(device, peer, change) => {
                    if matches!(change, tutti_midi_hardware::PropertyChange::Dropped)
                        && let Some(port) = self.port_of(device)
                    {
                        port.pending.lose_properties(*peer);
                    }
                    if let tutti_midi_hardware::PropertyChange::Event(event) = change
                        && let Some(port) = self.port_of(device)
                    {
                        port.pending.event(*peer, event);
                    }
                    // An event of the peer (its ResourceList, whose Get held one of
                    // its requests) may have made room for a call that waits.
                    if let Some(port) = self.port_of(device) {
                        port.pending.make_room_everywhere(io);
                    }
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
        for port in &mut self.inputs {
            // A Subscription's data that changed, or whose lease dropped, is a
            // change of the info too.
            out.extend(port.pending.take_notices());
            let held = port.pending.take_changed();
            if port
                .open
                .as_ref()
                .is_some_and(|id| held || touched.contains(id))
            {
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
            // Not held at all reads no fault: a failed reopen is then the
            // port's own.
            let fault = io.input_fault(&id);
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
                    Presence::Lost(reader_loss(fault)),
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
        let mut cell = self.shared.cell.lock_recover();
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

impl<F: tutti_buffers::Float> crate::Midi<'_, F> {
    /// Opens the input port of `saved` as `id`: the input node joins the graph
    /// and the device, when it is present, opens into it.
    pub(crate) fn open_input_port(
        &mut self,
        id: ResourceId,
        ids: crate::peer::ResourceIds,
        saved: DeviceInfo,
    ) -> Result<InputLoaded, MidiPortError> {
        let io = self
            .io
            .as_deref_mut()
            .ok_or(MidiPortError::from(NoHardware))?;
        // A present device is bound when it is surely the saved one, or the one
        // of its name; one of several that could be is for the app to choose.
        let found = match io.devices().resolve(&saved) {
            Resolved::Exact(info) | Resolved::SameName(info) => Some(info),
            Resolved::Ambiguous(candidates) => return Err(MidiPortError::Ambiguous(candidates)),
            Resolved::Missing => None,
        };
        let found = found.filter(|info| info.direction.is_input());
        let (writer, generation) = DeviceGeneration::new();
        let layout = self
            .nodes
            .map_or(MidiInputPorts::FIRST_GROUP, |n| n.input_ports);
        let node = tutti_midi_runtime::MidiInputNode::new()
            .hearing(layout)
            .mpe(self.state.mpe())
            .following(&generation);
        let (key, controls) = self.graph.add(node);
        let node = key.erase();
        let token = Arc::new(());
        let info = found.clone().unwrap_or_else(|| saved.clone());
        // The cells start closed: a connection reopens them.
        let (writers, cells) = InputWriters::new();
        drop(writers);
        let landed = self.jobs.queue();
        let input = controls.clone();
        let pending = Pending::new(ids);
        let shared = Arc::new_cyclic(|weak| {
            let link = PortLink::new(weak.clone(), landed);
            InputShared {
                id,
                cell: Mutex::new(Cell {
                    info: Arc::new(input_info(io, id, &info, &link, &pending)),
                    presence: Presence::Lost(PortCause::Unplugged),
                }),
                requests: Mutex::new(Inbox::default()),
                link,
                cells,
                input,
            }
        });
        self.state.ports.inputs.push(InputPort {
            token: Arc::downgrade(&token),
            shared: Arc::clone(&shared),
            node,
            controls,
            writer,
            generation,
            inlet: None,
            saved,
            open: None,
            pending,
        });
        let at = self.state.ports.inputs.len() - 1;
        if let Some(info) = found {
            let io = self
                .io
                .as_deref_mut()
                .ok_or(MidiPortError::from(NoHardware))?;
            if let Err(error) = self.state.ports.connect_input(at, io, self.graph, &info) {
                // The port was never loaded: it leaves with the entry's failure.
                self.state.ports.inputs.remove(at);
                let removed = self.graph.remove(node);
                // The input node was added in this call, so it is there.
                debug_assert!(removed.is_ok());
                return Err(error);
            }
            let io = self.io.as_deref().ok_or(MidiPortError::from(NoHardware))?;
            let mut cell = shared.cell.lock_recover();
            cell.info = Arc::new(input_info(
                io,
                shared.id,
                &info,
                &shared.link,
                &self.state.ports.inputs[at].pending,
            ));
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
        let io = self.io.as_deref().ok_or(MidiPortError::from(NoHardware))?;
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
    pub(crate) fn reopen<F: tutti_buffers::Float>(
        &mut self,
        graph: &mut crate::graph::Graph<F>,
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
}

#[cfg(test)]
mod tests;
