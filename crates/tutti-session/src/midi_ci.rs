//! A request to a MIDI-CI peer is a typed [`Job`] (feature `midi-hardware`).
//!
//! The Profiles an input's peers list
//! ([`MidiInputInfo::profiles`](crate::MidiInputInfo::profiles)) and the peers it
//! can ask for a MIDI Message Report
//! ([`MidiInputInfo::report_peers`](crate::MidiInputInfo::report_peers))
//! are handles that carry their port's request queue, so a request names only
//! what the peer listed: [`ChannelProfile::enable`], [`WholeProfile::enable`]
//! and `disable` return a [`Job<SwitchProfile>`](Job), [`ReportPeer::request`]
//! a [`Job<RequestReport>`](Job). The job lands in the [`update`](crate::Session::update)
//! after the poll that reads the peer's answer, as
//! [`SessionEvent::JobDone`].
//!
//! A peer's Property Exchange Resources
//! ([`MidiInputInfo::resource_peers`](crate::MidiInputInfo::resource_peers)) are
//! handles the same way: [`ListedResource::fetch`] returns a [`Job<GetProperty>`](Job)
//! landing the Property Data, [`ListedResource::write`] a [`Job<SetProperty>`](Job)
//! landing the status of the reply, [`ListedResource::subscribe`] a
//! [`Job<Subscribe>`](Job) landing a [`Subscription`] lease. While the lease is live
//! the session keeps the Resource's Property Data current
//! ([`ResourcePeer::property`]); dropping the lease ends the Subscription with the
//! peer.
//!
//! Each fails with [`PeerError`]: [`PeerError::Disconnected`] when the peer
//! leaves before the job lands, [`PeerError::TimedOut`] when it does not answer
//! in time, [`PeerError::Refused`] with its NAK (a Profile switch or a report) or
//! its [`PeFailure`] (a Property Exchange call). A request made of a port that
//! is lost, or of a peer, Profile or Resource the port no longer lists (a
//! [`ListedResource`] whose peer's ResourceList changed since it was read), fails at
//! the call: the job is returned already ended, as [`PeerError::Disconnected`].

use std::sync::Weak;

use tutti_midi_ci::{
    BodyLimit, CallError, CiDeviceId, Nak, PeData, PeOutcome, PeReply, PeResource, PeSignal,
    PeStatus, PeTerminator, ProfileId, ProfileOutcome, ProfileRequest, ProfileState, ReportRequest,
    ResourceEntry, ResourceFailure, ResourceGet, ResourceSet, ResourceTarget,
};
use tutti_midi_hardware::{
    Answer, ChannelCount, ChannelSwitcher as ListedChannel, MidiSession, PeerId, PeerProfiles,
    ProfileSwitch, PropertyAsk, PropertyCall, PropertyPeer, ReportAsk, ReportEnd,
    Reporter as ListedReporter, WholeSwitcher as ListedWhole,
};
use tutti_ump::MidiChannel;

use crate::SessionEvent;
use crate::job::{JobOutputQueue, Ticket, job_kind};
use crate::midi_port::{Ended, InputRequest, InputShared};
use crate::peer::ResourceIds;
use crate::{Job, PeerError};

job_kind!(
    /// A peer's Profile switch: the state the peer's Enabled or Disabled Report
    /// says the Profile is in. A Profile the peer could not enable lands as
    /// [`ProfileState::Disabled`], not as an error.
    pub SwitchProfile => ProfileState, PeerError<Nak>
);

job_kind!(
    /// A MIDI Message Report the peer sent: it lands at the peer's End of
    /// Report, and its messages reached the graph as the input's MIDI before it.
    pub RequestReport => (), PeerError<Nak>
);

mod subscription;

pub(crate) use subscription::{EndSubscription, Subscriptions};
pub use subscription::{Subscribe, Subscription, SubscriptionCause};

job_kind!(
    /// A Get of a peer's Resource: the Property Data the peer sent.
    pub GetProperty => PeData, PeerError<PeFailure>
);

job_kind!(
    /// A Set of a peer's Resource: the status of the peer's reply, 200 or 201
    /// (M2-103 §7.4.1).
    pub SetProperty => PeStatus, PeerError<PeFailure>
);

/// Why a peer's Property Exchange call was not a success, as it answered or as
/// the Initiator found it: a status other than success is not a NAK.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum PeFailure {
    /// The reply has a status of a failure, 400 to 500 (M2-103 §7.4.1), with
    /// its header.
    #[error("the peer replied with status {0:?}")]
    Status(PeStatus, Box<PeReply>),
    /// The reply has a status the spec says to retry, 341 to 343: the session
    /// does not, and the app decides.
    #[error("the peer replied with status {0:?}, to retry")]
    Retry(PeStatus),
    /// The transaction was ended with a NAK that terminates it (§12.1), by the
    /// Responder or by the Initiator.
    #[error("the transaction was terminated by {0:?}")]
    Terminated(PeTerminator),
    /// The peer ended the request with the NAK of a failure other than a
    /// terminate: out of sequence, malformed or a timeout.
    #[error("the peer answered with the NAK {0:?}")]
    Nak(PeSignal),
    /// The reply's Property Data grew past the limit the request set.
    #[error("the Property Data grew past {0:?}")]
    TooLarge(BodyLimit),
    /// What came cannot be used: the reply or its Property Data, with why.
    #[error("the reply cannot be used: {0}")]
    Unreadable(ResourceFailure),
    /// The Initiator did not send the call: it breaks a rule of the Resource or
    /// of the peer's ResourceList entry, or cannot be started.
    #[error("the call was not sent: {0}")]
    NotSent(CallError),
}

impl PeFailure {
    /// What an unsuccessful call ended as: a timeout is the job's own
    /// [`PeerError::TimedOut`].
    fn of(failure: &ResourceFailure) -> PeerError<Self> {
        let ResourceFailure::Peer(outcome) = failure else {
            return PeerError::Refused(Self::Unreadable(failure.clone()));
        };
        match &**outcome {
            PeOutcome::Failed(reply) => {
                PeerError::Refused(Self::Status(reply.status(), Box::new(reply.header.clone())))
            }
            PeOutcome::Retry(reply) => PeerError::Refused(Self::Retry(reply.status())),
            PeOutcome::Terminated(by) => PeerError::Refused(Self::Terminated(*by)),
            PeOutcome::TooLarge(limit) => PeerError::Refused(Self::TooLarge(*limit)),
            PeOutcome::Refused(signal) => PeerError::Refused(Self::Nak(*signal)),
            PeOutcome::TimedOut => PeerError::TimedOut,
            // `Success` never ends a call as a failure; a kind the session does
            // not know is read as a reply that cannot be used.
            _ => PeerError::Refused(Self::Unreadable(failure.clone())),
        }
    }
}

/// How a handle reaches its port: the port's record, held weakly because the
/// record holds the info that lists the handles, and the queue the jobs land in.
#[derive(Clone)]
pub(crate) struct PortLink {
    shared: Weak<InputShared>,
    outputs: JobOutputQueue,
}

impl PortLink {
    pub(crate) fn new(shared: Weak<InputShared>, outputs: JobOutputQueue) -> Self {
        Self { shared, outputs }
    }

    /// A job for `request`, queued for the next poll, or already ended when
    /// the port cannot take it: the check and the queueing are one step under
    /// the port's lock, so a request either reaches the queue the session drains
    /// or ends at once.
    fn submit<J, R, E>(
        &self,
        request: R,
        lists: impl FnOnce(&crate::MidiInputInfo, &R) -> bool,
        make: impl FnOnce(R, Ticket<J>) -> InputRequest,
    ) -> Job<J>
    where
        J: crate::JobKind<Error = PeerError<E>>,
    {
        let (job, ticket) = Ticket::new(&self.outputs);
        match self.shared.upgrade() {
            // Every handle of the port was dropped, and the session took the port out.
            None => ticket.land(Err(PeerError::Released)),
            Some(shared) => shared.push(request, ticket, lists, make),
        }
        job
    }

    /// A job that ended at the call, as `error`.
    fn failed<J: crate::JobKind>(&self, error: J::Error) -> Job<J> {
        let (job, ticket) = Ticket::new(&self.outputs);
        ticket.land(Err(error));
        job
    }
}

impl std::fmt::Debug for PortLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortLink").finish_non_exhaustive()
    }
}

/// One found peer's Profiles
/// ([`MidiInputInfo::profiles`](crate::MidiInputInfo::profiles)): the Profiles it
/// lists, as handles to switch them from ([`listed`](Self::listed)), and the
/// peer's table ([`profiles`](Self::profiles)).
#[derive(Clone, Debug)]
pub struct ProfilePeer {
    link: PortLink,
    peer: PeerProfiles,
}

impl ProfilePeer {
    /// What the peer lists, as the OS layer read it: its Profiles' table, the
    /// zones of the MPE Profile, and whether its inquiry is whole.
    #[must_use]
    pub const fn profiles(&self) -> &PeerProfiles {
        &self.peer
    }

    pub(crate) fn new(link: PortLink, peer: PeerProfiles) -> Self {
        Self { link, peer }
    }

    /// Every Profile the peer lists, as handles to switch them from.
    pub fn listed(&self) -> impl Iterator<Item = ListedProfile> + '_ {
        self.peer.listed().map(|listed| match listed {
            tutti_midi_hardware::Switcher::Channel(inner) => {
                ListedProfile::Channel(ChannelProfile {
                    link: self.link.clone(),
                    inner,
                })
            }
            tutti_midi_hardware::Switcher::Whole(inner) => ListedProfile::Whole(WholeProfile {
                link: self.link.clone(),
                inner,
            }),
        })
    }
}

impl PartialEq for ProfilePeer {
    fn eq(&self, other: &Self) -> bool {
        self.peer == other.peer
    }
}

impl Eq for ProfilePeer {}

/// A Profile a peer lists, at the address it lists it: the handle a switch is
/// made from.
#[derive(Clone, Debug)]
pub enum ListedProfile {
    /// On one Channel: enabling it says how many Channels it is to use.
    Channel(ChannelProfile),
    /// On the Group or Function Block: enabling it has no count.
    Whole(WholeProfile),
}

impl ListedProfile {
    /// The Profile.
    #[must_use]
    pub const fn profile(&self) -> ProfileId {
        match self {
            Self::Channel(p) => p.inner.profile(),
            Self::Whole(p) => p.inner.profile(),
        }
    }

    /// Whether the peer had it enabled when this was read.
    #[must_use]
    pub const fn state(&self) -> ProfileState {
        match self {
            Self::Channel(p) => p.inner.state(),
            Self::Whole(p) => p.inner.state(),
        }
    }

    /// The Channel count of the peer's last Enabled or Disabled Report of it, if
    /// a Version 2 peer sent one.
    #[must_use]
    pub const fn channels(&self) -> Option<tutti_midi_ci::ProfileChannels> {
        match self {
            Self::Channel(p) => p.inner.channels(),
            Self::Whole(p) => p.inner.channels(),
        }
    }

    /// Asks the peer to disable the Profile at its address.
    pub fn disable(&self) -> Job<SwitchProfile> {
        match self {
            Self::Channel(p) => p.disable(),
            Self::Whole(p) => p.disable(),
        }
    }
}

/// A Profile the peer lists on one Channel.
#[derive(Clone, Debug)]
pub struct ChannelProfile {
    link: PortLink,
    inner: ListedChannel,
}

impl ChannelProfile {
    /// Asks the peer to enable the Profile on this Channel, as the Manager of
    /// `channels` Channels in all.
    ///
    /// The count is not checked against [`ListedProfile::channels`]: that is the
    /// count of the peer's last Report, not a limit the peer declared, and the
    /// limit of a Profile Details reply is never inquired. A peer that cannot
    /// comply answers with its Disabled Report, which lands as
    /// [`ProfileState::Disabled`].
    pub fn enable(&self, channels: ChannelCount) -> Job<SwitchProfile> {
        self.switch(self.inner.enable(channels))
    }

    /// Asks the peer to disable the Profile on this Channel. A Profile the peer
    /// lists as disabled is asked all the same: the spec does not make it
    /// meaningless, and the peer answers with its Disabled Report.
    pub fn disable(&self) -> Job<SwitchProfile> {
        self.switch(self.inner.disable())
    }

    /// The Channel.
    #[must_use]
    pub const fn channel(&self) -> MidiChannel {
        self.inner.channel()
    }

    fn switch(&self, switch: tutti_midi_hardware::ProfileSwitch) -> Job<SwitchProfile> {
        switch_job(&self.link, switch)
    }
}

/// A Profile the peer lists on the whole Group or Function Block.
#[derive(Clone, Debug)]
pub struct WholeProfile {
    link: PortLink,
    inner: ListedWhole,
}

impl WholeProfile {
    /// Asks the peer to enable the Profile on the whole Group or Function Block.
    pub fn enable(&self) -> Job<SwitchProfile> {
        switch_job(&self.link, self.inner.enable())
    }

    /// Asks the peer to disable the Profile on the whole Group or Function Block.
    pub fn disable(&self) -> Job<SwitchProfile> {
        switch_job(&self.link, self.inner.disable())
    }

    /// Whether it is the Group's or the Function Block's.
    #[must_use]
    pub const fn whole(&self) -> tutti_midi_ci::Whole {
        self.inner.whole()
    }
}

fn switch_job(link: &PortLink, switch: tutti_midi_hardware::ProfileSwitch) -> Job<SwitchProfile> {
    link.submit(
        switch,
        |info, switch| {
            switch
                .check(info.profiles().iter().map(ProfilePeer::profiles))
                .is_ok()
        },
        InputRequest::Profile,
    )
}

/// A peer that declared Process Inquiry and supports MIDI Message Report
/// ([`MidiInputInfo::report_peers`](crate::MidiInputInfo::report_peers)): the
/// handle a report is asked from.
#[derive(Clone, Debug)]
pub struct ReportPeer {
    link: PortLink,
    peer: ListedReporter,
}

impl ReportPeer {
    /// The peer, by its device, Group and MUID.
    #[must_use]
    pub const fn peer(&self) -> &ListedReporter {
        &self.peer
    }

    pub(crate) fn new(link: PortLink, peer: ListedReporter) -> Self {
        Self { link, peer }
    }

    /// Asks the peer for the report `what` names. The report's messages reach
    /// the graph as the input's MIDI, and the job lands at the peer's End.
    pub fn request(&self, what: ReportRequest) -> Job<RequestReport> {
        self.link.submit(
            self.peer.ask(what),
            |info, ask| {
                ask.check(info.report_peers().iter().map(ReportPeer::peer))
                    .is_ok()
            },
            InputRequest::Report,
        )
    }
}

impl PartialEq for ReportPeer {
    fn eq(&self, other: &Self) -> bool {
        self.peer == other.peer
    }
}

impl Eq for ReportPeer {}

/// A peer that declared Property Exchange and answered its Capabilities
/// ([`MidiInputInfo::resource_peers`](crate::MidiInputInfo::resource_peers)): the
/// Resources its ResourceList names, as handles to get and set them from.
#[derive(Clone, Debug)]
pub struct ResourcePeer {
    link: PortLink,
    peer: PropertyPeer,
    /// What the live Subscriptions hold of its Resources.
    held: subscription::Bodies,
}

impl ResourcePeer {
    /// The peer, by its device, Group and MUID, with its ResourceList.
    #[must_use]
    pub const fn peer(&self) -> &PropertyPeer {
        &self.peer
    }

    pub(crate) fn new(link: PortLink, peer: PropertyPeer, held: subscription::Bodies) -> Self {
        Self { link, peer, held }
    }

    /// Every Resource the peer's ResourceList names, as handles to make a call
    /// from.
    pub fn listed(&self) -> impl Iterator<Item = ListedResource> + '_ {
        self.peer
            .resources()
            .entries()
            .iter()
            .map(|entry| ListedResource {
                link: self.link.clone(),
                peer: self.peer.clone(),
                entry: entry.clone(),
            })
    }
}

impl PartialEq for ResourcePeer {
    fn eq(&self, other: &Self) -> bool {
        self.peer == other.peer && self.held == other.held
    }
}

/// A Resource a peer's ResourceList names, with the entry it gave it: the handle
/// a Get or a Set is made from.
#[derive(Clone, Debug)]
pub struct ListedResource {
    link: PortLink,
    peer: PropertyPeer,
    entry: ResourceEntry,
}

impl ListedResource {
    /// The Resource.
    #[must_use]
    pub fn resource(&self) -> &PeResource {
        self.entry.resource()
    }

    /// What the peer says of the Resource: whether it can be got, set and
    /// subscribed to, and how its Property Data is encoded.
    #[must_use]
    pub fn entry(&self) -> &ResourceEntry {
        &self.entry
    }

    /// Asks the peer for the Resource's Property Data.
    ///
    /// A Resource whose entries are named by a `resId`, one the entry says cannot
    /// be got, and any other call the ResourceList refuses ends the job as
    /// [`PeFailure::NotSent`].
    pub fn fetch(&self) -> Job<GetProperty> {
        match ResourceTarget::try_from(self.resource().clone()) {
            Ok(target) => self.link.submit(
                self.peer.ask(PropertyCall::Get(ResourceGet::new(target))),
                listing(&self.entry),
                InputRequest::Get,
            ),
            Err(refused) => self.not_sent(refused),
        }
    }

    /// Asks the peer to send the Resource's updates (M2-103 §11): the job lands a
    /// [`Subscription`] once the peer accepts. A Resource the entry does not let be
    /// subscribed to ends the job as [`PeFailure::NotSent`]. Two calls make two
    /// Subscriptions: none is shared.
    pub fn subscribe(&self) -> Job<Subscribe> {
        match ResourceTarget::try_from(self.resource().clone()) {
            Ok(target) => self.link.submit(
                self.peer
                    .ask(PropertyCall::Subscribe(ResourceGet::new(target))),
                listing(&self.entry),
                InputRequest::Subscribe,
            ),
            Err(refused) => self.not_sent(refused),
        }
    }

    /// Sends the peer `data` as the Resource's whole Property Data. `data` that is
    /// another Resource's ends the job as [`PeFailure::NotSent`], as the
    /// ResourceList's refusal of a Set does.
    pub fn write(&self, data: PeData) -> Job<SetProperty> {
        let set = ResourceTarget::try_from(self.resource().clone())
            .and_then(|target| ResourceSet::full(target, data));
        match set {
            Ok(set) => self.link.submit(
                self.peer.ask(PropertyCall::Set(set)),
                listing(&self.entry),
                InputRequest::Set,
            ),
            Err(refused) => self.not_sent(refused),
        }
    }

    fn not_sent<J>(&self, refused: tutti_midi_ci::RequestRefused) -> Job<J>
    where
        J: crate::JobKind<Error = PeerError<PeFailure>>,
    {
        self.link
            .failed(PeerError::Refused(PeFailure::NotSent(CallError::Refused(
                refused,
            ))))
    }
}

/// Whether the port's info still lists the peer of `ask`.
pub(crate) fn listed_peer(info: &crate::MidiInputInfo, ask: &PropertyAsk) -> bool {
    info.resource_peers()
        .iter()
        .map(ResourcePeer::peer)
        .any(|peer| peer.group() == ask.peer().group() && peer.muid() == ask.peer().muid())
}

/// Whether the port's info still lists the peer of `ask` with the Resource's
/// entry as it was listed.
fn listing(entry: &ResourceEntry) -> impl FnOnce(&crate::MidiInputInfo, &PropertyAsk) -> bool + '_ {
    move |info, ask| {
        listed_peer(info, ask)
            && info
                .resource_peers()
                .iter()
                .map(ResourcePeer::peer)
                .find(|peer| peer.group() == ask.peer().group() && peer.muid() == ask.peer().muid())
                .is_some_and(|peer| peer.resources().entry(entry.resource()) == Some(entry))
    }
}

// --- The requests in flight ---

/// A request sent to a peer, or waiting to be, with the jobs that wait for its
/// answer. Identical requests share one slot and so one answer.
struct Slot<R, J: crate::JobKind> {
    peer: PeerId,
    request: R,
    sent: bool,
    tickets: Vec<Ticket<J>>,
}

/// The requests a port has made of its peers and they have not answered.
///
/// A peer answers one switch of a Profile at an address, and one report, at a
/// time, so a request that differs from the one in flight for the same target
/// waits behind it and is sent when that one ends. An identical request joins the
/// last request for its target, in flight or waiting, and lands with its answer;
/// one that follows a different request queues its own.
pub(crate) struct Pending {
    switches: Vec<Slot<ProfileSwitch, SwitchProfile>>,
    reports: Vec<Slot<ReportAsk, RequestReport>>,
    properties: Vec<PropertySlot>,
    /// The peers whose last call found them out of requests to take: their
    /// unsent calls wait for an answer or an event of the peer to make room. A
    /// request id is released only as its call ends, which is an answer or an
    /// event, so a parked call is never left without one to wake it.
    full: Vec<PeerId>,
    /// What the live Subscriptions hold.
    subs: Subscriptions,
    ids: ResourceIds,
}

/// A Property Exchange call sent to a peer, or waiting to be, in the order the
/// handles made them: a Get and a Set of one Resource wait on each other.
enum PropertySlot {
    Get(Slot<PropertyAsk, GetProperty>),
    Set(Slot<PropertyAsk, SetProperty>),
    Subscribe(Slot<PropertyAsk, Subscribe>),
    Unsubscribe(Slot<PropertyAsk, EndSubscription>),
}

impl PropertySlot {
    const fn peer(&self) -> PeerId {
        match self {
            Self::Get(s) => s.peer,
            Self::Set(s) => s.peer,
            Self::Subscribe(s) => s.peer,
            Self::Unsubscribe(s) => s.peer,
        }
    }

    const fn ask(&self) -> &PropertyAsk {
        match self {
            Self::Get(s) => &s.request,
            Self::Set(s) => &s.request,
            Self::Subscribe(s) => &s.request,
            Self::Unsubscribe(s) => &s.request,
        }
    }

    const fn sent(&self) -> bool {
        match self {
            Self::Get(s) => s.sent,
            Self::Set(s) => s.sent,
            Self::Subscribe(s) => s.sent,
            Self::Unsubscribe(s) => s.sent,
        }
    }

    fn set_sent(&mut self, sent: bool) {
        match self {
            Self::Get(s) => s.sent = sent,
            Self::Set(s) => s.sent = sent,
            Self::Subscribe(s) => s.sent = sent,
            Self::Unsubscribe(s) => s.sent = sent,
        }
    }

    /// Whether `ask` is a call of the same Resource of the same peer.
    fn is_target_of(&self, ask: &PropertyAsk) -> bool {
        self.ask().call().resource() == ask.call().resource() && same_peer(self.ask(), ask)
    }

    /// Ends every job of the slot as `error`.
    fn fail(self, error: &PeerError<PeFailure>) {
        match self {
            Self::Get(slot) => end_slot(slot, || Err(error.clone())),
            Self::Set(slot) => end_slot(slot, || Err(error.clone())),
            Self::Subscribe(slot) => end_slot(slot, || Err(error.clone())),
            Self::Unsubscribe(slot) => end_slot(slot, || Err(error.clone())),
        }
    }
}

/// Whether two asks are of one peer of the port.
fn same_peer(a: &PropertyAsk, b: &PropertyAsk) -> bool {
    a.peer().group() == b.peer().group() && a.peer().muid() == b.peer().muid()
}

/// The peer `ask` is of.
fn peer_of(ask: &PropertyAsk) -> PeerId {
    PeerId {
        group: ask.peer().group(),
        muid: ask.peer().muid(),
    }
}

impl Pending {
    pub(crate) fn new(ids: ResourceIds) -> Self {
        Self {
            switches: Vec::new(),
            reports: Vec::new(),
            properties: Vec::new(),
            full: Vec::new(),
            subs: Subscriptions::default(),
            ids,
        }
    }

    /// Takes in a switch the app asked for: sent now when nothing else is out
    /// for its Profile at its address.
    pub(crate) fn switch(
        &mut self,
        io: &mut MidiSession,
        switch: ProfileSwitch,
        ticket: Ticket<SwitchProfile>,
    ) {
        let key = (switch.peer(), switch_at(&switch.request()));
        let last = self
            .switches
            .iter_mut()
            .rev()
            .find(|s| (s.peer, switch_at(&s.request.request())) == key);
        if let Some(slot) = last.filter(|s| s.request == switch) {
            slot.tickets.push(ticket);
            return;
        }
        self.switches.push(Slot {
            peer: switch.peer(),
            request: switch,
            sent: false,
            tickets: vec![ticket],
        });
        self.send(io);
    }

    /// Takes in a report the app asked for: sent now when none is out for its
    /// peer.
    pub(crate) fn report(
        &mut self,
        io: &mut MidiSession,
        ask: ReportAsk,
        ticket: Ticket<RequestReport>,
    ) {
        let last = self.reports.iter_mut().rev().find(|s| s.peer == ask.peer());
        if let Some(slot) = last.filter(|s| s.request == ask) {
            slot.tickets.push(ticket);
            return;
        }
        self.reports.push(Slot {
            peer: ask.peer(),
            request: ask,
            sent: false,
            tickets: vec![ticket],
        });
        self.send(io);
    }

    /// Takes in a Get the app asked for: sent now when nothing else is out for its
    /// peer's Resource.
    pub(crate) fn get(
        &mut self,
        io: &mut MidiSession,
        ask: PropertyAsk,
        ticket: Ticket<GetProperty>,
    ) {
        if let Some(PropertySlot::Get(slot)) = self.last_for(&ask).filter(|s| *s.ask() == ask) {
            slot.tickets.push(ticket);
            return;
        }
        self.properties.push(PropertySlot::Get(Slot {
            peer: peer_of(&ask),
            request: ask,
            sent: false,
            tickets: vec![ticket],
        }));
        self.send(io);
    }

    /// Takes in a Set the app asked for: sent now when nothing else is out for
    /// its peer's Resource.
    pub(crate) fn set(
        &mut self,
        io: &mut MidiSession,
        ask: PropertyAsk,
        ticket: Ticket<SetProperty>,
    ) {
        if let Some(PropertySlot::Set(slot)) = self.last_for(&ask).filter(|s| *s.ask() == ask) {
            slot.tickets.push(ticket);
            return;
        }
        self.properties.push(PropertySlot::Set(Slot {
            peer: peer_of(&ask),
            request: ask,
            sent: false,
            tickets: vec![ticket],
        }));
        self.send(io);
    }

    /// Takes in a Subscription the app asked for. It is never joined: each lease
    /// is its own.
    pub(crate) fn subscribe(
        &mut self,
        io: &mut MidiSession,
        ask: PropertyAsk,
        ticket: Ticket<Subscribe>,
    ) {
        self.properties.push(PropertySlot::Subscribe(Slot {
            peer: peer_of(&ask),
            request: ask,
            sent: false,
            tickets: vec![ticket],
        }));
        self.send(io);
    }

    /// Takes in the end of a Subscription, asked when its lease dropped.
    pub(crate) fn unsubscribe(
        &mut self,
        io: &mut MidiSession,
        ask: PropertyAsk,
        ticket: Ticket<EndSubscription>,
    ) {
        self.properties.push(PropertySlot::Unsubscribe(Slot {
            peer: peer_of(&ask),
            request: ask,
            sent: false,
            tickets: vec![ticket],
        }));
        self.send(io);
    }

    /// The last call for `ask`'s Resource of its peer, in flight or waiting: an
    /// identical call joins it, and one that follows a different call queues its
    /// own, since it reads what that one leaves.
    fn last_for(&mut self, ask: &PropertyAsk) -> Option<&mut PropertySlot> {
        self.properties
            .iter_mut()
            .rev()
            .find(|s| s.is_target_of(ask))
    }

    /// Room may have opened in the requests `peer` takes: its waiting calls are
    /// tried again at the next [`send`](Self::send).
    pub(crate) fn make_room(&mut self, peer: PeerId) {
        self.full.retain(|full| *full != peer);
    }

    /// Room may have opened in every peer's requests, as one of the peer's own
    /// (the driver's, which this port did not make) ended.
    pub(crate) fn make_room_everywhere(&mut self, io: &mut MidiSession) {
        self.full.clear();
        self.send(io);
    }

    /// Sends each request that waits for nothing, and ends one the device
    /// cannot take.
    pub(crate) fn send(&mut self, io: &mut MidiSession) {
        let mut at = 0;
        while at < self.switches.len() {
            let busy = {
                let slot = &self.switches[at];
                let key = (slot.peer, switch_at(&slot.request.request()));
                self.switches[..at]
                    .iter()
                    .any(|s| (s.peer, switch_at(&s.request.request())) == key)
            };
            if busy || self.switches[at].sent {
                at += 1;
            } else if io.set_profile(self.switches[at].request.clone()).is_ok() {
                self.switches[at].sent = true;
                at += 1;
            } else {
                end_slot(self.switches.remove(at), || Err(PeerError::Disconnected));
            }
        }
        let mut at = 0;
        while at < self.reports.len() {
            let peer = self.reports[at].peer;
            let busy = self.reports[..at].iter().any(|s| s.peer == peer);
            if busy || self.reports[at].sent {
                at += 1;
            } else if io.report(self.reports[at].request.clone()).is_ok() {
                self.reports[at].sent = true;
                at += 1;
            } else {
                end_slot(self.reports.remove(at), || Err(PeerError::Disconnected));
            }
        }
        let mut at = 0;
        while at < self.properties.len() {
            let slot = &self.properties[at];
            let busy = self.properties[..at]
                .iter()
                .any(|s| s.is_target_of(slot.ask()));
            if busy || slot.sent() || self.full.contains(&slot.peer()) {
                at += 1;
            } else if io.ask_property(slot.ask().clone()).is_ok() {
                self.properties[at].set_sent(true);
                at += 1;
            } else {
                self.properties.remove(at).fail(&PeerError::Disconnected);
            }
        }
    }

    /// Lands `answer` on the request of `peer` it ends, and sends what waited
    /// behind it.
    pub(crate) fn answer(
        &mut self,
        io: &mut MidiSession,
        link: &PortLink,
        peer: PeerId,
        answer: &Answer,
    ) {
        match answer {
            Answer::Switch { request, outcome } => {
                self.end_switch(peer, request, &switched(outcome));
            }
            Answer::SwitchLost { request } => {
                self.end_switch(peer, request, &Err(PeerError::Disconnected));
            }
            Answer::Report(end) => self.end_report(peer, &reported(end)),
            Answer::ReportLost => self.end_report(peer, &Err(PeerError::Disconnected)),
            Answer::ReportUnsupported => self.end_report(peer, &Err(PeerError::Unsupported)),
            Answer::Got { target, result } => {
                self.make_room(peer);
                let result = match result {
                    Ok(data) => Ok(data.clone()),
                    Err(failure) => Err(PeFailure::of(failure)),
                };
                if let Some(at) = self.sent_call(peer, |s| {
                    matches!(s, PropertySlot::Get(_))
                        && s.ask().call().resource() == Some(target.resource())
                }) && let PropertySlot::Get(slot) = self.properties.remove(at)
                {
                    end_slot(slot, || result.clone());
                }
            }
            Answer::Set { resource, result } => {
                self.make_room(peer);
                let result = match result {
                    Ok(status) => Ok(*status),
                    Err(failure) => Err(PeFailure::of(failure)),
                };
                if let Some(at) = self.sent_call(peer, |s| {
                    matches!(s, PropertySlot::Set(_)) && s.ask().call().resource() == Some(resource)
                }) && let PropertySlot::Set(slot) = self.properties.remove(at)
                {
                    end_slot(slot, || result.clone());
                }
            }
            Answer::Subscribed {
                target,
                subscribe_id,
            } => {
                self.make_room(peer);
                self.subscribed(link, peer, target.resource(), subscribe_id);
            }
            Answer::SubscribeFailed { target, failure } => {
                self.make_room(peer);
                if let Some(at) = self.sent_call(peer, |s| {
                    matches!(s, PropertySlot::Subscribe(_))
                        && s.ask().call().resource() == Some(target.resource())
                }) {
                    self.properties.remove(at).fail(&PeFailure::of(failure));
                }
            }
            Answer::Unsubscribed {
                subscribe_id,
                result,
            } => {
                self.make_room(peer);
                let call = PropertyCall::Unsubscribe(subscribe_id.clone());
                if let Some(at) = self.sent_call(peer, |s| s.ask().call() == &call) {
                    let error = result.as_ref().err().map(PeFailure::of);
                    match (self.properties.remove(at), error) {
                        (PropertySlot::Unsubscribe(slot), None) => end_slot(slot, || Ok(())),
                        (slot, Some(error)) => slot.fail(&error),
                        (slot, None) => slot.fail(&PeerError::Disconnected),
                    }
                }
            }
            Answer::PropertyNotSent { call, error } => self.not_sent(peer, call, error),
            Answer::PropertyLost { call } => {
                if let Some(at) = self.sent_call(peer, |s| s.ask().call() == &**call) {
                    self.properties.remove(at).fail(&PeerError::Disconnected);
                }
            }
            // A kind of answer this session does not know ends nothing.
            _ => {}
        }
        self.send(io);
    }

    fn end_switch(
        &mut self,
        peer: PeerId,
        request: &ProfileRequest,
        result: &Result<ProfileState, PeerError<Nak>>,
    ) {
        let key = (peer, switch_at(request));
        let at = self
            .switches
            .iter()
            .position(|s| s.sent && (s.peer, switch_at(&s.request.request())) == key);
        if let Some(at) = at {
            end_slot(self.switches.remove(at), || result.clone());
        }
    }

    /// The call out for `peer` that `is` says is the one.
    fn sent_call(&self, peer: PeerId, is: impl Fn(&PropertySlot) -> bool) -> Option<usize> {
        self.properties
            .iter()
            .position(|s| s.sent() && s.peer() == peer && is(s))
    }

    /// The Initiator did not start the call out for `peer`. One that found the
    /// peer out of requests (`TooManyRequests`, M2-103 §12) is not refused: the
    /// peer is held to its count, and the call is sent once an answer or an
    /// event of the peer makes room. Any other refusal ends the job.
    fn not_sent(&mut self, peer: PeerId, call: &PropertyCall, error: &CallError) {
        let Some(at) = self.sent_call(peer, |s| s.ask().call() == call) else {
            return;
        };
        if matches!(
            error,
            CallError::Pe(tutti_midi_ci::PeError::TooManyRequests { .. })
        ) {
            self.properties[at].set_sent(false);
            if !self.full.contains(&peer) {
                self.full.push(peer);
            }
        } else {
            self.properties
                .remove(at)
                .fail(&PeerError::Refused(PeFailure::NotSent(error.clone())));
        }
    }

    /// The peer accepted the Subscription to `resource` that is out for it: its
    /// job lands the lease, and the record starts holding its Property Data. A
    /// job dropped since lands nowhere, and the lease it carries ends the
    /// Subscription as it drops.
    fn subscribed(
        &mut self,
        link: &PortLink,
        peer: PeerId,
        resource: &PeResource,
        subscribe_id: &tutti_midi_ci::SubscribeId,
    ) {
        // The slot is gone only when the peer's calls ended (it was lost, or its
        // port closed), and a Subscription lives in the peer's Initiator, which
        // goes with it: there is no Subscription left to end.
        let Some(at) = self.sent_call(peer, |s| {
            matches!(s, PropertySlot::Subscribe(_)) && s.ask().call().resource() == Some(resource)
        }) else {
            return;
        };
        let PropertySlot::Subscribe(slot) = self.properties.remove(at) else {
            return;
        };
        let (lease, weak) = Subscription::new(
            self.ids.mint(),
            link.clone(),
            &slot.request,
            resource.clone(),
            subscribe_id.clone(),
        );
        self.subs
            .started(peer, weak, resource.clone(), subscribe_id.clone());
        // One job asked for it: a Subscription is never joined.
        if let Some(ticket) = slot.tickets.into_iter().next() {
            ticket.land(Ok(lease));
        }
    }

    /// What a Subscription's peer sent, or the Initiator found, of its data.
    pub(crate) fn event(&mut self, peer: PeerId, event: &tutti_midi_ci::ResourceEvent) {
        use tutti_midi_ci::ResourceEvent;
        match event {
            ResourceEvent::Updated {
                subscribe_id, data, ..
            } => self.subs.update(peer, subscribe_id, Some(data.clone())),
            // What is held is out of date, and nothing says by how much.
            ResourceEvent::SyncFailed { subscribe_id, .. } => {
                self.subs.update(peer, subscribe_id, None);
            }
            ResourceEvent::SubscriptionEnded { subscribe_id, .. } => {
                self.subs.ended_by_peer(peer, subscribe_id);
            }
            _ => {}
        }
    }

    /// Forgets the Subscriptions whose leases dropped, and reads whether the
    /// Property Data held changed since the last call.
    pub(crate) fn take_changed(&mut self) -> bool {
        self.subs.prune();
        self.subs.take_dirty()
    }

    /// The notices of the leases that ended since the last call.
    pub(crate) fn take_notices(&mut self) -> Vec<SessionEvent> {
        self.subs.take_notices()
    }

    /// What `peer`'s Subscriptions hold of its Resources.
    pub(crate) fn bodies(&self, peer: PeerId) -> subscription::Bodies {
        self.subs.bodies(peer)
    }

    fn end_report(&mut self, peer: PeerId, result: &Result<(), PeerError<Nak>>) {
        if let Some(at) = self.reports.iter().position(|s| s.sent && s.peer == peer) {
            end_slot(self.reports.remove(at), || result.clone());
        }
    }

    /// The peer's Profiles went with it: its switches end.
    pub(crate) fn lose_switches(&mut self, peer: PeerId) {
        let (gone, kept) = std::mem::take(&mut self.switches)
            .into_iter()
            .partition(|s| s.peer == peer);
        self.switches = kept;
        for slot in gone {
            end_slot(slot, || Err(PeerError::Disconnected));
        }
    }

    /// The peer's Process Inquiry went with it: its reports end.
    pub(crate) fn lose_reports(&mut self, peer: PeerId) {
        let (gone, kept) = std::mem::take(&mut self.reports)
            .into_iter()
            .partition(|s| s.peer == peer);
        self.reports = kept;
        for slot in gone {
            end_slot(slot, || Err(PeerError::Disconnected));
        }
    }

    /// The peer's Property Exchange went with it: its calls end.
    pub(crate) fn lose_properties(&mut self, peer: PeerId) {
        self.subs.lose(Some(peer), SubscriptionCause::EndedByPeer);
        let (gone, kept) = std::mem::take(&mut self.properties)
            .into_iter()
            .partition(|s| s.peer() == peer);
        self.properties = kept;
        for slot in gone {
            slot.fail(&PeerError::Disconnected);
        }
    }

    /// Every request ends as `why`.
    pub(crate) fn end(&mut self, why: Ended) {
        self.subs.lose(None, SubscriptionCause::PortLost);
        for slot in self.switches.drain(..) {
            end_slot(slot, || Err(why.error()));
        }
        for slot in self.reports.drain(..) {
            end_slot(slot, || Err(why.error()));
        }
        for slot in self.properties.drain(..) {
            slot.fail(&why.error());
        }
    }
}

/// Lands what `result` makes on every job of `slot`.
fn end_slot<R, J: crate::JobKind>(
    slot: Slot<R, J>,
    result: impl Fn() -> Result<J::Output, J::Error>,
) {
    for ticket in slot.tickets {
        ticket.land(result());
    }
}

/// Where and which Profile a request switches: a Set On and a Set Off of one
/// Profile at one address wait for the same slot.
fn switch_at(request: &ProfileRequest) -> Option<(CiDeviceId, ProfileId)> {
    match request {
        ProfileRequest::SetOn { address, profile }
        | ProfileRequest::SetOff { address, profile } => Some((*address, *profile)),
        _ => None,
    }
}

fn switched(outcome: &ProfileOutcome) -> Result<ProfileState, PeerError<Nak>> {
    match outcome {
        ProfileOutcome::Reported { state, .. } => Ok(*state),
        ProfileOutcome::Refused(nak) => Err(PeerError::Refused(nak.clone())),
        ProfileOutcome::TimedOut => Err(PeerError::TimedOut),
        // An answer that is not a switch's ends no switch.
        _ => Err(PeerError::Disconnected),
    }
}

fn reported(end: &ReportEnd) -> Result<(), PeerError<Nak>> {
    match end {
        ReportEnd::Ended => Ok(()),
        ReportEnd::Refused(nak) => Err(PeerError::Refused(nak.clone())),
        ReportEnd::TimedOut => Err(PeerError::TimedOut),
        _ => Err(PeerError::Disconnected),
    }
}

#[cfg(test)]
mod tests;
