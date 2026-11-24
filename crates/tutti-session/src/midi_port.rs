//! MIDI ports (feature `midi-hardware`): [`Session::open_midi_input`] and
//! [`Session::open_midi_output`] begin opening a saved device, and
//! [`Session::apply`] returns a [`MidiInput`] or a [`MidiOutput`] once the
//! wait for the OS MIDI layer has resolved.
//!
//! The OS layer's background work (an input's pump, the device watch, Active
//! Sensing, the legacy poll) runs on tutti's own thread, whether or not the app calls
//! [`update`](Session::update).
//!
//! A port needs no device to exist. A saved device that is absent opens at
//! once and reads [`Presence::Lost`] with [`PortCause::Unplugged`]; the session
//! opens it in place, behind the same handle, when the device arrives and is
//! the one an open would bind (surely the saved device, or the only present one
//! of its name). Only a saved device that several present devices could be
//! fails the open, with the candidates ([`MidiPortError::Ambiguous`]).
//!
//! A handle is a reference-counted claim on the port: clones share it, and
//! the port closes when the last clone is dropped. The session tracks its open
//! ports by a weak reference to that claim, so a dropped handle needs no
//! release call. An input closes at the session's next
//! [`update`](Session::update) or at the next open, whichever comes first, and
//! its node goes once it has released its device's notes. An output retires
//! the same way: its connection and Active Sensing
//! thread end after the graph has rendered past the drop, and a device
//! the dropped port held can be opened by the open that follows.
//!
//! An input is fed to the graph through an input node of its own, made when
//! the port opens: it releases the notes its device holds before it goes, and
//! survives an unplug and a replug, so what is wired to it stays wired. An
//! output is a [`MidiOutNode`](tutti_midi_runtime::MidiOutNode) of its own,
//! whose events the session sends to the device each update.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tutti_graph::{EventTapReader, NodeKey};
use tutti_midi_ci::{Nak, PeData, PeStatus, ProfileState};
use tutti_midi_device::{DeviceInfo, Learned, Match, MidiDeviceId, Resolved, SavedDevice};
use tutti_midi_hardware::{
    Answer, CiPeerInfo, DeviceStream, InputFault, MidiSession, OpenOutput, PeerProfiles,
    ProfileSwitch, PropertyAsk, PropertyPeer, ReportAsk, Start, Started,
};
use tutti_midi_runtime::{
    DeviceGeneration, GenerationWriter, InputReaders, InputWriters, JrStream, MidiInputControls,
    MidiInputPorts,
};
use tutti_rt::{Bell, Counter, Gauge, Mutex};
use tutti_ump::StreamConfig;
use tutti_units::Losses;
use tutti_units::{Resyncs, Seconds};

use crate::apply::{Cut, Sealed};
use crate::midi::{
    Claim, Departure, Destination, Held, LossReaders, LossWriters, Retiring, Sweepable, UmpOut,
    loss_cells,
};
use crate::midi_ci::{
    ListedPeer, Pending, PortLink, ProfilePeer, ReplySender, ReportPeer, ResourcePeer,
    Subscription, at_peer, resolve,
};
use crate::{
    EventInputPort, EventOutputPort, MidiEvent, Peer, PeerError, PeerEvent, PeerResourceId,
    Presence, Resource, ResourceFact, Session, SessionEvent,
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
    InUse(MidiDeviceId),
    /// The device's previous connection still holds the port's cells (timing and
    /// losses), so the new one would report nothing.
    #[error("the previous connection of {0:?} still holds the port's cells")]
    CellsHeld(MidiDeviceId),
    /// The OS layer refused to open the device.
    #[error("MIDI hardware: {0}")]
    Hardware(Arc<tutti_midi_hardware::MidiHardwareError>),
    /// The open is another session's: its wait was made by a session other than
    /// the one applying it.
    #[error("the open was made by another session")]
    Stale,
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

    /// Whether `other` says the same of the input.
    fn same_as(&self, other: &Self) -> bool {
        self.device == other.device
            && self.stream == other.stream
            && self.learned == other.learned
            && *self.peers == *other.peers
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

/// An input port's record: shared by its handles and the session's
/// table.
///
/// Lock order: `pending`, then `inbox`. A Subscription lease dropped under
/// `pending` queues its end through `inbox`; nothing takes `pending` while
/// holding `inbox`, and no path holds both for more than that.
pub(crate) struct InputShared {
    id: PeerResourceId,
    inbox: Mutex<Inbox>,
    /// The requests sent to the device's peers that they have not answered.
    /// Resolved by the session's MIDI-CI task as the peers answer.
    pending: Mutex<Pending>,
    /// Rung when a handle queues a request: the MIDI-CI task sends it.
    bell: Bell,
    /// What the handles its info lists make requests through.
    link: PortLink,
    /// The cells a connection's device input writes: the latest correction the
    /// port's clock map made, its re-anchors and the events its inlet had no
    /// room for. Made once with the port; a connection writes them and closes
    /// them when it ends, and the counts are cumulative across connections.
    cells: InputReaders,
    /// What the port's input node lost: translated events its mailbox had no
    /// room for. The node outlives every connection.
    input: MidiInputControls,
}

/// What a handle asked of the device, for the session's MIDI-CI task to send,
/// with the `ReplySender` that resolves the future that asked. The end of a
/// Subscription asks for no answer.
pub(crate) enum InputRequest {
    Profile(ProfileSwitch, ReplySender<ProfileState, Nak>),
    Report(ReportAsk, ReplySender<(), Nak>),
    Get(PropertyAsk, ReplySender<PeData>),
    Set(PropertyAsk, ReplySender<PeStatus>),
    Subscribe(PropertyAsk, ReplySender<Subscription>),
    Unsubscribe(PropertyAsk),
}

impl InputRequest {
    /// The request ends as `why`.
    fn end(self, why: Shut) {
        match self {
            Self::Profile(_, reply) => resolve(reply, Err(why.error())),
            Self::Report(_, reply) => resolve(reply, Err(why.error())),
            Self::Get(_, reply) => resolve(reply, Err(why.error())),
            Self::Set(_, reply) => resolve(reply, Err(why.error())),
            Self::Subscribe(_, reply) => resolve(reply, Err(why.error())),
            Self::Unsubscribe(_) => {}
        }
    }
}

/// Why a port takes no request, and with what an unanswered request ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shut {
    /// The device is away, or the session is gone.
    Away,
    /// Every handle of the port was dropped, for good.
    Released,
}

impl Shut {
    /// The error a request of any kind ends with.
    pub(crate) const fn error<E>(self) -> PeerError<E> {
        match self {
            Self::Away => PeerError::Disconnected,
            Self::Released => PeerError::Released,
        }
    }
}

/// Whether an input takes requests, and for which device.
enum Door {
    /// The device is open: a request is queued for the next poll.
    Open(MidiDeviceId),
    /// A request ends at once, as `Shut` says.
    Closed(Shut),
}

/// What the handles and the session's MIDI-CI task read of an input port, under
/// one lock: whether it takes requests and for which device, the snapshot those
/// requests are checked against, its presence, and the requests queued for the
/// session's next poll.
///
/// A connection opens the door, replaces the info and sets the presence in one
/// step ([`InputShared::open`]), so a request is never checked against the info
/// of a connection the door no longer belongs to, and is either queued where the
/// session will drain it or ended at the call.
struct Inbox {
    door: Door,
    info: Arc<MidiInputInfo>,
    /// A published copy of the port's link, for the handles on other threads:
    /// written only by the session thread, at the end of a transition
    /// ([`InputShared::open`], [`InputShared::away`]), and derived from the link
    /// then. Nothing decides it but the link.
    presence: Presence<PortCause>,
    queue: Vec<InputRequest>,
}

impl Inbox {
    /// Takes `info` as the snapshot, and whether it differs from the last.
    fn replace_info(&mut self, info: Arc<MidiInputInfo>) -> bool {
        let differs = !self.info.same_as(&info);
        if differs {
            self.info = info;
        }
        differs
    }
}

/// What opening a connection changed of what the handles read.
struct Published {
    info: bool,
    presence: bool,
}

impl InputShared {
    /// Queues `request` with its `reply` when the port can take it now and
    /// `lists` says the peer and what is asked of it are still listed in the
    /// port's info; otherwise hands the reply back with why: [`Shut::Released`]
    /// once every handle is dropped, [`Shut::Away`] while the device is
    /// away or when `lists` is false, since the peer or the Profile it lists has
    /// gone.
    pub(crate) fn push<R, L>(
        &self,
        request: R,
        reply: L,
        lists: impl FnOnce(&MidiInputInfo, &R) -> bool,
        make: impl FnOnce(R, L) -> InputRequest,
    ) -> Result<(), (L, Shut)> {
        let mut inbox = self.inbox.lock();
        let refused = match &inbox.door {
            Door::Closed(why) => Some(*why),
            Door::Open(_) if !lists(&inbox.info, &request) => Some(Shut::Away),
            Door::Open(_) => None,
        };
        match refused {
            Some(why) => Err((reply, why)),
            None => {
                inbox.queue.push(make(request, reply));
                self.bell.ring();
                Ok(())
            }
        }
    }

    /// Sends what the handles queued: a request the device took waits for its
    /// answer, and one it cannot take ends its future.
    pub(crate) fn send_requests(&self, io: &mut MidiSession) {
        let queued = self.drain();
        if queued.is_empty() {
            return;
        }
        let mut pending = self.pending.lock();
        for request in queued {
            match request {
                InputRequest::Profile(switch, reply) => pending.switch(io, switch, reply),
                InputRequest::Report(ask, reply) => pending.report(io, ask, reply),
                InputRequest::Get(ask, reply) => pending.get(io, ask, reply),
                InputRequest::Set(ask, reply) => pending.set(io, ask, reply),
                InputRequest::Subscribe(ask, reply) => pending.subscribe(io, ask, reply),
                InputRequest::Unsubscribe(ask) => pending.unsubscribe(io, ask),
            }
        }
    }

    /// Resolves the requests waiting for what a peer answered.
    pub(crate) fn answer(
        &self,
        io: &mut MidiSession,
        peer: tutti_midi_hardware::PeerId,
        answer: &Answer,
    ) {
        self.pending.lock().answer(io, &self.link, peer, answer);
    }

    /// Whether the port holds `device` open.
    pub(crate) fn is_open_on(&self, device: &MidiDeviceId) -> bool {
        matches!(&self.inbox.lock().door, Door::Open(open) if open == device)
    }

    /// The device the port holds open.
    fn device(&self) -> Option<MidiDeviceId> {
        match &self.inbox.lock().door {
            Door::Open(device) => Some(device.clone()),
            Door::Closed(_) => None,
        }
    }

    /// The last snapshot of the port.
    fn info(&self) -> Arc<MidiInputInfo> {
        Arc::clone(&self.inbox.lock().info)
    }

    /// The last published presence of the port.
    fn presence(&self) -> Presence<PortCause> {
        self.inbox.lock().presence
    }

    /// The session takes what the handles queued.
    fn drain(&self) -> Vec<InputRequest> {
        std::mem::take(&mut self.inbox.lock().queue)
    }

    /// A connection to `device` opened: requests are taken again and checked
    /// against `info`, and the port reads [`Presence::Live`], in one step. A
    /// port whose handles were all dropped stays shut.
    fn open(&self, device: MidiDeviceId, info: Arc<MidiInputInfo>) -> Published {
        let mut inbox = self.inbox.lock();
        match inbox.door {
            Door::Closed(Shut::Away) => inbox.door = Door::Open(device),
            Door::Closed(Shut::Released) => {}
            Door::Open(_) => {
                debug_assert!(
                    false,
                    "a connection opened on a port whose door was open: a connection is closed first"
                );
                inbox.door = Door::Open(device);
            }
        }
        let presence = inbox.presence != Presence::Live;
        inbox.presence = Presence::Live;
        Published {
            info: inbox.replace_info(info),
            presence,
        }
    }

    /// The port is away for `cause` and reads [`Presence::Lost`]; whether that
    /// differs from what the handles read. Closing the connection already shut the door,
    /// so presence can read [`Presence::Live`] behind a closed door until this runs.
    fn away(&self, cause: PortCause) -> bool {
        let mut inbox = self.inbox.lock();
        let lost = Presence::Lost(cause);
        let differs = inbox.presence != lost;
        inbox.presence = lost;
        differs
    }

    /// Takes `info` as the snapshot of the open connection; whether it differs
    /// from the last.
    fn refresh(&self, info: Arc<MidiInputInfo>) -> bool {
        self.inbox.lock().replace_info(info)
    }

    /// Ends what the handles queued as `why`, and refuses what they ask next:
    /// for good with [`Shut::Released`], or until a connection opens with
    /// [`Shut::Away`].
    fn shut(&self, why: Shut) {
        let queued = {
            let mut inbox = self.inbox.lock();
            if !matches!(inbox.door, Door::Closed(Shut::Released)) {
                inbox.door = Door::Closed(why);
            }
            std::mem::take(&mut inbox.queue)
        };
        for request in queued {
            request.end(why);
        }
    }
}

/// An output port's record.
pub(crate) struct OutputShared {
    id: PeerResourceId,
    cell: Mutex<Cell<MidiOutputInfo>>,
    /// What the port did not send, counted by the session.
    losses: LossReaders,
}

impl OutputShared {
    fn read<R>(&self, read: impl FnOnce(&Cell<MidiOutputInfo>) -> R) -> R {
        read(&self.cell.lock())
    }
}

// --- The entries ---

// --- The handles ---

/// An input port: [`Session::open_midi_input`].
///
/// `Clone` and `Send`. Clones share the port, and it closes when the last
/// clone is dropped, at the session's next [`update`](Session::update) or
/// open: the OS port is released then, and the session takes the input node
/// out of the graph once that node has released the notes its device held. It
/// follows its device across an unplug and a replug.
#[derive(Clone, Debug)]
pub struct MidiInput {
    claim: Arc<Claim<InputShared>>,
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
    /// rule, a follower or a tap reads. The node is in the graph while a handle of the
    /// port lives, and the same node serves the port across an unplug and a
    /// replug.
    ///
    /// Its one event input is the session's: the device's inlet is wired to it
    /// at every open, so a host `set_sources` onto it would cut the device off
    /// until the next replug. The graph has no way to refuse an edge into a node
    /// by who owns it, so it is not refused; do not wire into it.
    #[must_use]
    pub fn node(&self) -> NodeKey {
        self.claim.node
    }

    /// The latest correction the port's clock map made to place the device's
    /// events on the graph's frames, signed in seconds: positive when a reading
    /// came late. A steady input reads a small figure, and one whose timing is
    /// unsteady jumps.
    ///
    /// The gauge belongs to the port, not to a connection: it reads the same
    /// cell across an unplug and a replug, and reads 0 until a map first makes
    /// a correction. While the device is away it holds the last correction
    /// ([`is_closed`](Gauge::is_closed) says so); a replug does not reset it,
    /// and the new connection's first map writes over it.
    #[must_use]
    pub fn timing(&self) -> Gauge<Seconds> {
        self.claim.shared.cells.timing.clone()
    }

    /// How many times the port's clock map re-anchored (a reading too far from
    /// its prediction, or one the map refused and was rebuilt on), so an app can
    /// show an input whose timing is unsteady. A map that was only rescaled for
    /// another rate is not one, and neither is the first map of a connection.
    ///
    /// Cumulative for the port: the count carries across an unplug and a
    /// replug on the same cell. It is meaningful only against a real-time
    /// clock: an offline session's readings re-anchor often.
    #[must_use]
    pub fn resyncs(&self) -> Counter<Resyncs> {
        self.claim.shared.cells.resyncs.clone()
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
        self.claim.shared.cells.lost.get()
            + self.claim.shared.input.losses()
            + self.claim.shared.cells.inlet.get()
            + self.claim.shared.cells.ci_refused.get()
    }
}

impl crate::peer::sealed::Sealed for MidiInput {}

impl Resource for MidiInput {
    fn id(&self) -> PeerResourceId {
        self.claim.shared.id
    }
}

impl Peer for MidiInput {
    type Info = MidiInputInfo;
    type Cause = PortCause;

    fn info(&self) -> Arc<MidiInputInfo> {
        self.claim.shared.info()
    }

    fn presence(&self) -> Presence<PortCause> {
        self.claim.shared.presence()
    }
}

/// An output port: [`Session::open_midi_output`]. As [`MidiInput`], for the
/// device an output sends to: when the last clone is dropped, the output retires.
/// Its connection stays, and what its node renders is sent to the device, until
/// the graph has rendered past the drop; then the tap is drained a last time and
/// the connection and its Active Sensing thread end, at the first
/// [`update`](Session::update) after. So an event queued just before the drop
/// is sent, and a note a source starts in the window is ended before the
/// connection closes.
///
/// **The window is two rendered blocks.** An event that does not reach the
/// output's node within them (a delayed or latency-compensated path, a feedback
/// loop, a clip event stamped for later) is not sent; the node counts it
/// dropped. **A session that renders no block, and an output whose node is
/// bypassed or muted, keep the output retiring**, with its connection, until
/// the device is opened again or the session ends. Opening the device again
/// ends a retiring output at once, so a device has one connection of the
/// session; what the graph had not rendered is lost. A retiring output lets go
/// of a device that unplugs.
#[derive(Clone, Debug)]
pub struct MidiOutput {
    claim: Arc<Claim<OutputShared>>,
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
    /// in the graph while a handle of the port lives, and the same node serves the
    /// port across an unplug and a replug.
    #[must_use]
    pub fn node(&self) -> NodeKey {
        self.claim.node
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
        self.claim.shared.losses.get()
    }
}

impl crate::peer::sealed::Sealed for MidiOutput {}

impl Resource for MidiOutput {
    fn id(&self) -> PeerResourceId {
        self.claim.shared.id
    }
}

impl Peer for MidiOutput {
    type Info = MidiOutputInfo;
    type Cause = PortCause;

    fn info(&self) -> Arc<MidiOutputInfo> {
        self.claim.shared.read(|cell| Arc::clone(&cell.info))
    }

    fn presence(&self) -> Presence<PortCause> {
        self.claim.shared.read(|cell| cell.presence)
    }
}

/// The present device `saved` binds to: the one that is surely it, or the only
/// present one of its name; `None` when none is present.
///
/// # Errors
///
/// [`MidiPortError::Ambiguous`] when several present devices could be `saved`.
fn bind(io: &MidiSession, saved: &DeviceInfo) -> Result<Option<DeviceInfo>, MidiPortError> {
    match io.devices().resolve(saved) {
        Resolved::Exact(info) | Resolved::SameName(info) => Ok(Some(info)),
        Resolved::Ambiguous(candidates) => Err(MidiPortError::Ambiguous(candidates)),
        Resolved::Missing => Ok(None),
    }
}

/// Why the OS MIDI layer did not start, so no port opened
/// ([`OpeningMidiInput`], [`OpeningMidiOutput`]).
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MidiOpenError {
    /// The backend failed to start, or its device-watch thread ended before it
    /// answered.
    #[error(transparent)]
    BackendFailed(#[from] tutti_midi_hardware::StartFailed),
}

/// Polls the start: the proof, or why the backend has none.
fn poll_start(start: &mut Start, cx: &mut Context<'_>) -> Poll<Result<Started, MidiOpenError>> {
    Pin::new(start)
        .poll(cx)
        .map(|outcome| outcome.map_err(MidiOpenError::from))
}

/// The wait for the OS MIDI layer before an input port opens, from
/// [`Session::open_midi_input`]: a `Send` [`Future`] that resolves to the
/// [`OpenedMidiInput`] that [`Session::apply`] installs. Fed by the OS MIDI
/// layer's start: ready at once off macOS, woken by the device-watch thread on
/// macOS; no [`Session::update`] is needed.
///
/// It resolves at once off macOS. On macOS it resolves when the device-watch
/// thread has made the process's CoreMIDI client or given up, and that thread
/// wakes it. It never pends for ever: a watch thread that ends without answering
/// resolves it to [`MidiOpenError::BackendFailed`].
///
/// It holds nothing of the session, and the watch thread is the process's, which
/// outlives every session: dropping the session changes neither when the wait
/// resolves nor how. The [`OpenedMidiInput`] it resolves to is
/// [`MidiPortError::Stale`] in any session but the one that made it.
#[derive(Debug)]
#[must_use = "an input port opens only once the wait resolves and the session applies it"]
pub struct OpeningMidiInput {
    cut: Cut,
    saved: DeviceInfo,
    start: Start,
}

impl Future for OpeningMidiInput {
    type Output = Result<OpenedMidiInput, MidiOpenError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        poll_start(&mut this.start, cx).map(|outcome| {
            outcome.map(|started| OpenedMidiInput {
                cut: this.cut,
                saved: this.saved.clone(),
                started,
            })
        })
    }
}

/// An input port ready to open: what [`OpeningMidiInput`] resolves to, which
/// [`Session::apply`] installs as a [`MidiInput`].
///
/// It holds the proof that the OS MIDI layer started and the device to open,
/// not a connection: the device is bound against the session's device table when
/// it is applied. It is cut from one session; another session refuses it as
/// [`MidiPortError::Stale`].
#[derive(Debug)]
#[must_use = "an input port opens only once the session applies it"]
pub struct OpenedMidiInput {
    cut: Cut,
    saved: DeviceInfo,
    started: Started,
}

impl<F: crate::SessionFloat> Sealed<F> for OpenedMidiInput {
    type Applied = MidiInput;
    type Error = MidiPortError;

    fn cut(&self) -> &Cut {
        &self.cut
    }

    fn stale() -> MidiPortError {
        MidiPortError::Stale
    }

    fn apply_to<K: crate::AppKey, C: crate::Clock<Float = F>>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<MidiInput, MidiPortError> {
        let (mut midi, ids) = session.midi_and_ids();
        midi.open_input_port(ids, self.saved, self.started)
    }
}

/// The wait for the OS MIDI layer before an output port opens, from
/// [`Session::open_midi_output`]: as [`OpeningMidiInput`], resolving to the
/// [`OpenedMidiOutput`] that [`Session::apply`] installs. Fed as
/// [`OpeningMidiInput`] is: no [`Session::update`] is needed.
///
/// The port's Active Sensing is set here and not on the handle: a transmitter
/// that has started Active Sensing goes on (MIDI 1.0 Detailed Specification
/// p. 32), so a running port cannot turn it off.
#[derive(Debug)]
#[must_use = "an output port opens only once the wait resolves and the session applies it"]
pub struct OpeningMidiOutput {
    cut: Cut,
    saved: DeviceInfo,
    active_sensing: bool,
    start: Start,
}

impl OpeningMidiOutput {
    /// Whether the port sends Active Sensing (`0xFE`) for its whole life: from
    /// a task on the session's executor, whenever it has been silent for 270 ms
    /// ([`ACTIVE_SENSING_INTERVAL`](tutti_midi_runtime::ACTIVE_SENSING_INTERVAL)),
    /// until the port closes, and again on each device it reopens on. A receiver
    /// that has seen one releases its notes when they stop (MIDI 1.0 Detailed
    /// Specification p. 32). Off by default.
    ///
    /// The heartbeat has a dedicated thread of the executor, not a call in
    /// [`update`](Session::update), so a stalled update or app driver does not
    /// stop it.
    ///
    /// A port whose device is in the MIDI 2.0 Protocol sends none: MIDI 2.0
    /// has no Active Sensing. It starts when a negotiation moves the device to
    /// the MIDI 1.0 Protocol.
    pub const fn active_sensing(mut self, on: bool) -> Self {
        self.active_sensing = on;
        self
    }
}

impl Future for OpeningMidiOutput {
    type Output = Result<OpenedMidiOutput, MidiOpenError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        poll_start(&mut this.start, cx).map(|outcome| {
            outcome.map(|started| OpenedMidiOutput {
                cut: this.cut,
                saved: this.saved.clone(),
                active_sensing: this.active_sensing,
                started,
            })
        })
    }
}

/// An output port ready to open: what [`OpeningMidiOutput`] resolves to, which
/// [`Session::apply`] installs as a [`MidiOutput`]. As [`OpenedMidiInput`].
#[derive(Debug)]
#[must_use = "an output port opens only once the session applies it"]
pub struct OpenedMidiOutput {
    cut: Cut,
    saved: DeviceInfo,
    active_sensing: bool,
    started: Started,
}

impl<F: crate::SessionFloat> Sealed<F> for OpenedMidiOutput {
    type Applied = MidiOutput;
    type Error = MidiPortError;

    fn cut(&self) -> &Cut {
        &self.cut
    }

    fn stale() -> MidiPortError {
        MidiPortError::Stale
    }

    fn apply_to<K: crate::AppKey, C: crate::Clock<Float = F>>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<MidiOutput, MidiPortError> {
        let (mut midi, ids) = session.midi_and_ids();
        midi.open_output_port(ids, self.saved, self.active_sensing, self.started)
    }
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// Begins opening the input port of the saved device `saved`: the wait for
    /// the OS MIDI layer, which resolves to an [`OpenedMidiInput`] that
    /// [`apply`](Self::apply) turns into the port.
    ///
    /// ```no_run
    /// # use tutti_midi_device::DeviceInfo;
    /// # use tutti_session::Session;
    /// # async fn open(session: &mut Session, saved: DeviceInfo)
    /// #     -> Result<(), Box<dyn std::error::Error>> {
    /// let opened = session.open_midi_input(saved)?.await?;
    /// let input = session.apply(opened)?;
    /// # Ok(()) }
    /// ```
    ///
    /// The apply joins the port's input node to the graph (the next
    /// [`update`](Session::update) commits it), and the device opens into it when
    /// it is present. The port lives while any clone of the handle does; after the
    /// last is dropped, the device is free to open again from the next apply of an
    /// open (it closes what was dropped first).
    ///
    /// It binds a device that is surely the saved one, or the only present one
    /// of its name; an absent device opens the port reading
    /// [`Lost`](Presence::Lost)([`PortCause::Unplugged`]), and it opens
    /// behind the same handle when the device arrives. The device table is read
    /// again when the open is applied, so a device that came or went while the
    /// wait was pending binds as it then is.
    ///
    /// # Errors
    ///
    /// This call: [`MidiPortError::NoHardware`] without
    /// [`midi_hardware`](crate::SessionBuilder::midi_hardware);
    /// [`MidiPortError::Ambiguous`] when several present devices could be
    /// `saved`.
    ///
    /// The apply: [`MidiPortError::Stale`] for another session's open;
    /// [`MidiPortError::Ambiguous`] when the table now holds several that could be
    /// `saved`; and when the present device cannot be opened,
    /// [`MidiPortError::InUse`] (another port holds it),
    /// [`MidiPortError::CellsHeld`] (its previous connection still holds the
    /// port's cells) or [`MidiPortError::Hardware`].
    pub fn open_midi_input(&self, saved: DeviceInfo) -> Result<OpeningMidiInput, MidiPortError> {
        let start = self.midi_layer_for(&saved)?;
        Ok(OpeningMidiInput {
            cut: Cut::of(self),
            saved,
            start,
        })
    }

    /// Begins opening the output port of the saved device `saved`: as
    /// [`open_midi_input`](Self::open_midi_input), for a device a port sends
    /// to. [`active_sensing`](OpeningMidiOutput::active_sensing) sets what the
    /// port sends.
    ///
    /// # Errors
    ///
    /// This call: as [`open_midi_input`](Self::open_midi_input).
    ///
    /// The apply: [`MidiPortError::Stale`] for another session's open;
    /// [`MidiPortError::Ambiguous`] when the table now holds several that could be
    /// `saved`; [`MidiPortError::Hardware`] when the present device cannot be
    /// opened.
    pub fn open_midi_output(&self, saved: DeviceInfo) -> Result<OpeningMidiOutput, MidiPortError> {
        let start = self.midi_layer_for(&saved)?;
        Ok(OpeningMidiOutput {
            cut: Cut::of(self),
            saved,
            active_sensing: false,
            start,
        })
    }

    /// The wait for the OS MIDI layer, once `saved` is known to bind.
    fn midi_layer_for(&self, saved: &DeviceInfo) -> Result<Start, MidiPortError> {
        let io = self
            .midi
            .hardware
            .locked()
            .ok_or(MidiPortError::from(NoHardware))?;
        bind(&io, saved)?;
        Ok(io.start())
    }
}

// --- The session's table of ports ---

/// The session's side of an input port's connection. The device it is open on
/// is the door's ([`InputShared::device`]).
enum InputLink {
    /// The device is open: its inlet is in the graph.
    Open { inlet: NodeKey },
    /// No connection, for `cause`.
    Away { cause: PortCause },
}

/// One input port as the session keeps it.
struct InputPort {
    /// Upgrades while a handle of the port lives.
    claim: Held<InputShared>,
    shared: Arc<InputShared>,
    node: NodeKey,
    controls: MidiInputControls,
    /// The one writer of the node's generation.
    writer: GenerationWriter,
    /// The node's half of it.
    generation: DeviceGeneration,
    /// The port's connection to its device.
    link: InputLink,
    /// What the port was opened with: what a returning device is matched on.
    saved: DeviceInfo,
    /// The proof the open brought, which every reopen of the port's device takes.
    started: Started,
}

/// A device's output stamps iff its negotiated configuration receives JR
/// Timestamps, and counts its own refusals in its losses.
impl Destination for OpenOutput {
    fn stamps(&self) -> bool {
        self.config().rx_jr
    }

    fn queue_timed(&mut self, events: &[tutti_midi_runtime::Outgoing]) -> usize {
        let _ = OpenOutput::queue_timed(self, events);
        0
    }
}

impl UmpOut<OpenOutput> {
    /// The device output this sends into.
    pub(crate) fn destination(&self) -> &OpenOutput {
        &self.destination
    }

    /// The device output this sends into, to follow what its device settled.
    pub(crate) fn destination_mut(&mut self) -> &mut OpenOutput {
        &mut self.destination
    }

    /// The stamp stream, which outlives the output it stamped for.
    pub(crate) fn into_stream(self) -> JrStream {
        self.stream
    }
}

/// One output port as the session keeps it.
struct OutputPort {
    /// Upgrades while a handle of the port lives.
    claim: Held<OutputShared>,
    shared: Arc<OutputShared>,
    node: NodeKey,
    tap: EventTapReader,
    /// Moves the node's generation on when the last handle drops.
    departure: Departure,
    saved: DeviceInfo,
    /// Whether each device output the port opens sends Active Sensing.
    active_sensing: bool,
    losses: LossWriters,
    /// What the open device's output had counted of its own losses when they
    /// were last folded into `losses`.
    device_seen: Losses,
    /// The open device's output with its stamp stream; `None` while the device
    /// is away.
    out: Option<UmpOut<OpenOutput>>,
    /// The stamp stream of an output that was closed, which the next one carries
    /// on.
    parked: Option<JrStream>,
    /// The proof the open brought, which every reopen of the port's device takes.
    started: Started,
}

/// An input port leaves nothing while its node goes: its device's notes are being
/// released by the node.
impl From<InputPort> for () {
    fn from(_: InputPort) -> Self {}
}

impl Sweepable for InputPort {
    fn is_held(&self) -> bool {
        self.claim.is_held()
    }

    fn node(&self) -> NodeKey {
        self.node
    }

    fn depart(&mut self) -> DeviceGeneration {
        self.writer.depart();
        self.generation.clone()
    }
}

/// The ports of the session, and what they are opened with.
#[derive(Default)]
pub(crate) struct Ports {
    /// Held for as long as the ports are: what a request tells a released port
    /// from an ended session by.
    alive: Arc<()>,
    inputs: Vec<InputPort>,
    outputs: Vec<OutputPort>,
    /// The input nodes going.
    retiring: Vec<Retiring<()>>,
    /// The output ports going: their device stays open until the graph has
    /// rendered past the drop and the tap was drained.
    retiring_outputs: Vec<Retiring<OutputPort>>,
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

fn changed(out: &mut Vec<SessionEvent>, id: PeerResourceId, fact: ResourceFact) {
    out.push(SessionEvent::Peer(PeerEvent::ResourceChanged { id, fact }));
}

/// Sets a record's presence, saying so once when it changed.
fn set_presence<I>(
    cell: &Mutex<Cell<I>>,
    id: PeerResourceId,
    presence: Presence<PortCause>,
    out: &mut Vec<SessionEvent>,
) {
    let mut cell = cell.lock();
    if cell.presence != presence {
        cell.presence = presence;
        changed(out, id, ResourceFact::Presence);
    }
}

/// The snapshot of the open input `device`, as the OS layer reads it now.
fn input_info(
    io: &MidiSession,
    port: PeerResourceId,
    device: &DeviceInfo,
    link: &PortLink,
    pending: &Pending,
) -> MidiInputInfo {
    let learned = io.learned(&device.id);
    let profiles = io.profiles(&device.id);
    let reporters = io.report_peers(&device.id);
    let properties = io.property_peers(&device.id);
    let infos = io.ci_peers(&device.id);
    let ids = pending.peer_ids(&infos.iter().map(CiPeerInfo::id).collect::<Vec<_>>());
    let peers = infos
        .iter()
        .zip(ids)
        .map(|(info, peer_id)| {
            let id = info.id();
            ListedPeer::new(
                peer_id,
                port,
                SavedDevice::new(device.clone(), learned),
                info.clone(),
                at_peer(&profiles, id).map(|p| ProfilePeer::new(link.clone(), p.clone())),
                at_peer(&reporters, id).map(|p| ReportPeer::new(link.clone(), p.clone())),
                at_peer(&properties, id)
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
        self.end_requests(Shut::Away);
    }
}

impl InputPort {
    /// Every request of the port, queued or sent, ends as `why`, and the port
    /// takes none until its device is open again: for good with
    /// [`Shut::Released`].
    fn end_requests(&mut self, why: Shut) {
        self.shared.shut(why);
        self.shared.pending.lock().end(why);
    }

    /// Publishes the away link: the port reads [`Presence::Lost`] for its cause,
    /// saying so when that changed.
    fn publish_away(&self, out: &mut Vec<SessionEvent>) {
        if let InputLink::Away { cause } = self.link
            && self.shared.away(cause)
        {
            changed(out, self.shared.id, ResourceFact::Presence);
        }
    }

    /// Takes in what the OS layer reads of the open device, saying so when it
    /// differs from what the handles read.
    fn refresh(&self, io: &MidiSession, out: &mut Vec<SessionEvent>) {
        let Some(id) = self.shared.device() else {
            return;
        };
        let Some(device) = io.devices().list().into_iter().find(|d| d.id == id) else {
            return;
        };
        let info = input_info(
            io,
            self.shared.id,
            &device,
            &self.shared.link,
            &self.shared.pending.lock(),
        );
        if self.shared.refresh(Arc::new(info)) {
            changed(out, self.shared.id, ResourceFact::Info);
        }
    }
}

/// Closes the connection of the input `port`, which is then away for `cause`: its
/// inlet leaves the graph and its device is let go; the node releases what the
/// device held. The door still holds the device until the requests end
/// ([`InputPort::end_requests`]).
fn close_connection<F: tutti_buffers::Float>(
    port: &mut InputPort,
    io: &mut MidiSession,
    graph: &mut crate::graph::Graph<F>,
    cause: PortCause,
) {
    if let InputLink::Open { inlet } = std::mem::replace(&mut port.link, InputLink::Away { cause })
    {
        if let Some(id) = port.shared.device() {
            // An input the session no longer holds is closed already; a query by
            // id fails only with `NotConnected`.
            let closed = io.disconnect_input(&id);
            debug_assert!(
                matches!(
                    closed,
                    Ok(()) | Err(tutti_midi_hardware::MidiHardwareError::NotConnected(_))
                ),
                "closing an input by id: {closed:?}"
            );
        }
        // A host that removed the inlet itself leaves nothing to take.
        let _ = graph.remove(inlet);
    }
    port.writer.depart();
    port.controls.clear_mpe_profiles();
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
    fn sync_mpe_profiles(&self, io: &MidiSession, device: &MidiDeviceId) {
        let Some(port) = self.inputs.iter().find(|p| p.shared.is_open_on(device)) else {
            return;
        };
        let mut zones = [None; tutti_ump::MidiGroup::COUNT as usize];
        for peer in io.profiles(device).iter() {
            let Some(enabled) = peer.mpe_zones() else {
                continue;
            };
            let held: &mut Option<tutti_midi_protocol::MpeZones> =
                &mut zones[peer.at().peer().group.index()];
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
    /// joins the graph, wired to the input node, and a device arrives there. The
    /// port takes requests again once the snapshot of the new connection is
    /// published with it.
    fn connect_input<F: tutti_buffers::Float>(
        &mut self,
        at: usize,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
        info: &DeviceInfo,
    ) -> Result<Published, MidiPortError> {
        let port = &mut self.inputs[at];
        // The previous connection's input has dropped with its device being
        // closed; one that still holds the cells is refused, never connected
        // without readings.
        let clock = InputWriters::reopen(&port.shared.cells)
            .ok_or_else(|| MidiPortError::CellsHeld(info.id.clone()))?;
        match io
            .connect_input_with(port.started, &info.id, clock)
            .map_err(|e| MidiPortError::Hardware(Arc::new(e)))?
        {
            tutti_midi_hardware::Connected::Opened(inlet) => {
                let key = graph.add(inlet).erase();
                // The input node has the one event input, and the inlet was just
                // added. A host that removed the input node itself leaves it nothing to feed.
                let _ = graph.set_sources(
                    EventInputPort::new(port.node, 0),
                    [EventOutputPort::new(key, 0)],
                );
                port.link = InputLink::Open { inlet: key };
                port.writer.arrive();
                let snapshot = input_info(
                    io,
                    port.shared.id,
                    info,
                    &port.shared.link,
                    &port.shared.pending.lock(),
                );
                Ok(port.shared.open(info.id.clone(), Arc::new(snapshot)))
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
        cause: PortCause,
    ) {
        let port = &mut self.inputs[at];
        close_connection(port, io, graph, cause);
        // The device's peers went with its connection.
        port.end_requests(Shut::Away);
    }

    /// The step of the update's `Phase::Poll` for the ports: retires those whose
    /// handles were dropped, takes in a device that went or came, sends what the handles
    /// asked, and reads what the devices said. What was edited.
    pub(crate) fn poll<F: tutti_buffers::Float>(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
        rate: tutti_units::SampleRate,
        events: Vec<tutti_midi_device::DeviceEvent>,
        polled: Vec<tutti_midi_hardware::Polled>,
        out: &mut Vec<SessionEvent>,
    ) -> Edits {
        let mut edits = Edits::default();
        edits |= self.retire(io, graph);
        for event in events {
            out.push(MidiEvent::of_device(event.clone()).into());
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
        edits |= self.step(io, graph, polled, out);
        edits
    }

    /// Ends the input ports whose handles were all dropped, and takes out the
    /// input nodes that have released what their devices held. Inputs retire
    /// here, and outputs through the same [`settle`](crate::midi::settle) in
    /// [`close_dropped_outputs`](Self::close_dropped_outputs).
    pub(crate) fn retire<F: tutti_buffers::Float>(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
    ) -> Edits {
        let mut edits = Edits::default();
        let notices = &mut self.notices;
        let left = crate::midi::sweep_dropped(&mut self.inputs, &mut self.retiring, |port| {
            // The port is going: no handle is left to read why it is away.
            close_connection(port, io, graph, PortCause::Unplugged);
            port.end_requests(Shut::Released);
            notices.extend(port.shared.pending.lock().take_notices());
        });
        // The node's block that released the notes has run, or a host removed
        // the node itself.
        let gone = crate::midi::settle(&mut self.retiring, graph, |_| {}, |_| false);
        edits.graph = left || gone;
        edits.routes = left || gone;
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
            if !self.inputs[at].shared.is_open_on(&device.id) {
                continue;
            }
            self.disconnect_input(at, io, graph, PortCause::Unplugged);
            self.inputs[at].publish_away(out);
            edits.graph = true;
        }
        // A retiring output lets go of an unplugged device too, so a device that
        // returns finds no connection of the session but its held ports'.
        let going = self
            .retiring_outputs
            .iter_mut()
            .map(|going| &mut going.going);
        for port in self.outputs.iter_mut().chain(going) {
            let held = port.out.as_ref().map(UmpOut::destination);
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
            if matches!(port.link, InputLink::Open { .. })
                || !arrived.direction.is_input()
                || !is_arrived(io, &port.saved)
            {
                continue;
            }
            let id = port.shared.id;
            match self.connect_input(at, io, graph, arrived) {
                Ok(published) => {
                    changed(out, id, ResourceFact::Info);
                    if published.presence {
                        changed(out, id, ResourceFact::Presence);
                    }
                }
                Err(_) => {
                    let port = &mut self.inputs[at];
                    port.link = InputLink::Away {
                        cause: PortCause::ReopenFailed,
                    };
                    port.publish_away(out);
                }
            }
            edits.graph = true;
        }
        for port in &mut self.outputs {
            if port.out.is_some() || !arrived.direction.is_output() || !is_arrived(io, &port.saved)
            {
                continue;
            }
            let presence =
                match open_device_output(io, port.started, &arrived.id, port.active_sensing) {
                    Ok(device) => {
                        port.out = Some(match port.parked.take() {
                            Some(stream) => UmpOut::resumed(device, stream),
                            None => UmpOut::new(device, rate),
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

    /// The open input port that holds `device`.
    fn port_of(&mut self, device: &MidiDeviceId) -> Option<&mut InputPort> {
        self.inputs
            .iter_mut()
            .find(|port| port.shared.is_open_on(device))
    }

    /// Applies what the MIDI-CI task polled (`polled`) of the open inputs:
    /// follows what they settled, reopens an input whose reader died, and says
    /// what changed. The peers' answers were resolved as they came.
    fn step<F: tutti_buffers::Float>(
        &mut self,
        io: &mut MidiSession,
        graph: &mut crate::graph::Graph<F>,
        polled: Vec<tutti_midi_hardware::Polled>,
        out: &mut Vec<SessionEvent>,
    ) -> Edits {
        let mut edits = Edits::default();
        out.append(&mut self.notices);
        let mut touched: Vec<MidiDeviceId> = Vec::new();
        let mut profile_devices: Vec<MidiDeviceId> = Vec::new();
        for polled in polled {
            // The output follows what its device settled, at once: the
            // `MidiOut` phase that drains next stamps by it.
            for port in &mut self.outputs {
                if let Some(device) = port.out.as_mut().map(UmpOut::destination_mut) {
                    device.follow(&polled);
                }
            }
            let device = match &polled {
                tutti_midi_hardware::Polled::Stream(device, _)
                | tutti_midi_hardware::Polled::Ci(device, ..) => device.clone(),
                tutti_midi_hardware::Polled::Profile(device, peer, change) => {
                    if matches!(change, tutti_midi_hardware::ProfilesChange::Dropped)
                        && let Some(port) = self.port_of(device)
                    {
                        port.shared.pending.lock().lose_switches(*peer);
                    }
                    if !profile_devices.contains(device) {
                        profile_devices.push(device.clone());
                    }
                    out.push(
                        MidiEvent::CiProfiles {
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
                        port.shared.pending.lock().lose_reports(*peer);
                    }
                    out.push(
                        MidiEvent::CiProcess {
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
                        port.shared.pending.lock().lose_properties(*peer);
                    }
                    if let tutti_midi_hardware::PropertyChange::Event(event) = change
                        && let Some(port) = self.port_of(device)
                    {
                        port.shared.pending.lock().event(*peer, event);
                    }
                    // An event of the peer (its ResourceList, whose Get held one of
                    // its requests) may have made room for a call that waits.
                    if let Some(port) = self.port_of(device) {
                        port.shared.pending.lock().make_room_everywhere(io);
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
            out.extend(port.shared.pending.lock().take_notices());
            let held = port.shared.pending.lock().take_changed();
            if port
                .shared
                .device()
                .is_some_and(|id| held || touched.contains(&id))
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
            let Some(id) = self.inputs[at].shared.device() else {
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
            self.disconnect_input(at, io, graph, reader_loss(fault));
            let port_id = self.inputs[at].shared.id;
            match self.connect_input(at, io, graph, &device) {
                Ok(published) => {
                    if published.info {
                        changed(out, port_id, ResourceFact::Info);
                    }
                    if published.presence {
                        changed(out, port_id, ResourceFact::Presence);
                    }
                }
                Err(_) => self.inputs[at].publish_away(out),
            }
            edits.graph = true;
        }
        edits
    }
}

impl Sweepable for OutputPort {
    fn is_held(&self) -> bool {
        self.claim.is_held()
    }

    fn node(&self) -> NodeKey {
        self.node
    }

    fn depart(&mut self) -> DeviceGeneration {
        self.departure.depart()
    }
}

impl OutputPort {
    /// Reads the tap dry to the device, and counts what did not go.
    fn drain(&mut self, timing: Option<&crate::midi::OutputTiming>) {
        crate::midi::drain_events(&mut self.tap, self.out.as_mut(), &mut self.losses, timing);
        self.fold_device_losses();
    }

    /// Counts what the device's output refused or lost after taking since the
    /// last call: a device counts its own, which a send's return does not carry.
    fn fold_device_losses(&mut self) {
        let Some(device) = self.out.as_ref().map(UmpOut::destination) else {
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
        let Some(device) = self.out.as_ref().map(UmpOut::destination) else {
            return;
        };
        let config = device.config();
        let mut cell = self.shared.cell.lock();
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

/// The output of device `id`, sending Active Sensing when `active_sensing`.
fn open_device_output(
    io: &MidiSession,
    started: Started,
    id: &MidiDeviceId,
    active_sensing: bool,
) -> Result<tutti_midi_hardware::OpenOutput, MidiPortError> {
    let mut device = io
        .connect_output(started, id)
        .map_err(|e| MidiPortError::Hardware(Arc::new(e)))?;
    if active_sensing {
        device
            .active_sensing()
            .map_err(|e| MidiPortError::Hardware(Arc::new(e)))?;
    }
    Ok(device)
}

impl<F: tutti_buffers::Float> crate::Midi<'_, F> {
    /// Opens the input port of `saved`, with an id from `ids`: the input node joins the graph
    /// and the device, when it is present, opens into it.
    pub(crate) fn open_input_port(
        &mut self,
        ids: &mut crate::peer::ResourceIds,
        saved: DeviceInfo,
        started: Started,
    ) -> Result<MidiInput, MidiPortError> {
        self.close_dropped();
        let io = self
            .io
            .as_deref_mut()
            .ok_or(MidiPortError::from(NoHardware))?;
        let link = self.link.ok_or(MidiPortError::from(NoHardware))?;
        // A present device is bound when it is surely the saved one, or the one
        // of its name; one of several that could be is for the app to choose.
        let found = bind(io, &saved)?;
        let found = found.filter(|info| info.direction.is_input());
        let id = ids.mint();
        let ids = ids.clone();
        let (writer, generation) = DeviceGeneration::new();
        let layout = self
            .nodes
            .map_or(MidiInputPorts::FIRST_GROUP, |n| n.input_ports);
        let node = tutti_midi_runtime::MidiInputNode::new()
            .hearing(layout)
            .mpe(self.state.mpe())
            .following(&generation);
        let key = self.graph.add(node);
        let controls = self
            .graph
            .controls(&key)
            .cloned()
            .expect("the input node was added a moment ago");
        let node = key.erase();
        let info = found.clone().unwrap_or_else(|| saved.clone());
        // The cells start closed: a connection reopens them.
        let (writers, cells) = InputWriters::new();
        drop(writers);
        let input = controls.clone();
        let pending = Pending::new(ids);
        let bell = link.bell().clone();
        let shared = Arc::new_cyclic(|weak| {
            let link = PortLink::new(weak.clone(), Arc::downgrade(&self.state.ports.alive));
            InputShared {
                id,
                inbox: Mutex::new(Inbox {
                    door: Door::Closed(Shut::Away),
                    info: Arc::new(input_info(io, id, &info, &link, &pending)),
                    presence: Presence::Lost(PortCause::Unplugged),
                    queue: Vec::new(),
                }),
                pending: Mutex::new(pending),
                bell,
                link,
                cells,
                input,
            }
        });
        let (claim, held) = Held::new(Arc::clone(&shared), node);
        self.state.ports.inputs.push(InputPort {
            claim: held,
            shared: Arc::clone(&shared),
            node,
            controls,
            writer,
            generation,
            link: InputLink::Away {
                cause: PortCause::Unplugged,
            },
            saved,
            started,
        });
        link.register(&shared);
        let at = self.state.ports.inputs.len() - 1;
        if let Some(info) = found
            && let Err(error) = self.state.ports.connect_input(at, io, self.graph, &info)
        {
            // The port was never loaded: it leaves with the open's failure.
            self.state.ports.inputs.remove(at);
            let removed = self.graph.remove(node);
            // The input node was added in this call, so it is there.
            debug_assert!(removed.is_ok());
            return Err(error);
        }
        self.compile_session_routes();
        Ok(MidiInput { claim })
    }

    /// Opens the output port of `saved`, with an id from `ids`: the output node joins the graph
    /// ([`add_output_node`](Self::add_output_node)) and the device, when it is
    /// present, opens into it.
    pub(crate) fn open_output_port(
        &mut self,
        ids: &mut crate::peer::ResourceIds,
        saved: DeviceInfo,
        active_sensing: bool,
        started: Started,
    ) -> Result<MidiOutput, MidiPortError> {
        self.close_dropped();
        let io = self.io.as_deref().ok_or(MidiPortError::from(NoHardware))?;
        let found = bind(io, &saved)?;
        let found = found.filter(|info| info.direction.is_output());
        // One connection a device: a dropped output still retiring on it ends
        // here, with what its tap held sent and what the graph has not rendered
        // lost.
        if let Some(info) = &found {
            let timing = self.state.sweep_timing();
            self.state
                .ports
                .release_device(&info.id, self.graph, timing.as_ref());
        }
        let device = match &found {
            Some(info) => Some(open_device_output(io, started, &info.id, active_sensing)?),
            None => None,
        };
        let id = ids.mint();
        let rate = self.graph.prepared().sample_rate();
        let (node, tap, departure) = self.add_output_node();
        let config = device.as_ref().map(tutti_midi_hardware::OpenOutput::config);
        let info = found.clone().unwrap_or_else(|| saved.clone());
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
        let (claim, held) = Held::new(Arc::clone(&shared), node);
        self.state.ports.outputs.push(OutputPort {
            claim: held,
            shared: Arc::clone(&shared),
            node,
            tap,
            departure,
            saved,
            active_sensing,
            losses,
            device_seen: Losses::NONE,
            out: device.map(|device| UmpOut::new(device, rate)),
            parked: None,
            started,
        });
        Ok(MidiOutput { claim })
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
        let going = self
            .retiring_outputs
            .iter_mut()
            .map(|going| &mut going.going);
        for port in self.outputs.iter_mut().chain(going) {
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

    /// Each held output port's tap, read dry and routed to its device, and the
    /// ports whose handles were all dropped retired. Whether the graph was edited.
    pub(crate) fn drain_outputs<F: tutti_buffers::Float>(
        &mut self,
        graph: &mut crate::graph::Graph<F>,
        timing: Option<&crate::midi::OutputTiming>,
    ) -> bool {
        for port in self.outputs.iter_mut().filter(|port| port.claim.is_held()) {
            port.drain(timing);
        }
        self.close_dropped_outputs(graph, timing)
    }

    /// Ends the retiring output ports that hold `device`: the tap is drained a
    /// last time, the node leaves the graph and the device closes, so the device
    /// is free to open again at once.
    fn release_device<F: tutti_buffers::Float>(
        &mut self,
        device: &MidiDeviceId,
        graph: &mut crate::graph::Graph<F>,
        timing: Option<&crate::midi::OutputTiming>,
    ) {
        crate::midi::settle(
            &mut self.retiring_outputs,
            graph,
            |port| port.drain(timing),
            |port| {
                port.out
                    .as_ref()
                    .map(UmpOut::destination)
                    .is_some_and(|open| open.id() == device)
            },
        );
    }

    /// The output ports whose handles were all dropped, retired: each is sent
    /// what its node rendered, and its device closes once the graph has rendered
    /// past the drop and the tap was drained, with its node taken out. Whether
    /// the graph was edited.
    pub(crate) fn close_dropped_outputs<F: tutti_buffers::Float>(
        &mut self,
        graph: &mut crate::graph::Graph<F>,
        timing: Option<&crate::midi::OutputTiming>,
    ) -> bool {
        crate::midi::sweep_dropped(&mut self.outputs, &mut self.retiring_outputs, |_| {});
        crate::midi::settle(
            &mut self.retiring_outputs,
            graph,
            |port| port.drain(timing),
            |_| false,
        )
    }
}

#[cfg(test)]
mod tests;
