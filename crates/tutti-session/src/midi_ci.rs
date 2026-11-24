//! Requests to a MIDI-CI peer (feature `midi-hardware`).
//!
//! An input lists its peers ([`MidiInputInfo::peers`](crate::MidiInputInfo::peers)),
//! each a [`ListedPeer`] that carries its port's request queue, so a request
//! names only what the peer listed. One task on the session's executor sends the
//! requests and reads the peers' answers.
//!
//! Every request is a future that resolves when the device answers, on that
//! task, with no [`update`](crate::Session::update). The session's view of the
//! peer (the port's info, [`MidiEvent::CiProfiles`](crate::MidiEvent) and the
//! other events) changes at the next `update`, so a future may resolve before
//! the `update` that reports its peer's change; each future's output carries what
//! the answer said, so a request needs no report. Dropping one is harmless. A peer's state is
//! [`ListedPeer::save_state`], a [`SavePeerState`], and
//! [`ListedPeer::restore_state`], a [`RestorePeerState`].
//! [`ChannelProfile::enable`], [`WholeProfile::enable`] and `disable` return a
//! [`SwitchProfile`], [`ReportPeer::request`] a [`RequestReport`], and for a
//! peer's Property Exchange Resources ([`ListedPeer::resources`])
//! [`ListedResource::fetch`] returns a [`GetProperty`], [`ListedResource::write`]
//! a [`SetProperty`] and [`ListedResource::subscribe`] a [`Subscribe`] resolving
//! to a [`Subscription`] lease. While the lease is live the session keeps the
//! Resource's Property Data current ([`ResourcePeer::property`]); dropping the
//! lease ends the Subscription with the peer.
//!
//! Each fails with [`PeerError`]: [`PeerError::Disconnected`] when the peer
//! leaves before the answer, [`PeerError::TimedOut`] when it does not answer in
//! time, [`PeerError::Refused`] with its NAK (a Profile switch or a report) or its
//! [`PeFailure`] (a Property Exchange call). A request made of a port that is
//! lost, or of a peer, Profile or Resource the port no longer lists, fails at the
//! call: its future is returned already resolved, as [`PeerError::Disconnected`].

use std::collections::BTreeMap;
use std::sync::Weak;

use tutti_midi_ci::{
    BodyLimit, CallError, CiDeviceId, Nak, PartialUpdate, PeData, PeEncoding, PeOutcome, PePage,
    PeReply, PeResource, PeSignal, PeStatus, PeTerminator, ProfileId, ProfileOutcome,
    ProfileRequest, ProfileState, ReportRequest, RequestRefused, ResourceEntry, ResourceFailure,
    ResourceGet, ResourceId, ResourceSet, ResourceTarget,
};
use tutti_midi_device::SavedDevice;
use tutti_midi_hardware::{
    Answer, ChannelCount, ChannelSwitcher as ListedChannel, CiPeerInfo, PeerId, PeerProfiles,
    ProfileSwitch, PropertyAsk, PropertyCall, PropertyPeer, ReportAsk, ReportEnd,
    Reporter as ListedReporter, WholeSwitcher as ListedWhole,
};
use tutti_rt::Mutex;
use tutti_ump::MidiChannel;

use crate::PeerError;
use crate::SessionEvent;
use crate::midi_port::{InputRequest, InputShared, Shut};
use crate::peer::{PeerResourceId, ResourceIds};
use reply::{PeerReply, peer_future};
pub(crate) use reply::{ReplySender, resolve};

mod state;
pub use state::{RestorePeerState, SavePeerState};
mod reply;
mod subscription;

pub(crate) use subscription::Subscriptions;
pub use subscription::{Subscribe, Subscription, SubscriptionCause};

peer_future!(
    /// A peer's Profile switch in flight: it resolves to the state the peer's
    /// Enabled or Disabled Report says the Profile is in. A Profile the peer could
    /// not enable resolves to [`ProfileState::Disabled`], not to an error.
    SwitchProfile => ProfileState, Nak, "a switch is answered only when it is awaited"
);

peer_future!(
    /// A MIDI Message Report the peer sent, in flight: it resolves at the peer's
    /// End of Report, and its messages reached the graph as the input's MIDI
    /// before it.
    RequestReport => (), Nak, "a report is answered only when it is awaited"
);

peer_future!(
    /// A Get of a peer's Resource in flight: it resolves to the Property Data the
    /// peer sent.
    GetProperty => PeData, PeFailure, "a Get is answered only when it is awaited"
);

peer_future!(
    /// A Set of a peer's Resource in flight: it resolves to the status of the
    /// peer's reply, 200 or 201 (M2-103 §7.4.1).
    SetProperty => PeStatus, PeFailure, "a Set is answered only when it is awaited"
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
    /// What an unsuccessful call ended as: a timeout is the call's own
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
/// record holds the info that lists the handles.
#[derive(Clone)]
pub(crate) struct PortLink {
    shared: Weak<InputShared>,
    /// Upgrades while the session's ports live.
    session: Weak<()>,
}

impl PortLink {
    pub(crate) const fn new(shared: Weak<InputShared>, session: Weak<()>) -> Self {
        Self { shared, session }
    }

    /// A [`PeerReply`] for `request`, queued for the next poll: it resolves when
    /// the peer answers, or at once when the port cannot take it. The check and
    /// the queueing are one step under the port's lock, so a request either
    /// reaches the queue the session drains or ends at once.
    fn reply<T, E, R>(
        &self,
        request: R,
        lists: impl FnOnce(&crate::MidiInputInfo, &R) -> bool,
        make: impl FnOnce(R, ReplySender<T, E>) -> InputRequest,
    ) -> PeerReply<T, E> {
        let (sender, asked) = PeerReply::new();
        if let Err((sender, why)) = self.queue(request, sender, lists, make) {
            resolve(sender, Err(why.error()));
        }
        asked
    }

    /// Queues `request` with no answer to wait for: it is dropped when the port
    /// cannot take it.
    pub(crate) fn send<R>(
        &self,
        request: R,
        lists: impl FnOnce(&crate::MidiInputInfo, &R) -> bool,
        make: impl FnOnce(R, ()) -> InputRequest,
    ) {
        // A refused request has no answer to hand back: nothing waits for it.
        let _ = self.queue(request, (), lists, make);
    }

    /// Queues `request` with its `reply`, or hands the reply back with why the
    /// port could not take it.
    fn queue<R, L>(
        &self,
        request: R,
        reply: L,
        lists: impl FnOnce(&crate::MidiInputInfo, &R) -> bool,
        make: impl FnOnce(R, L) -> InputRequest,
    ) -> Result<(), (L, Shut)> {
        match self.shared.upgrade() {
            // The port is gone: released when every handle was dropped and the
            // session took it out, disconnected when the session itself ended.
            None if self.session.strong_count() > 0 => Err((reply, Shut::Released)),
            None => Err((reply, Shut::Away)),
            Some(shared) => shared.push(request, reply, lists, make),
        }
    }
}

impl std::fmt::Debug for PortLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortLink").finish_non_exhaustive()
    }
}

/// A MIDI-CI peer found on an input, as
/// [`MidiInputInfo::peers`](crate::MidiInputInfo::peers) lists it: what its
/// Discovery declared ([`info`](Self::info)) and the handles of what it offers,
/// each `None` for a peer that did not declare it. It names its port and its
/// [`PeerId`]; its requests go through the port's queue.
#[derive(Clone, Debug, PartialEq)]
pub struct ListedPeer {
    id: PeerResourceId,
    port: PeerResourceId,
    /// The device the peer is on, as the input said of it when the peer was
    /// listed: whose state a [`PeerState`](crate::PeerState) of the peer is.
    device: SavedDevice,
    info: CiPeerInfo,
    profiles: Option<ProfilePeer>,
    report: Option<ReportPeer>,
    resources: Option<ResourcePeer>,
}

impl ListedPeer {
    pub(crate) fn new(
        id: PeerResourceId,
        port: PeerResourceId,
        device: SavedDevice,
        info: CiPeerInfo,
        profiles: Option<ProfilePeer>,
        report: Option<ReportPeer>,
        resources: Option<ResourcePeer>,
    ) -> Self {
        Self {
            id,
            port,
            device,
            info,
            profiles,
            report,
            resources,
        }
    }

    /// The peer's id in the session, as a [`Resource`](crate::Resource) is named: minted when
    /// its input first lists it, and the same for as long as the input lists it
    /// ([`peer_id`](Self::peer_id) is its Group and MUID on the wire).
    #[must_use]
    pub const fn id(&self) -> PeerResourceId {
        self.id
    }

    /// The input the peer was found on, by its [`PeerResourceId`].
    #[must_use]
    pub const fn port(&self) -> PeerResourceId {
        self.port
    }

    /// The device the peer is on, as its input said of it when the peer was
    /// listed.
    #[must_use]
    pub const fn device(&self) -> &SavedDevice {
        &self.device
    }

    /// The peer's Group and MUID, which name it on the wire
    /// ([`id`](Self::id) names it in the session).
    #[must_use]
    pub const fn peer_id(&self) -> PeerId {
        self.info.id()
    }

    /// What Discovery and the Endpoint Inquiry told of the peer.
    #[must_use]
    pub const fn info(&self) -> &CiPeerInfo {
        &self.info
    }

    /// The peer's Profiles (M2-101 section 7), `None` when it did not declare
    /// Profile Configuration. A switch is a request of a listed Profile
    /// ([`ProfilePeer::listed`]).
    #[must_use]
    pub const fn profiles(&self) -> Option<&ProfilePeer> {
        self.profiles.as_ref()
    }

    /// The peer as one that supports MIDI Message Report (M2-101 section 9),
    /// `None` when it did not declare Process Inquiry or does not support it.
    #[must_use]
    pub const fn report(&self) -> Option<&ReportPeer> {
        self.report.as_ref()
    }

    /// The peer's Property Exchange Resources (M2-103), `None` when it did not
    /// declare Property Exchange or has not answered its Capabilities.
    #[must_use]
    pub const fn resources(&self) -> Option<&ResourcePeer> {
        self.resources.as_ref()
    }
}

/// One found peer's Profiles
/// ([`ListedPeer::profiles`](crate::ListedPeer::profiles)): the Profiles it
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
    pub fn disable(&self) -> SwitchProfile {
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
    /// comply answers with its Disabled Report, which resolves as
    /// [`ProfileState::Disabled`].
    pub fn enable(&self, channels: ChannelCount) -> SwitchProfile {
        self.switch(self.inner.enable(channels))
    }

    /// Asks the peer to disable the Profile on this Channel. A Profile the peer
    /// lists as disabled is asked all the same: the spec does not make it
    /// meaningless, and the peer answers with its Disabled Report.
    pub fn disable(&self) -> SwitchProfile {
        self.switch(self.inner.disable())
    }

    /// The Channel.
    #[must_use]
    pub const fn channel(&self) -> MidiChannel {
        self.inner.channel()
    }

    fn switch(&self, switch: tutti_midi_hardware::ProfileSwitch) -> SwitchProfile {
        send_switch(&self.link, switch)
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
    pub fn enable(&self) -> SwitchProfile {
        send_switch(&self.link, self.inner.enable())
    }

    /// Asks the peer to disable the Profile on the whole Group or Function Block.
    pub fn disable(&self) -> SwitchProfile {
        send_switch(&self.link, self.inner.disable())
    }

    /// Whether it is the Group's or the Function Block's.
    #[must_use]
    pub const fn whole(&self) -> tutti_midi_ci::Whole {
        self.inner.whole()
    }
}

fn send_switch(link: &PortLink, switch: tutti_midi_hardware::ProfileSwitch) -> SwitchProfile {
    SwitchProfile::new(link.reply(
        switch,
        |info, switch| switch.check(info.profile_tables()).is_ok(),
        InputRequest::Profile,
    ))
}

/// A peer that declared Process Inquiry and supports MIDI Message Report
/// ([`ListedPeer::report`](crate::ListedPeer::report)): the
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
    /// the graph as the input's MIDI, and the future resolves at the peer's End.
    pub fn request(&self, what: ReportRequest) -> RequestReport {
        RequestReport::new(self.link.reply(
            self.peer.ask(what),
            |info, ask| ask.check(info.reporters()).is_ok(),
            InputRequest::Report,
        ))
    }
}

impl PartialEq for ReportPeer {
    fn eq(&self, other: &Self) -> bool {
        self.peer == other.peer
    }
}

impl Eq for ReportPeer {}

/// A peer that declared Property Exchange and answered its Capabilities
/// ([`ListedPeer::resources`](crate::ListedPeer::resources)): the
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
                res_id: None,
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
    /// The entry of the Resource the calls name.
    res_id: Option<ResourceId>,
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

    /// The handle of the entry `res_id` of the Resource (M2-103 §8.5): its calls
    /// name that entry. ProgramList, JSONSchema, ChCtrlList, CtrlMapList and State
    /// are named with a `resId` and are called only through this handle, as is a
    /// custom or unmodelled Resource whose ResourceList entry sets `requireResId`;
    /// a Resource with no entries resolves to [`PeFailure::NotSent`] through it.
    #[must_use]
    pub fn with_res_id(&self, res_id: ResourceId) -> Self {
        Self {
            res_id: Some(res_id),
            ..self.clone()
        }
    }

    /// The Resource, and the entry the handle names.
    fn target(&self) -> Result<ResourceTarget, RequestRefused> {
        match &self.res_id {
            None => ResourceTarget::try_from(self.resource().clone()),
            Some(res_id) => ResourceTarget::entry(self.resource().clone(), res_id.clone()),
        }
    }

    /// Asks the peer for the Resource's Property Data.
    ///
    /// A Resource whose entries are named by a `resId`, called without
    /// [`with_res_id`](Self::with_res_id), one the entry says cannot be got, and
    /// any other call the ResourceList refuses resolve to [`PeFailure::NotSent`].
    pub fn fetch(&self) -> GetProperty {
        self.get(ResourceGet::new)
    }

    /// Asks the peer for one page of a List Resource (M2-103 §8.6.2). A Resource
    /// that is no List Resource, and one whose entry does not declare
    /// `canPaginate`, resolve to [`PeFailure::NotSent`].
    pub fn fetch_page(&self, page: PePage) -> GetProperty {
        self.get(|target| ResourceGet::new(target).paged(page))
    }

    /// The encoding a call is made in: ASCII where the entry takes it, the first
    /// the entry lists otherwise.
    fn encoding(&self) -> PeEncoding {
        let listed = self.entry.encodings();
        if listed.contains(&PeEncoding::Ascii) {
            PeEncoding::Ascii
        } else {
            listed.first().copied().unwrap_or(PeEncoding::Ascii)
        }
    }

    fn get(&self, get: impl FnOnce(ResourceTarget) -> ResourceGet) -> GetProperty {
        GetProperty::new(match self.target() {
            Ok(target) => self.link.reply(
                self.peer
                    .ask(PropertyCall::Get(get(target).encoded(self.encoding()))),
                listing(&self.entry),
                InputRequest::Get,
            ),
            Err(refused) => not_sent(refused),
        })
    }

    /// Asks the peer to send the Resource's updates (M2-103 §11): the future
    /// resolves to a [`Subscription`] once the peer accepts. A Resource the entry
    /// does not let be subscribed to resolves to [`PeFailure::NotSent`]. Two calls
    /// make two Subscriptions: none is shared.
    pub fn subscribe(&self) -> Subscribe {
        Subscribe::new(match self.target() {
            Ok(target) => self.link.reply(
                self.peer.ask(PropertyCall::Subscribe(
                    ResourceGet::new(target).encoded(self.encoding()),
                )),
                listing(&self.entry),
                InputRequest::Subscribe,
            ),
            Err(refused) => not_sent(refused),
        })
    }

    /// Sends the peer `data` as the Resource's whole Property Data. `data` that is
    /// another Resource's resolves to [`PeFailure::NotSent`], as the
    /// ResourceList's refusal of a Set does.
    pub fn write(&self, data: PeData) -> SetProperty {
        self.set(|target| ResourceSet::full(target, data))
    }

    /// Sends the peer `update` as a Partial Set (M2-103 §10). An entry that
    /// does not declare `canSet` `partial` resolves to [`PeFailure::NotSent`].
    pub fn write_partial(&self, update: PartialUpdate) -> SetProperty {
        self.set(|target| Ok(ResourceSet::partial(target, update)))
    }

    fn set(
        &self,
        set: impl FnOnce(ResourceTarget) -> Result<ResourceSet, RequestRefused>,
    ) -> SetProperty {
        SetProperty::new(match self.target().and_then(set) {
            Ok(set) => self.link.reply(
                self.peer
                    .ask(PropertyCall::Set(set.encoded(self.encoding()))),
                listing(&self.entry),
                InputRequest::Set,
            ),
            Err(refused) => not_sent(refused),
        })
    }
}

/// A call that was not sent because the rules of the Resource refuse it.
pub(crate) fn refusal(refused: RequestRefused) -> PeerError<PeFailure> {
    PeerError::Refused(PeFailure::NotSent(CallError::Refused(refused)))
}

/// A call that ended at the handle: the rules of the Resource refuse it.
fn not_sent<T>(refused: RequestRefused) -> PeerReply<T, PeFailure> {
    PeerReply::failed(refusal(refused))
}

/// A handle the hardware layer holds of one peer, named by its Group and MUID.
pub(crate) trait AtPeer {
    fn peer_id(&self) -> PeerId;
}

impl AtPeer for PeerProfiles {
    fn peer_id(&self) -> PeerId {
        self.at().peer()
    }
}

impl AtPeer for ListedReporter {
    fn peer_id(&self) -> PeerId {
        self.at().peer()
    }
}

impl AtPeer for PropertyPeer {
    fn peer_id(&self) -> PeerId {
        self.at().peer()
    }
}

/// The handle of `peer` among `handles`, if one is.
pub(crate) fn at_peer<T: AtPeer>(handles: &[T], peer: PeerId) -> Option<&T> {
    handles.iter().find(|handle| handle.peer_id() == peer)
}

/// Whether the port's info still lists the peer of `ask`.
pub(crate) fn listed_peer(info: &crate::MidiInputInfo, ask: &PropertyAsk) -> bool {
    info.property_peers()
        .any(|peer| peer.peer_id() == peer_of(ask))
}

/// Whether the port's info still lists the peer of `ask` with the Resource's
/// entry as it was listed.
fn listing(entry: &ResourceEntry) -> impl FnOnce(&crate::MidiInputInfo, &PropertyAsk) -> bool + '_ {
    move |info, ask| {
        listed_peer(info, ask)
            && info
                .property_peers()
                .find(|peer| peer.peer_id() == peer_of(ask))
                .is_some_and(|peer| peer.resources().entry(entry.resource()) == Some(entry))
    }
}

// --- The requests in flight ---

/// A request sent to a peer, or waiting to be, with the futures that wait for its
/// answer. Identical requests share one slot and so one answer.
struct Slot<R, T, E> {
    peer: PeerId,
    request: R,
    sent: bool,
    replies: Vec<ReplySender<T, E>>,
}

/// The requests a port has made of its peers and they have not answered.
///
/// A peer answers one switch of a Profile at an address, and one report, at a
/// time, so a request that differs from the one in flight for the same target
/// waits behind it and is sent when that one ends. An identical request joins the
/// last request for its target, in flight or waiting, and resolves with its answer;
/// one that follows a different request queues its own.
pub(crate) struct Pending {
    switches: Vec<Slot<ProfileSwitch, ProfileState, Nak>>,
    reports: Vec<Slot<ReportAsk, (), Nak>>,
    properties: Vec<PropertySlot>,
    /// The peers whose last call found them out of requests to take: their
    /// unsent calls wait for an answer or an event of the peer to make room. A
    /// request id is released only as its call ends, which is an answer or an
    /// event, so a parked call is never left without one to wake it.
    full: Vec<PeerId>,
    /// What the live Subscriptions hold.
    subs: Subscriptions,
    ids: ResourceIds,
    /// The id of each peer the port lists: kept for as long as the peer stays
    /// listed, so a refresh of the listing gives it the same id, and dropped
    /// when a refresh lists the peer no more, so a peer that returns after that
    /// is a new one with a new id.
    peer_ids: Mutex<BTreeMap<PeerId, PeerResourceId>>,
}

/// A Property Exchange call sent to a peer, or waiting to be, in the order the
/// handles made them: a Get and a Set of one Resource wait on each other.
enum PropertySlot {
    Get(Slot<PropertyAsk, PeData, PeFailure>),
    Set(Slot<PropertyAsk, PeStatus, PeFailure>),
    Subscribe(Slot<PropertyAsk, Subscription, PeFailure>),
    /// The end of a Subscription: no one waits for its answer, so it holds none.
    Unsubscribe(Slot<PropertyAsk, (), PeFailure>),
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

    /// Ends every future of the slot as `error`.
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
    peer_of(a) == peer_of(b)
}

/// The peer `ask` is of.
fn peer_of(ask: &PropertyAsk) -> PeerId {
    ask.peer().at().peer()
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
            peer_ids: Mutex::default(),
        }
    }

    /// The ids of the peers `listed`, in its order: minted when a peer is first
    /// listed, and dropped for the peers `listed` no longer holds.
    pub(crate) fn peer_ids(&self, listed: &[PeerId]) -> Vec<PeerResourceId> {
        let mut ids = self.peer_ids.lock();
        ids.retain(|peer, _| listed.contains(peer));
        listed
            .iter()
            .map(|peer| *ids.entry(*peer).or_insert_with(|| self.ids.mint()))
            .collect()
    }

    /// Takes in a switch the app asked for: sent now when nothing else is out
    /// for its Profile at its address.
    pub(crate) fn switch(
        &mut self,
        io: &mut tutti_midi_hardware::MidiSession,
        switch: ProfileSwitch,
        reply: ReplySender<ProfileState, Nak>,
    ) {
        let key = (switch.at().peer(), switch_at(&switch.request()));
        let last = self
            .switches
            .iter_mut()
            .rev()
            .find(|s| (s.peer, switch_at(&s.request.request())) == key);
        if let Some(slot) = last.filter(|s| s.request == switch) {
            slot.replies.push(reply);
            return;
        }
        self.switches.push(Slot {
            peer: switch.at().peer(),
            request: switch,
            sent: false,
            replies: vec![reply],
        });
        self.send(io);
    }

    /// Takes in a report the app asked for: sent now when none is out for its
    /// peer.
    pub(crate) fn report(
        &mut self,
        io: &mut tutti_midi_hardware::MidiSession,
        ask: ReportAsk,
        reply: ReplySender<(), Nak>,
    ) {
        let last = self
            .reports
            .iter_mut()
            .rev()
            .find(|s| s.peer == ask.peer().at().peer());
        if let Some(slot) = last.filter(|s| s.request == ask) {
            slot.replies.push(reply);
            return;
        }
        self.reports.push(Slot {
            peer: ask.peer().at().peer(),
            request: ask,
            sent: false,
            replies: vec![reply],
        });
        self.send(io);
    }

    /// Takes in a Get the app asked for: sent now when nothing else is out for its
    /// peer's Resource.
    pub(crate) fn get(
        &mut self,
        io: &mut tutti_midi_hardware::MidiSession,
        ask: PropertyAsk,
        reply: ReplySender<PeData>,
    ) {
        if let Some(PropertySlot::Get(slot)) = self.last_for(&ask).filter(|s| *s.ask() == ask) {
            slot.replies.push(reply);
            return;
        }
        self.properties.push(PropertySlot::Get(Slot {
            peer: peer_of(&ask),
            request: ask,
            sent: false,
            replies: vec![reply],
        }));
        self.send(io);
    }

    /// Takes in a Set the app asked for: sent now when nothing else is out for
    /// its peer's Resource.
    pub(crate) fn set(
        &mut self,
        io: &mut tutti_midi_hardware::MidiSession,
        ask: PropertyAsk,
        reply: ReplySender<PeStatus>,
    ) {
        if let Some(PropertySlot::Set(slot)) = self.last_for(&ask).filter(|s| *s.ask() == ask) {
            slot.replies.push(reply);
            return;
        }
        self.properties.push(PropertySlot::Set(Slot {
            peer: peer_of(&ask),
            request: ask,
            sent: false,
            replies: vec![reply],
        }));
        self.send(io);
    }

    /// Takes in a Subscription the app asked for. It is never joined: each lease
    /// is its own.
    pub(crate) fn subscribe(
        &mut self,
        io: &mut tutti_midi_hardware::MidiSession,
        ask: PropertyAsk,
        reply: ReplySender<Subscription>,
    ) {
        self.properties.push(PropertySlot::Subscribe(Slot {
            peer: peer_of(&ask),
            request: ask,
            sent: false,
            replies: vec![reply],
        }));
        self.send(io);
    }

    /// Takes in the end of a Subscription, asked when its lease dropped.
    pub(crate) fn unsubscribe(
        &mut self,
        io: &mut tutti_midi_hardware::MidiSession,
        ask: PropertyAsk,
    ) {
        self.properties.push(PropertySlot::Unsubscribe(Slot {
            peer: peer_of(&ask),
            request: ask,
            sent: false,
            replies: Vec::new(),
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
    pub(crate) fn make_room_everywhere(&mut self, io: &mut tutti_midi_hardware::MidiSession) {
        self.full.clear();
        self.send(io);
    }

    /// Sends each request that waits for nothing, and ends one the device
    /// cannot take.
    pub(crate) fn send(&mut self, io: &mut tutti_midi_hardware::MidiSession) {
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

    /// Resolves the request of `peer` it ends, and sends what waited
    /// behind it.
    pub(crate) fn answer(
        &mut self,
        io: &mut tutti_midi_hardware::MidiSession,
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
    /// event of the peer makes room. Any other refusal ends the call.
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
    /// future resolves to the lease, and the record starts holding its Property
    /// Data. A future dropped since is answered to no one, and the lease it
    /// carries ends the Subscription as it drops.
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
        // One future asked for it: a Subscription is never joined.
        if let Some(reply) = slot.replies.into_iter().next() {
            resolve(reply, Ok(lease));
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
    pub(crate) fn end(&mut self, why: Shut) {
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

/// Resolves every future of `slot` with what `result` makes of `slot`.
fn end_slot<R, T, E>(slot: Slot<R, T, E>, result: impl Fn() -> Result<T, PeerError<E>>) {
    for reply in slot.replies {
        resolve(reply, result());
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
