//! A request to a MIDI-CI peer is a typed [`Job`] (feature `midi-hardware`).
//!
//! The Profiles an input's peers list ([`MidiInputInfo::profiles`]) and the
//! peers it can ask for a MIDI Message Report ([`MidiInputInfo::report_peers`])
//! are handles that carry their port's request queue, so a request names only
//! what the peer listed: [`ChannelProfile::enable`], [`WholeProfile::enable`]
//! and `disable` return a [`Job<SwitchProfile>`](Job), [`ReportPeer::request`]
//! a [`Job<RequestReport>`](Job). The job lands in the [`update`](crate::Session::update)
//! after the poll that reads the peer's answer, as
//! [`SessionEvent::Landed`](crate::SessionEvent::Landed).
//!
//! Each fails with [`PeerError<Nak>`]: [`PeerError::Disconnected`] when the peer
//! leaves before the job lands, [`PeerError::TimedOut`] when it does not answer
//! in time, [`PeerError::Refused`] with its NAK. A request made of a port that
//! is lost, or of a peer or Profile the port no longer lists, fails at the call:
//! the job is returned already ended.

use std::ops::Deref;
use std::sync::Weak;

use tutti_midi_device::{
    CiDeviceId, Nak, Outcome, ProfileId, ProfileState, ReportRequest, Request,
};
use tutti_midi_hardware::{
    Answer, ChannelCount, ChannelProfile as ListedChannel, PeerId, PeerProfiles, ReportEnd,
    ReportPeer as ListedReporter, WholeProfile as ListedWhole,
};
use tutti_ump::MidiChannel;

use crate::job::{LandedQueue, Ticket, job_kind};
use crate::midi_port::{InputRequest, InputShared};
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

/// What a handle needs of its port to make a request: the port's record, weakly
/// (the record holds the handles its info lists), and the queue its jobs land
/// in.
#[derive(Clone)]
pub(crate) struct PortLink {
    shared: Weak<InputShared>,
    landed: LandedQueue,
}

impl PortLink {
    pub(crate) fn new(shared: Weak<InputShared>, landed: LandedQueue) -> Self {
        Self { shared, landed }
    }

    /// A job for `make`'s request, queued for the next poll, or already ended
    /// when the port cannot take it.
    fn submit<J, R>(
        &self,
        request: R,
        lists: impl FnOnce(&crate::MidiInputInfo, &R) -> bool,
        make: impl FnOnce(R, Ticket<J>) -> InputRequest,
    ) -> Job<J>
    where
        J: crate::JobKind<Error = PeerError<Nak>>,
    {
        let (job, ticket) = Ticket::new(&self.landed);
        match self.shared.upgrade() {
            None => ticket.land(Err(PeerError::Disconnected)),
            Some(shared) => match shared.admit(|info| lists(info, &request)) {
                Ok(()) => shared.enqueue(make(request, ticket)),
                Err(error) => ticket.land(Err(error)),
            },
        }
        job
    }
}

impl std::fmt::Debug for PortLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortLink").finish_non_exhaustive()
    }
}

/// One found peer's Profiles ([`MidiInputInfo::profiles`](crate::MidiInputInfo::profiles)):
/// the readings of [`PeerProfiles`], and the Profiles it lists as handles to
/// switch them from.
#[derive(Clone, Debug)]
pub struct ProfilePeer {
    link: PortLink,
    peer: PeerProfiles,
}

impl ProfilePeer {
    pub(crate) fn new(link: PortLink, peer: PeerProfiles) -> Self {
        Self { link, peer }
    }

    /// Every Profile the peer lists, as handles to switch them from.
    pub fn listed(&self) -> impl Iterator<Item = ListedProfile> + '_ {
        self.peer.listed().map(|listed| match listed {
            tutti_midi_hardware::ListedProfile::Channel(inner) => {
                ListedProfile::Channel(ChannelProfile {
                    link: self.link.clone(),
                    inner,
                })
            }
            tutti_midi_hardware::ListedProfile::Whole(inner) => {
                ListedProfile::Whole(WholeProfile {
                    link: self.link.clone(),
                    inner,
                })
            }
        })
    }
}

impl Deref for ProfilePeer {
    type Target = PeerProfiles;

    fn deref(&self) -> &PeerProfiles {
        &self.peer
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
    pub const fn channels(&self) -> Option<tutti_midi_device::ProfileChannels> {
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
    pub fn enable(&self, channels: ChannelCount) -> Job<SwitchProfile> {
        self.switch(self.inner.enable(channels))
    }

    /// Asks the peer to disable the Profile on this Channel.
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
    pub const fn whole(&self) -> tutti_midi_hardware::Whole {
        self.inner.whole()
    }
}

fn switch_job(link: &PortLink, switch: tutti_midi_hardware::ProfileSwitch) -> Job<SwitchProfile> {
    link.submit(
        switch,
        |info, switch| {
            switch
                .check(info.profiles().iter().map(Deref::deref))
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
    pub(crate) fn new(link: PortLink, peer: ListedReporter) -> Self {
        Self { link, peer }
    }

    /// Asks the peer for the report `what` names. The report's messages reach
    /// the graph as the input's MIDI, and the job lands at the peer's End.
    pub fn request(&self, what: ReportRequest) -> Job<RequestReport> {
        self.link.submit(
            self.peer.ask(what),
            |info, ask| {
                ask.check(info.report_peers().iter().map(Deref::deref))
                    .is_ok()
            },
            InputRequest::Report,
        )
    }
}

impl Deref for ReportPeer {
    type Target = ListedReporter;

    fn deref(&self) -> &ListedReporter {
        &self.peer
    }
}

impl PartialEq for ReportPeer {
    fn eq(&self, other: &Self) -> bool {
        self.peer == other.peer
    }
}

impl Eq for ReportPeer {}

// --- The requests in flight ---

/// The requests a port has sent and the peer has not answered.
#[derive(Default)]
pub(crate) struct Pending {
    switches: Vec<(PeerId, Request, Ticket<SwitchProfile>)>,
    reports: Vec<(PeerId, Ticket<RequestReport>)>,
}

impl Pending {
    pub(crate) fn switch(&mut self, peer: PeerId, request: Request, ticket: Ticket<SwitchProfile>) {
        self.switches.push((peer, request, ticket));
    }

    pub(crate) fn report(&mut self, peer: PeerId, ticket: Ticket<RequestReport>) {
        self.reports.push((peer, ticket));
    }

    /// Lands `answer` on the requests of `peer` it ends.
    pub(crate) fn answer(&mut self, peer: PeerId, answer: &Answer) {
        match answer {
            Answer::Switch { request, outcome } => {
                let at = switch_at(request);
                let ended = take(&mut self.switches, |(p, r, _)| {
                    *p == peer && switch_at(r) == at
                });
                for (_, _, ticket) in ended {
                    ticket.complete(|| switched(outcome));
                }
            }
            Answer::SwitchLost { profile } => {
                let ended = take(&mut self.switches, |(p, r, _)| {
                    *p == peer && switch_at(r).is_some_and(|(_, id)| id == *profile)
                });
                for (_, _, ticket) in ended {
                    ticket.complete(|| Err(PeerError::Disconnected));
                }
            }
            Answer::Report(end) => {
                for (_, ticket) in take(&mut self.reports, |(p, _)| *p == peer) {
                    ticket.complete(|| reported(end));
                }
            }
            Answer::ReportLost => {
                for (_, ticket) in take(&mut self.reports, |(p, _)| *p == peer) {
                    ticket.complete(|| Err(PeerError::Disconnected));
                }
            }
            // A kind of answer this session does not know ends nothing.
            _ => {}
        }
    }

    /// The peer's Profiles went with it: its switches end.
    pub(crate) fn lose_switches(&mut self, peer: PeerId) {
        for (_, _, ticket) in take(&mut self.switches, |(p, ..)| *p == peer) {
            ticket.complete(|| Err(PeerError::Disconnected));
        }
    }

    /// The peer's Process Inquiry went with it: its reports end.
    pub(crate) fn lose_reports(&mut self, peer: PeerId) {
        for (_, ticket) in take(&mut self.reports, |(p, _)| *p == peer) {
            ticket.complete(|| Err(PeerError::Disconnected));
        }
    }

    /// Every request ends as `why`.
    pub(crate) fn end(&mut self, why: &PeerError<Nak>) {
        for (_, _, ticket) in self.switches.drain(..) {
            ticket.complete(|| Err(why.clone()));
        }
        for (_, ticket) in self.reports.drain(..) {
            ticket.complete(|| Err(why.clone()));
        }
    }
}

/// Removes the items `ended` holds of `items`, in order.
fn take<T>(items: &mut Vec<T>, ended: impl Fn(&T) -> bool) -> Vec<T> {
    let (gone, kept) = std::mem::take(items)
        .into_iter()
        .partition(|item| ended(item));
    *items = kept;
    gone
}

/// Where and which Profile a request switches: a Set On and a Set Off of one
/// Profile at one address wait in the same slot.
fn switch_at(request: &Request) -> Option<(CiDeviceId, ProfileId)> {
    match request {
        Request::SetOn { address, profile } | Request::SetOff { address, profile } => {
            Some((*address, *profile))
        }
        _ => None,
    }
}

fn switched(outcome: &Outcome) -> Result<ProfileState, PeerError<Nak>> {
    match outcome {
        Outcome::Reported { state, .. } => Ok(*state),
        Outcome::Refused(nak) => Err(PeerError::Refused(nak.clone())),
        Outcome::TimedOut => Err(PeerError::TimedOut),
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
