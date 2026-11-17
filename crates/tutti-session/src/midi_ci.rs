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
//! [`SessionEvent::Landed`](crate::SessionEvent::Landed).
//!
//! Each fails with [`PeerError<Nak>`]: [`PeerError::Disconnected`] when the peer
//! leaves before the job lands, [`PeerError::TimedOut`] when it does not answer
//! in time, [`PeerError::Refused`] with its NAK. A request made of a port that
//! is lost, or of a peer or Profile the port no longer lists, fails at the call:
//! the job is returned already ended.

use std::sync::Weak;

use tutti_midi_device::{
    CiDeviceId, Nak, Outcome, ProfileId, ProfileState, ReportRequest, Request,
};
use tutti_midi_hardware::{
    Answer, ChannelCount, ChannelSwitcher as ListedChannel, MidiSession, PeerId, PeerProfiles,
    ProfileSwitch, ReportAsk, ReportEnd, Reporter as ListedReporter, WholeSwitcher as ListedWhole,
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

/// How a handle reaches its port: the port's record, held weakly because the
/// record holds the info that lists the handles, and the queue the jobs land in.
#[derive(Clone)]
pub(crate) struct PortLink {
    shared: Weak<InputShared>,
    landed: LandedQueue,
}

impl PortLink {
    pub(crate) fn new(shared: Weak<InputShared>, landed: LandedQueue) -> Self {
        Self { shared, landed }
    }

    /// A job for `request`, queued for the next poll, or already ended when
    /// the port cannot take it: the check and the queueing are one step under
    /// the port's lock, so a request either reaches the queue the session drains
    /// or ends at once.
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
            Some(shared) => shared.push(request, ticket, lists, make),
        }
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
    pub const fn whole(&self) -> tutti_midi_hardware::Whole {
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
#[derive(Default)]
pub(crate) struct Pending {
    switches: Vec<Slot<ProfileSwitch, SwitchProfile>>,
    reports: Vec<Slot<ReportAsk, RequestReport>>,
}

impl Pending {
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
    }

    /// Lands `answer` on the request of `peer` it ends, and sends what waited
    /// behind it.
    pub(crate) fn answer(&mut self, io: &mut MidiSession, peer: PeerId, answer: &Answer) {
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
            // A kind of answer this session does not know ends nothing.
            _ => {}
        }
        self.send(io);
    }

    fn end_switch(
        &mut self,
        peer: PeerId,
        request: &Request,
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

    /// Every request ends as `why`.
    pub(crate) fn end(&mut self, why: &PeerError<Nak>) {
        for slot in self.switches.drain(..) {
            end_slot(slot, || Err(why.clone()));
        }
        for slot in self.reports.drain(..) {
            end_slot(slot, || Err(why.clone()));
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
