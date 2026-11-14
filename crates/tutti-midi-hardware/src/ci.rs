//! MIDI-CI on a device's wire: the Management side of
//! [`CiDevice`] (Discovery, Invalidate MUID, NAK)
//! run on an open input, the peers it finds
//! ([`CiPeerInfo`], reported as [`CiChange`]), and Profile Configuration
//! with those that declare it (reported as [`ProfilesChange`]), and Process Inquiry with
//! those that declare that (reported as [`ProcessChange`]).
//!
//! # Opt-in
//!
//! **It is off until the app gives an identity**
//! ([`MidiSession::enable_ci`](crate::MidiSession::enable_ci)). A Discovery
//! announces a System Exclusive manufacturer id, and tutti has none of its own:
//! M1 reserves `7D` for non-commercial use, never for a released product, and
//! tutti is a library that apps ship. So no
//! device is started, and the port copies no SysEx for MIDI-CI, until the app
//! says who it is, with the id it registered. Enabling reaches the inputs
//! already open (they start at the next poll, after any Identity Request in
//! progress has ended) and later ones; disabling ends every running device,
//! sending each an Invalidate MUID (M2-101 §5.9 lists a shutdown among its
//! reasons) and reporting its peers as lost.
//!
//! # When it ends
//!
//! Besides a disable or another identity, a device ends when its Group is no
//! longer called for: a settled Endpoint whose Function Block changes (the
//! Connection keeps hearing them unless the Endpoint declared them static)
//! is read again, and the Groups recomputed. A Group that stays keeps its
//! device and peers; one that goes ends its device (Invalidate MUID, peers
//! lost: M2-101 §3.3.2, a block that changes its Group location or number of
//! Groups invalidates its MUID); a new one starts a device (Discovery). **A
//! block that moves is by Group an end and a start**: the old Group's device
//! ends and the new Group's begins under a fresh MUID.
//!
//! Closing an input ([`MidiSession::disconnect_input`](crate::MidiSession::disconnect_input),
//! `disconnect_all_inputs`, a dead input reopened) sends each running device's
//! Invalidate MUID, best effort, and reports **no** `Lost` (the app closed it;
//! the entry is gone at the next poll). Dropping the session sends nothing: a
//! drop runs where no output may be reachable.
//!
//! # Where it runs
//!
//! One [`CiDevice`] is one MIDI-CI Device on one Group, with its own MUID.
//! Enabled, the session runs them for every open input whose device has an output (the
//! one the negotiation sends on):
//!
//! - **A UMP Endpoint that answered its Stream negotiation** gets one for each
//!   Function Block that declares MIDI-CI (`midi_ci_version() != 0`, M2-104
//!   §7.1.8), on that block's first Group, so a block is a MIDI-CI Device of
//!   its own with its own MUID (M2-101 §3.2.2). **A block that declares no
//!   MIDI-CI gets none**: it said it does not speak it. An Endpoint that
//!   declares no Function Block at all, and one that never answered the
//!   negotiation, get one on Group 0: with no block to say otherwise the
//!   Endpoint's Group 0 is treated as a Function Block with one member
//!   Group (§3.2.2).
//! - **A MIDI 1.0 port** gets one on Group 0, the port's own, once the
//!   Identity Request ([`Negotiation`](crate::stream)) has ended, answered or
//!   given up, so the two never read the SysEx stream at once.
//!
//! # What it does
//!
//! It runs while enabled and the input is open: the Discovery it sends when it
//! starts is only the first round, and it answers a Discovery or an Invalidate
//! MUID from anyone, at any time (M2-101 Appendix E: a Responder). It speaks
//! Management and, as the Initiator, Profile Configuration and Process Inquiry; there is no Property
//! Exchange, so every other message addressed to it is NAK'd by the core.
//!
//! # Profiles
//!
//! A peer whose Discovery Reply declares Profile Configuration
//! ([`CiCategories::PROFILE_CONFIGURATION`]) gets a [`ProfileInitiator`], made
//! when it is found: it sends a Profile Inquiry to the Function Block, keeps the
//! Profiles the peer answers with and the reports it sends after
//! ([`MidiSession::profiles`](crate::MidiSession::profiles)), and is stepped by
//! the same poll and its time as the device is. It sends on the same output.
//! An app's request to enable or disable a Profile
//! ([`MidiSession::set_profile`](crate::MidiSession::set_profile)) is queued
//! and started by the next poll, which has the time its wait counts from. When
//! the peer is lost (an Invalidate MUID, a collision, a changed MUID of the
//! device, MIDI-CI disabled or its Group ending) its Initiator and Profiles go
//! and [`ProfilesChange::Dropped`] says so; closing the input reports nothing, as
//! for peers.
//!
//! # Process Inquiry
//!
//! A peer whose Discovery Reply declares Process Inquiry
//! ([`CiCategories::PROCESS_INQUIRY`]) gets a [`ProcessInitiator`], made when it is
//! found: it sends an Inquiry: Process Inquiry Capabilities to the Function Block,
//! and a peer whose reply says it supports MIDI Message Report is listed
//! ([`MidiSession::report_peers`](crate::MidiSession::report_peers)). An app's request
//! for a report ([`MidiSession::report`](crate::MidiSession::report)) is queued and
//! started by the next poll. The messages of a report are not CI: they are the peer's
//! MIDI, and reach the graph with the rest of the input, between the
//! [`ProcessChange::ReportBegan`] and the [`ProcessChange::ReportEnded`] the
//! session reports. A lost peer's Initiator goes with it ([`ProcessChange::Dropped`]).
//!
//! The port copies the device's SysEx7 packets to the stream ring while any
//! device is to run (`hears_sysex`); the graph still gets them all. They are
//! reassembled per Group, decoded, and routed to that Group's device. **A gap
//! in the ring never joins a message**: the port counts what the ring had no
//! room for, the session tells the reader ([`Heard`]), and the assembler drops
//! its open messages ([`SysexAssembler::lost_tail`]) before the next packets.
//! What a device sends goes out as SysEx7 on its Group, on the output of the
//! negotiation, never the session's MIDI output; a refused send is logged and
//! not repeated, since the protocol is built to tolerate loss.
//!
//! # What it declares
//!
//! - **Identity**: the app's, as given to `enable_ci`.
//! - **Categories**: Profile Configuration and Process Inquiry. The devices initiate
//!   both. They answer a Profile Inquiry with no Profiles (M2-101 §7.13 allows a device
//!   that has none to reply) and NAK a Set Profile On or Off; they answer an Inquiry:
//!   Process Inquiry Capabilities with MIDI Message Report supported, and a MIDI Message
//!   Report of the Function Block or Group with an empty Reply and the End (§9.6, §9.8),
//!   as a device with no MIDI state to report does, and one of a Channel with a NAK, since
//!   none is in use (§9.5).
//! - **Receivable Maximum SysEx Message Size**: [`MAX_SYSEX`], 512 bytes, the
//!   floor of a device that will initiate Profile Configuration or Property
//!   Exchange (§5.5.3); the reassembler is sized for it, delimiters included.
//! - **MUIDs** are drawn from a stream the operating system seeded
//!   ([`entropy`]), which M2-101 Appendix A asks of a desktop OS's random
//!   numbers. A Group whose stream cannot be seeded starts no device; the
//!   failure is reported once until it starts
//!   ([`Polled::CiNotStarted`](crate::Polled::CiNotStarted); a Group that
//!   leaves the Groups and returns, or a new identity, is reported again) and
//!   the Group is tried again at every poll while MIDI-CI is enabled.
//!
//! Time is an argument: nothing here reads a clock.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use tutti_graph::Offset;
use tutti_midi_device::{
    CiAction, CiCategories, CiDevice, CiHeader, CiMessage, CiPeer, Decoded, DeviceId, EntropyError,
    EntropyWords, MaxSysexSize, Muid, ProcessAction, ProcessFeatures, ProcessInitiator,
    ProfileAction, ProfileInitiator,
};
use tutti_ump::{DeviceIdentity, MidiGroup, SysexAssembler, SysexStep, Ump};

use crate::devices::OpenOutput;
use crate::process::{self, ProcessChange, ReportAsk, ReportError, ReportPeer};
use crate::profiles::{
    PeerProfiles, ProfileError, ProfileSwitch, ProfilesChange, Snapshot, Switch,
};
use crate::stream::DeviceStream;

/// The Receivable Maximum SysEx Message Size the session declares: 512 bytes
/// (see the module).
pub(crate) const MAX_SYSEX: MaxSysexSize = MaxSysexSize::of(MaxSysexSize::INITIATOR_MIN);

/// What the devices declare: Management, Profile Configuration and Process
/// Inquiry, which the session initiates with every peer that declares them too (the
/// module's "What it does"). They answer a Profile Inquiry with no Profiles, and NAK a
/// Set Profile On or Off, since they have none to enable; they answer a Process
/// Inquiry with the report of a device with no MIDI state.
const CATEGORIES: CiCategories =
    CiCategories::PROFILE_CONFIGURATION.union(CiCategories::PROCESS_INQUIRY);

/// What one step of an input's MIDI-CI changed, for the app.
#[derive(Default)]
pub(crate) struct CiChanges {
    /// Peers found, updated or lost.
    pub(crate) peers: Vec<(PeerId, CiChange)>,
    /// Peers' Profiles.
    pub(crate) profiles: Vec<(PeerId, ProfilesChange)>,
    /// Peers' Process Inquiry.
    pub(crate) process: Vec<(PeerId, ProcessChange)>,
    /// The Groups whose device could not start, with why.
    pub(crate) unstarted: Vec<(MidiGroup, EntropyError)>,
}

impl CiChanges {
    fn is_empty(&self) -> bool {
        self.peers.is_empty() && self.profiles.is_empty() && self.process.is_empty()
    }
}

/// What one poll of an input's port heard: the packets of its stream ring, and
/// whether the ring had no room for some.
///
/// A packet is lost when the ring is full, so the gap is after everything the
/// ring held at that moment: a loss counted before the drain began is a gap at
/// the end of the packets, and one counted while it ran (a pop made room for a
/// packet that followed the gap) is somewhere inside them, so none of them can
/// be trusted to join up.
pub(crate) struct Heard<'a> {
    /// What the ring held, oldest first.
    pub(crate) packets: &'a [Ump],
    /// The ring lost packets before this drain began.
    pub(crate) lost_before: bool,
    /// The ring lost packets while it was drained.
    pub(crate) lost_during: bool,
}

impl<'a> Heard<'a> {
    /// The packets a SysEx reader may take: none when the gap is somewhere
    /// inside them.
    pub(crate) fn sysex(&self) -> &'a [Ump] {
        if self.lost_during { &[] } else { self.packets }
    }

    /// Whether a SysEx reader's open messages end here, after the packets of
    /// [`sysex`](Self::sysex): a message that was open has lost its tail.
    pub(crate) const fn gap(&self) -> bool {
        self.lost_before || self.lost_during
    }
}

/// One MIDI-CI peer of a device: the Group it was found on (the Group of the
/// MIDI-CI Device that found it) and its MUID. Every change
/// [`MidiSession::poll`](crate::MidiSession::poll) reports about a peer names it
/// with one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId {
    /// The Group it was found on.
    pub group: MidiGroup,
    /// Its MUID.
    pub muid: Muid,
}

/// A peer of a device: who it is, and what Discovery and the Endpoint Inquiry
/// told of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiPeerInfo {
    id: PeerId,
    peer: CiPeer,
}

impl CiPeerInfo {
    /// The peer: its Group and MUID.
    #[must_use]
    pub const fn id(&self) -> PeerId {
        self.id
    }

    /// The Group of the connection it answered on: the Group of the
    /// MIDI-CI Device that found it.
    #[must_use]
    pub const fn group(&self) -> MidiGroup {
        self.id.group
    }

    /// Its MUID.
    #[must_use]
    pub const fn muid(&self) -> Muid {
        self.id.muid
    }

    /// What it declared: identity, categories, receivable size, Function
    /// Block, and its Product Instance Id once it was asked.
    #[must_use]
    pub const fn peer(&self) -> &CiPeer {
        &self.peer
    }
}

/// A change of one peer found on a device (named by the [`PeerId`] beside it in
/// [`Polled::Ci`](crate::Polled::Ci)), once each from
/// [`MidiSession::poll`](crate::MidiSession::poll).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CiChange {
    /// The peer answered Discovery; this is what it declared.
    Found(CiPeer),
    /// What the peer holds changed (its Product Instance Id arrived); this is
    /// what it holds now.
    Updated(CiPeer),
    /// The peer is gone: it was invalidated, collided, went with the device's
    /// own MUID (which a collision changes), or MIDI-CI was disabled.
    Lost,
}

/// The MUID source: words from a stream the operating system seeded.
type Entropy = Box<dyn FnMut() -> u32 + Send>;

/// A source of words for MUIDs (the module's "MUIDs"), seeded on its own for
/// each device.
///
/// # Errors
///
/// [`EntropyError`] when the operating system has no random bytes to seed it.
fn entropy() -> Result<Entropy, EntropyError> {
    let mut words = EntropyWords::from_os()?;
    Ok(Box::new(move || words.next_word()))
}

/// One MIDI-CI Device on its Group.
struct Member {
    group: MidiGroup,
    device: CiDevice<Entropy>,
    /// The Initiator of Profile Configuration with each peer that declared it.
    initiators: BTreeMap<Muid, ProfileInitiator>,
    /// The Initiator of Process Inquiry with each peer that declared it.
    process: BTreeMap<Muid, ProcessInitiator>,
}

impl Member {
    fn new(group: MidiGroup, device: CiDevice<Entropy>) -> Self {
        Self {
            group,
            device,
            initiators: BTreeMap::new(),
            process: BTreeMap::new(),
        }
    }
}

/// The devices running on one input, and what hears their SysEx.
struct Running {
    /// The identity they declare.
    identity: DeviceIdentity,
    members: Vec<Member>,
    assembler: SysexAssembler<Box<[u8]>>,
    /// The Groups whose device could not start and was reported: reported once
    /// until it starts, tried again at every poll. A Group that leaves the
    /// Groups and returns, or a new identity, is reported again.
    unstarted: Vec<MidiGroup>,
}

/// One input's MIDI-CI: the Groups it is to run on, once the negotiation or the
/// inquiry has decided them, and the devices while they run.
pub(crate) struct Ci {
    /// The device the input is, named in every Profile snapshot.
    device: DeviceId,
    groups: Vec<MidiGroup>,
    running: Option<Box<Running>>,
    /// Every peer found, by Group then MUID: rebuilt when a step changes them,
    /// so a read is a pointer clone, not a copy.
    peers: Arc<[CiPeerInfo]>,
    /// Each peer's Profiles, rebuilt with `peers`.
    profiles: Snapshot,
    /// Switches asked for, started by the next poll (it has the time).
    queue: Vec<ProfileSwitch>,
    /// The peers that can be asked for a MIDI Message Report, rebuilt with
    /// `peers`.
    reporters: process::Snapshot,
    /// Reports asked for, started by the next poll.
    reports: Vec<ReportAsk>,
}

/// Group 0, the Group of a MIDI 1.0 port and of an Endpoint that says nothing
/// of its blocks.
const GROUP_ZERO: MidiGroup = MidiGroup::of(0);

/// The Groups a settled Stream negotiation calls for (the module's "Where it
/// runs").
pub(crate) fn groups_after_settle(stream: &DeviceStream) -> Vec<MidiGroup> {
    let endpoint = stream.endpoint();
    if stream.basis() != crate::Basis::Negotiated || endpoint.function_blocks().next().is_none() {
        return vec![GROUP_ZERO];
    }
    let mut groups: Vec<MidiGroup> = endpoint
        .function_blocks()
        .filter(|block| block.midi_ci_version() != 0)
        .filter_map(|block| MidiGroup::new(block.first_group().get()))
        .collect();
    groups.sort_unstable();
    groups.dedup();
    groups
}

/// The Groups of a MIDI 1.0 port.
pub(crate) fn groups_of_a_midi1_port() -> Vec<MidiGroup> {
    vec![GROUP_ZERO]
}

impl Ci {
    /// MIDI-CI for the input `device`, with nothing decided.
    pub(crate) fn new(device: DeviceId) -> Self {
        Self {
            device,
            groups: Vec::new(),
            running: None,
            peers: Arc::default(),
            profiles: Arc::default(),
            queue: Vec::new(),
            reporters: Arc::default(),
            reports: Vec::new(),
        }
    }

    /// MIDI-CI is to run on `groups` when it is enabled.
    pub(crate) fn decide(&mut self, groups: Vec<MidiGroup>) {
        self.groups = groups;
    }

    /// Whether the port should copy SysEx7 packets to the stream ring: it is
    /// enabled (`enabled`) and there is a device to run, or one runs.
    pub(crate) fn hears_sysex(&self, enabled: bool) -> bool {
        enabled && (!self.groups.is_empty() || self.running.is_some())
    }

    /// One step at `now`, with the identity the app enabled MIDI-CI with, if it
    /// did: ends the devices that run under another identity (or none), starts
    /// them if they are to run and do not, else hands the running ones what the
    /// port heard and the time. What they send goes out on `out`. Returns the
    /// peers' changes.
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        heard: &Heard<'_>,
        enabled: Option<DeviceIdentity>,
        out: &mut OpenOutput,
    ) -> CiChanges {
        let mut changes = CiChanges::default();
        if self
            .running
            .as_ref()
            .is_some_and(|running| enabled != Some(running.identity))
            && let Some(running) = self.running.take()
        {
            running.stop(out, &mut changes);
        }
        match (&mut self.running, enabled) {
            (Some(running), _) => {
                running.reconcile(&self.groups, &mut entropy, now, out, &mut changes);
                running.step(now, heard, out, &mut changes);
                running.start_switches(&mut self.queue, now, out, &mut changes);
                running.start_reports(&mut self.reports, now, out, &mut changes);
            }
            (None, Some(identity)) if !self.groups.is_empty() => {
                let (running, started) =
                    Running::start(&self.groups, identity, &mut entropy, now, out);
                self.running = Some(Box::new(running));
                changes.peers.extend(started.peers);
                changes.profiles.extend(started.profiles);
                changes.process.extend(started.process);
                changes.unstarted.extend(started.unstarted);
            }
            _ => {}
        }
        self.queue.clear();
        self.reports.clear();
        if self.running.as_ref().is_some_and(|r| r.is_idle()) {
            self.running = None;
        }
        if !changes.is_empty() {
            self.peers = self.collect_peers();
            self.profiles = self.collect_profiles();
            self.reporters = self.collect_reporters();
        }
        changes
    }

    /// The input is closing: each running device announces its MUID invalid
    /// on `out` (M2-101 §5.9), best effort. **Nothing is reported to the app**:
    /// it closed the input, so it knows the peers went with it.
    pub(crate) fn close(&mut self, out: &mut OpenOutput) {
        if let Some(running) = self.running.take() {
            running.stop(out, &mut CiChanges::default());
        }
        self.peers = Arc::default();
        self.profiles = Arc::default();
        self.reporters = Arc::default();
        self.queue.clear();
        self.reports.clear();
    }

    /// Every peer's Profiles, by Group then MUID: only the peers that declared
    /// Profile Configuration have an entry.
    pub(crate) fn profiles(&self) -> Snapshot {
        Arc::clone(&self.profiles)
    }

    /// Asks that the peer of `switch` be sent it at the next poll, after
    /// checking the peer still lists the Profile at that address.
    pub(crate) fn set_profile(&mut self, switch: ProfileSwitch) -> Result<(), ProfileError> {
        let peer = self
            .profiles
            .iter()
            .find(|p| p.group() == switch.group && p.muid() == switch.muid)
            .ok_or(ProfileError::NoSuchPeer)?;
        let address = match switch.switch {
            Switch::On(target) => target.address(),
            Switch::Off(address) => address,
        };
        if peer.table().get(address, switch.profile).is_none() {
            return Err(ProfileError::NotListed);
        }
        self.queue.push(switch);
        Ok(())
    }

    /// The peers that can be asked for a MIDI Message Report, by Group then MUID.
    pub(crate) fn report_peers(&self) -> process::Snapshot {
        Arc::clone(&self.reporters)
    }

    /// Asks that the peer of `ask` be sent its request at the next poll, after
    /// checking it is still one that supports MIDI Message Report.
    pub(crate) fn report(&mut self, ask: ReportAsk) -> Result<(), ReportError> {
        if !self.reporters.contains(&ask.peer) {
            return Err(ReportError::NoSuchPeer);
        }
        self.reports.push(ask);
        Ok(())
    }

    /// Every peer found, by Group then MUID.
    pub(crate) fn peers(&self) -> Arc<[CiPeerInfo]> {
        Arc::clone(&self.peers)
    }

    fn collect_profiles(&self) -> Snapshot {
        let Some(running) = &self.running else {
            return Arc::default();
        };
        running
            .members
            .iter()
            .flat_map(|member| {
                member
                    .initiators
                    .iter()
                    .map(|(&muid, initiator)| PeerProfiles {
                        device: self.device.clone(),
                        group: member.group,
                        muid,
                        known: initiator.is_known(),
                        table: initiator.table().clone(),
                    })
            })
            .collect()
    }

    fn collect_reporters(&self) -> process::Snapshot {
        let Some(running) = &self.running else {
            return Arc::default();
        };
        running
            .members
            .iter()
            .flat_map(|member| {
                member
                    .process
                    .iter()
                    .filter(|(_, initiator)| {
                        initiator
                            .features()
                            .is_some_and(|f| f.contains(ProcessFeatures::MIDI_MESSAGE_REPORT))
                    })
                    .map(|(&muid, _)| ReportPeer {
                        device: self.device.clone(),
                        group: member.group,
                        muid,
                    })
            })
            .collect()
    }

    fn collect_peers(&self) -> Arc<[CiPeerInfo]> {
        let Some(running) = &self.running else {
            return Arc::default();
        };
        running
            .members
            .iter()
            .flat_map(|member| {
                member.device.peers().map(|(muid, peer)| CiPeerInfo {
                    id: PeerId {
                        group: member.group,
                        muid,
                    },
                    peer: peer.clone(),
                })
            })
            .collect()
    }
}

impl Running {
    /// Whether nothing runs and nothing waits to start: the devices are done.
    fn is_idle(&self) -> bool {
        self.members.is_empty() && self.unstarted.is_empty()
    }

    /// The devices of `groups`, each with its first Discovery sent on `out`. A
    /// Group whose device cannot start is reported in the changes and kept in
    /// [`unstarted`](Self::unstarted).
    fn start(
        groups: &[MidiGroup],
        identity: DeviceIdentity,
        entropy: &mut impl FnMut() -> Result<Entropy, EntropyError>,
        now: Instant,
        out: &mut OpenOutput,
    ) -> (Self, CiChanges) {
        let mut sends = Vec::new();
        let mut firsts = Vec::new();
        let mut unstarted = Vec::new();
        let mut failures = Vec::new();
        let members: Vec<Member> = groups
            .iter()
            .filter_map(|&group| match start_device(identity, entropy, now) {
                Ok((device, first)) => {
                    firsts.push(first);
                    Some(Member::new(group, device))
                }
                Err(error) => {
                    unstarted.push(group);
                    failures.push((group, error));
                    None
                }
            })
            .collect();
        let max = MAX_SYSEX.get() as usize;
        let lanes = members.len().max(1);
        let room = SysexAssembler::room_for(max, 1, lanes);
        let assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), max, lanes);
        let mut running = Self {
            identity,
            members,
            assembler,
            unstarted,
        };
        let mut changes = CiChanges {
            unstarted: failures,
            ..CiChanges::default()
        };
        for (member, actions) in running.members.iter_mut().zip(firsts) {
            member.apply(actions, now, &mut sends, &mut changes);
        }
        send(out, &sends);
        (running, changes)
    }

    /// Ends the devices: each announces that its MUID is no longer valid (an
    /// Invalidate MUID from and for its own MUID, M2-101 §5.9), and its peers
    /// are lost.
    fn stop(self, out: &mut OpenOutput, changes: &mut CiChanges) {
        stop_members(&self.members, out, changes);
    }

    /// Brings the devices to `groups` (the Groups MIDI-CI is now called for,
    /// sorted): a Group no longer called for ends its device (as
    /// [`stop`](Self::stop)), a new one starts a device with its first
    /// Discovery, and a Group that stays keeps its device and peers. A Group
    /// whose device cannot start is tried again each time and reported only when
    /// it first fails; nothing is rebuilt while the devices are the same.
    fn reconcile(
        &mut self,
        groups: &[MidiGroup],
        entropy: &mut impl FnMut() -> Result<Entropy, EntropyError>,
        now: Instant,
        out: &mut OpenOutput,
        changes: &mut CiChanges,
    ) {
        let (kept, gone): (Vec<Member>, Vec<Member>) = std::mem::take(&mut self.members)
            .into_iter()
            .partition(|m| groups.contains(&m.group));
        let mut changed = !gone.is_empty();
        stop_members(&gone, out, changes);
        self.members = kept;
        self.unstarted.retain(|g| groups.contains(g));
        let mut sends = Vec::new();
        let mut started = Vec::new();
        for &group in groups {
            if self.members.iter().any(|m| m.group == group) {
                continue;
            }
            match start_device(self.identity, entropy, now) {
                Ok((device, first)) => {
                    started.push((group, first));
                    self.unstarted.retain(|g| *g != group);
                    self.members.push(Member::new(group, device));
                    changed = true;
                }
                Err(error) => {
                    if !self.unstarted.contains(&group) {
                        self.unstarted.push(group);
                        changes.unstarted.push((group, error));
                    }
                }
            }
        }
        if !changed {
            return;
        }
        self.members.sort_by_key(|m| m.group);
        for (group, actions) in started {
            if let Some(member) = self.members.iter_mut().find(|m| m.group == group) {
                member.apply(actions, now, &mut sends, changes);
            }
        }
        // The reader is sized by its Groups; a message open across the change
        // is not finished.
        let max = MAX_SYSEX.get() as usize;
        let lanes = self.members.len().max(1);
        let room = SysexAssembler::room_for(max, 1, lanes);
        self.assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), max, lanes);
        send(out, &sends);
    }
}

/// A device with a MUID of its own and its first actions.
///
/// # Errors
///
/// [`EntropyError`] when `entropy` gives none for the MUID: the operating
/// system has no random bytes.
fn start_device(
    identity: DeviceIdentity,
    entropy: &mut impl FnMut() -> Result<Entropy, EntropyError>,
    now: Instant,
) -> Result<(CiDevice<Entropy>, Vec<CiAction>), EntropyError> {
    Ok(CiDevice::new(
        identity,
        CATEGORIES,
        MAX_SYSEX,
        entropy()?,
        now,
    ))
}

/// Each of `members` announces that its MUID is no longer valid (an Invalidate
/// MUID from and for its own MUID, M2-101 §5.9), and its peers are lost.
fn stop_members(members: &[Member], out: &mut OpenOutput, changes: &mut CiChanges) {
    let mut sends = Vec::new();
    for member in members {
        let muid = member.device.muid();
        sends.push((
            member.group,
            CiMessage::InvalidateMuid {
                header: CiHeader::new(muid),
                target: muid,
            },
        ));
        let peer = |muid| PeerId {
            group: member.group,
            muid,
        };
        changes.peers.extend(
            member
                .device
                .peers()
                .map(|(muid, _)| (peer(muid), CiChange::Lost)),
        );
        changes.profiles.extend(
            member
                .initiators
                .keys()
                .map(|&muid| (peer(muid), ProfilesChange::Dropped)),
        );
        changes.process.extend(
            member
                .process
                .keys()
                .map(|&muid| (peer(muid), ProcessChange::Dropped)),
        );
    }
    send(out, &sends);
}

impl Running {
    /// Hands the devices what the port heard and the time (see [`Ci::poll`]).
    fn step(
        &mut self,
        now: Instant,
        heard: &Heard<'_>,
        out: &mut OpenOutput,
        changes: &mut CiChanges,
    ) {
        let mut sends = Vec::new();
        for &ump in heard.sysex() {
            // Only a message of a Group that has a device is read.
            let Some(group) = ump.group() else { continue };
            if ump.message_type() != 0x3 || !self.members.iter().any(|m| m.group == group) {
                continue;
            }
            self.assembler.begin_block();
            let SysexStep::Complete(done) = self.assembler.push(ump) else {
                continue;
            };
            let decoded = CiMessage::decode(self.assembler.message(done).payload());
            if matches!(decoded, Decoded::NotCi) {
                continue;
            }
            for member in self.members.iter_mut().filter(|m| m.group == group) {
                let actions = member.device.on_message(&decoded, now);
                member.apply(actions, now, &mut sends, changes);
                member.route(&decoded, &mut sends, changes);
                member.route_process(&decoded, now, &mut sends, changes);
            }
        }
        if heard.gap() {
            // A message still open lost its tail: its next packets are not its.
            self.assembler.lost_tail();
        }
        for member in &mut self.members {
            if member.device.deadline().is_some_and(|at| now >= at) {
                let actions = member.device.on_timeout(now);
                member.apply(actions, now, &mut sends, changes);
            }
            member.expire(now, &mut sends, changes);
            member.expire_process(now, &mut sends, changes);
        }
        send(out, &sends);
    }

    /// Sends the switches the app asked for (see [`Ci::set_profile`]): each goes
    /// to the Initiator of its peer, which refuses one already waiting.
    fn start_switches(
        &mut self,
        queue: &mut Vec<ProfileSwitch>,
        now: Instant,
        out: &mut OpenOutput,
        changes: &mut CiChanges,
    ) {
        let mut sends = Vec::new();
        for switch in queue.drain(..) {
            let Some(member) = self.members.iter_mut().find(|m| m.group == switch.group) else {
                continue;
            };
            member.start_switch(&switch, now, &mut sends, changes);
        }
        send(out, &sends);
    }
}

impl Running {
    /// Sends the reports the app asked for (see [`Ci::report`]): each goes to the
    /// Initiator of its peer, which refuses one while another is waiting.
    fn start_reports(
        &mut self,
        asks: &mut Vec<ReportAsk>,
        now: Instant,
        out: &mut OpenOutput,
        changes: &mut CiChanges,
    ) {
        let mut sends = Vec::new();
        for ask in asks.drain(..) {
            let Some(member) = self.members.iter_mut().find(|m| m.group == ask.peer.group) else {
                continue;
            };
            member.start_report(&ask, now, &mut sends, changes);
        }
        send(out, &sends);
    }
}

impl Member {
    /// Carries out `actions`: what is to be sent goes in `sends`, what changed
    /// for the app in `changes`. A peer that declared Profile Configuration
    /// gets an Initiator, which asks for its Profiles; one that is lost loses it.
    fn apply(
        &mut self,
        actions: Vec<CiAction>,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        for action in actions {
            match action {
                CiAction::Send(message) => sends.push((self.group, message)),
                CiAction::Found(muid) => {
                    changes.peers.extend(
                        self.info(muid)
                            .map(|info| (info.id, CiChange::Found(info.peer))),
                    );
                    self.initiate(muid, now, sends, changes);
                    self.initiate_process(muid, now, sends, changes);
                }
                CiAction::Updated(muid) => {
                    changes.peers.extend(
                        self.info(muid)
                            .map(|info| (info.id, CiChange::Updated(info.peer))),
                    );
                }
                CiAction::Lost(muid) => {
                    let id = self.peer_id(muid);
                    changes.peers.push((id, CiChange::Lost));
                    if self.initiators.remove(&muid).is_some() {
                        changes.profiles.push((id, ProfilesChange::Dropped));
                    }
                    if self.process.remove(&muid).is_some() {
                        changes.process.push((id, ProcessChange::Dropped));
                    }
                }
                // The device's own MUID changing reaches the app as every peer
                // lost (and so every Initiator dropped); the window closing is
                // the core's own bookkeeping.
                _ => {}
            }
        }
    }

    const fn peer_id(&self, muid: Muid) -> PeerId {
        PeerId {
            group: self.group,
            muid,
        }
    }

    fn info(&self, muid: Muid) -> Option<CiPeerInfo> {
        self.device.peer(muid).map(|peer| CiPeerInfo {
            id: self.peer_id(muid),
            peer: peer.clone(),
        })
    }

    /// A found peer that declared Profile Configuration is asked for its
    /// Profiles (M2-101 §7.2): by a new Initiator, or, for one that answered
    /// Discovery again, by the old one asking again.
    fn initiate(
        &mut self,
        muid: Muid,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let declares = self.device.peer(muid).is_some_and(|peer| {
            peer.discovery()
                .categories
                .contains(CiCategories::PROFILE_CONFIGURATION)
        });
        if !declares {
            return;
        }
        let actions = match self.initiators.get_mut(&muid) {
            Some(initiator) => initiator.inquire(now),
            None => {
                let (initiator, first) = ProfileInitiator::new(self.device.muid(), muid, now);
                self.initiators.insert(muid, initiator);
                first
            }
        };
        self.carry_out(muid, actions, sends, changes);
    }

    /// A found peer that declared Process Inquiry is asked for its capabilities
    /// (M2-101 §9.2): by a new Initiator, or, for one that answered Discovery
    /// again, by the old one asking again.
    fn initiate_process(
        &mut self,
        muid: Muid,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let declares = self.device.peer(muid).is_some_and(|peer| {
            peer.discovery()
                .categories
                .contains(CiCategories::PROCESS_INQUIRY)
        });
        if !declares {
            return;
        }
        let actions = match self.process.get_mut(&muid) {
            Some(initiator) => initiator.inquire(now),
            None => {
                let (initiator, first) = ProcessInitiator::new(self.device.muid(), muid, now);
                self.process.insert(muid, initiator);
                first
            }
        };
        self.carry_out_process(muid, actions, sends, changes);
    }

    /// Hands every Process Inquiry Initiator the message, which is theirs to keep
    /// or ignore.
    fn route_process(
        &mut self,
        decoded: &Decoded,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let heard: Vec<_> = self
            .process
            .iter_mut()
            .map(|(&muid, initiator)| (muid, initiator.on_message(decoded, now)))
            .collect();
        for (muid, actions) in heard {
            self.carry_out_process(muid, actions, sends, changes);
        }
    }

    /// Ends every wait of the Process Inquiry Initiators that has run out.
    fn expire_process(
        &mut self,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let ended: Vec<_> = self
            .process
            .iter_mut()
            .filter(|(_, initiator)| initiator.deadline().is_some_and(|at| now >= at))
            .map(|(&muid, initiator)| (muid, initiator.on_timeout(now)))
            .collect();
        for (muid, actions) in ended {
            self.carry_out_process(muid, actions, sends, changes);
        }
    }

    fn start_report(
        &mut self,
        ask: &ReportAsk,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let Some(initiator) = self.process.get_mut(&ask.peer.muid) else {
            return;
        };
        match initiator.report(ask.request, now) {
            Ok(actions) => self.carry_out_process(ask.peer.muid, actions, sends, changes),
            Err(error) => changes
                .process
                .push((self.peer_id(ask.peer.muid), ProcessChange::NotSent(error))),
        }
    }

    /// What a Process Inquiry Initiator asks for: messages to send, changes to tell.
    fn carry_out_process(
        &self,
        muid: Muid,
        actions: Vec<ProcessAction>,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let id = self.peer_id(muid);
        for action in actions {
            let change = match action {
                ProcessAction::Send(message) => {
                    sends.push((self.group, message));
                    continue;
                }
                ProcessAction::Capabilities(features) => ProcessChange::Capabilities(features),
                ProcessAction::CapabilitiesTimedOut => ProcessChange::CapabilitiesTimedOut,
                ProcessAction::CapabilitiesRefused(_) => ProcessChange::CapabilitiesRefused,
                ProcessAction::ReportBegan { declared } => ProcessChange::ReportBegan(declared),
                ProcessAction::ReportEnded => ProcessChange::ReportEnded,
                ProcessAction::ReportRefused(nak) => ProcessChange::ReportRefused(nak),
                ProcessAction::ReportTimedOut => ProcessChange::ReportTimedOut,
                _ => continue,
            };
            changes.process.push((id, change));
        }
    }

    /// Hands every Initiator the message, which is theirs to keep or ignore.
    fn route(
        &mut self,
        decoded: &Decoded,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let heard: Vec<_> = self
            .initiators
            .iter_mut()
            .map(|(&muid, initiator)| (muid, initiator.on_message(decoded)))
            .collect();
        for (muid, actions) in heard {
            self.carry_out(muid, actions, sends, changes);
        }
    }

    /// Ends every wait of the Initiators that has run out.
    fn expire(
        &mut self,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let ended: Vec<_> = self
            .initiators
            .iter_mut()
            .filter(|(_, initiator)| initiator.deadline().is_some_and(|at| now >= at))
            .map(|(&muid, initiator)| (muid, initiator.on_timeout(now)))
            .collect();
        for (muid, actions) in ended {
            self.carry_out(muid, actions, sends, changes);
        }
    }

    fn start_switch(
        &mut self,
        switch: &ProfileSwitch,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let Some(initiator) = self.initiators.get_mut(&switch.muid) else {
            return;
        };
        let started = match switch.switch {
            Switch::On(target) => initiator.set_on(target, switch.profile, now),
            Switch::Off(address) => initiator.set_off(address, switch.profile, now),
        };
        match started {
            Ok(actions) => self.carry_out(switch.muid, actions, sends, changes),
            Err(error) => changes.profiles.push((
                self.peer_id(switch.muid),
                ProfilesChange::NotSent {
                    profile: switch.profile,
                    error,
                },
            )),
        }
    }

    /// What an Initiator asks for: messages to send, changes to tell.
    fn carry_out(
        &self,
        muid: Muid,
        actions: Vec<ProfileAction>,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let id = self.peer_id(muid);
        for action in actions {
            let change = match action {
                ProfileAction::Send(message) => {
                    sends.push((self.group, message));
                    continue;
                }
                ProfileAction::ProfilesKnown => ProfilesChange::Known,
                ProfileAction::InquiryTimedOut => ProfilesChange::InquiryTimedOut,
                ProfileAction::InquiryRefused(_) => ProfilesChange::InquiryRefused,
                ProfileAction::Changed {
                    address,
                    profile,
                    change,
                } => ProfilesChange::Changed {
                    address,
                    profile,
                    change,
                },
                ProfileAction::Resolved { request, outcome } => {
                    ProfilesChange::Resolved { request, outcome }
                }
                ProfileAction::SpecificData {
                    address,
                    profile,
                    data,
                } => ProfilesChange::SpecificData {
                    address,
                    profile,
                    data,
                },
                _ => continue,
            };
            changes.profiles.push((id, change));
        }
    }
}

/// Queues the messages as SysEx7 on `out`, each in one batch. A refusal is not
/// retried: Discovery and its replies are repeated by their protocol, and a
/// lost one is a peer found later or never. A device takes a batch as a prefix
/// (`MidiOut::queue`), so a message taken in part is a SysEx7 whose end the
/// device never gets; it is logged as cut, apart from one wholly refused.
fn send(out: &mut OpenOutput, sends: &[(MidiGroup, CiMessage)]) {
    for (group, message) in sends {
        let packets: Vec<_> = message
            .packets(*group)
            .map(|ump| (Offset::ZERO, ump))
            .collect();
        match out.queue(&packets) {
            taken if taken == packets.len() => {}
            0 => tracing::debug!("a MIDI-CI message was refused by {}", out.name()),
            taken => tracing::debug!(
                "a MIDI-CI message was cut after {taken} of {} packets by {}",
                packets.len(),
                out.name()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use tutti_midi_device::{CiCategories, DiscoveryData, EndpointId, PortDirection};
    use tutti_ump::{ManufacturerId, U7};

    use super::*;
    use crate::MidiSession;
    use crate::test_support::{FakeBackend, FakeCounters, endpoint};

    fn identity() -> DeviceIdentity {
        DeviceIdentity {
            manufacturer: ManufacturerId::new(0x41).unwrap(),
            family: [U7::of(1), U7::of(2)],
            model: [U7::of(3), U7::of(4)],
            revision: [U7::of(5), U7::of(6), U7::of(7), U7::of(8)],
        }
    }

    /// The output of a fake device, and the counters of what it was sent.
    fn output() -> (OpenOutput, FakeCounters) {
        let (backend, counters) = FakeBackend::build_with(vec![], vec![], usize::MAX);
        let inlets = tutti_midi_runtime::DeviceInlets::new(
            NonZeroUsize::new(4).unwrap(),
            tutti_graph::EventCapacity::of(64),
            Duration::ZERO,
        );
        let session = MidiSession::open_over(backend, inlets).unwrap();
        counters.plug(endpoint(7, "Wire"), PortDirection::Duplex);
        let id = session.outputs().first().expect("the device").id.clone();
        (session.connect_output(&id).unwrap(), counters)
    }

    /// An entropy source that refuses draws `from..to` (counted from 1) and
    /// gives every other, with the count it keeps.
    fn refusing(from: usize, to: usize) -> impl FnMut() -> Result<Entropy, EntropyError> {
        let mut draws = 0;
        move || {
            draws += 1;
            if (from..to).contains(&draws) {
                Err(EntropyError::unavailable())
            } else {
                entropy()
            }
        }
    }

    fn heard(packets: &[Ump]) -> Heard<'_> {
        Heard {
            packets,
            lost_before: false,
            lost_during: false,
        }
    }

    fn group_of(ump: &Ump) -> u8 {
        ((ump.0[0] >> 24) & 0xF) as u8
    }

    const G0: MidiGroup = MidiGroup::of(0);
    const G1: MidiGroup = MidiGroup::of(1);

    // A Group whose device cannot start is reported once, however many polls
    // retry it, and the retries do not rebuild the reader, so a message open on
    // a Group that did start is finished; the Group starts when the source gives.
    #[test]
    fn a_group_that_cannot_start_is_reported_once_and_retried_without_dropping_a_message() {
        let (mut out, counters) = output();
        let raw = EndpointId::from_raw(7);
        // Group 0 takes the first draw; Group 1 is refused the next three.
        let mut source = refusing(2, 5);
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);

        let (mut running, first) =
            Running::start(&[G0, G1], identity(), &mut source, at(0), &mut out);
        assert_eq!(first.unstarted, [(G1, EntropyError::unavailable())]);
        let _ = counters.take_sent(raw);

        // A peer's Discovery arrives on Group 0 in two polls; Group 1 is refused
        // again in both and is not reported again.
        let message = CiMessage::Discovery {
            header: CiHeader::new(Muid::of(0x0012_3456)),
            data: DiscoveryData::new(identity(), CiCategories::empty(), MaxSysexSize::of(256)),
        };
        let packets: Vec<Ump> = message.packets(G0).collect();
        assert!(packets.len() >= 2);
        let (head, tail) = packets.split_at(packets.len() / 2);
        for (ms, part) in [(1, head), (2, tail)] {
            let mut changes = CiChanges::default();
            running.reconcile(&[G0, G1], &mut source, at(ms), &mut out, &mut changes);
            running.step(at(ms), &heard(part), &mut out, &mut changes);
            assert!(changes.unstarted.is_empty(), "reported once");
            if ms == 1 {
                assert!(
                    counters.take_sent(raw).is_empty(),
                    "the message is not whole yet"
                );
            }
        }
        assert!(
            !counters.take_sent(raw).is_empty(),
            "the message survived the retries and was answered"
        );

        // The next draw succeeds: Group 1 starts and sends its Discovery.
        let mut changes = CiChanges::default();
        running.reconcile(&[G0, G1], &mut source, at(3), &mut out, &mut changes);
        assert!(changes.unstarted.is_empty());
        assert!(running.unstarted.is_empty());
        let sent = counters.take_sent(raw);
        assert!(!sent.is_empty() && sent.iter().all(|u| group_of(u) == 1));
    }

    // Every Group refused: each is reported once over several polls, and the
    // devices are not idle, so `Ci::poll` keeps them and retries.
    #[test]
    fn when_every_group_is_refused_each_is_reported_once_and_the_retry_goes_on() {
        let (mut out, _counters) = output();
        let mut source = refusing(1, 100);
        let now = Instant::now();
        let (mut running, first) =
            Running::start(&[G0, G1], identity(), &mut source, now, &mut out);
        assert_eq!(
            first.unstarted,
            [
                (G0, EntropyError::unavailable()),
                (G1, EntropyError::unavailable())
            ]
        );
        assert!(!running.is_idle(), "the Groups still wait to start");
        for _ in 0..3 {
            let mut changes = CiChanges::default();
            running.reconcile(&[G0, G1], &mut source, now, &mut out, &mut changes);
            assert!(changes.unstarted.is_empty(), "reported once");
            assert!(!running.is_idle());
        }
        // No Group is called for: nothing waits and nothing runs.
        let mut changes = CiChanges::default();
        running.reconcile(&[], &mut source, now, &mut out, &mut changes);
        assert!(running.is_idle());
    }

    // A Group that leaves the Groups while it is refused and returns is a new
    // failure: it is reported again.
    #[test]
    fn a_group_that_leaves_and_returns_while_refused_is_reported_again() {
        let (mut out, _counters) = output();
        let mut source = refusing(1, 100);
        let now = Instant::now();
        let (mut running, first) = Running::start(&[G0], identity(), &mut source, now, &mut out);
        assert_eq!(first.unstarted.len(), 1);
        let mut changes = CiChanges::default();
        running.reconcile(&[G1], &mut source, now, &mut out, &mut changes);
        assert_eq!(changes.unstarted, [(G1, EntropyError::unavailable())]);
        let mut changes = CiChanges::default();
        running.reconcile(&[G0, G1], &mut source, now, &mut out, &mut changes);
        assert_eq!(changes.unstarted, [(G0, EntropyError::unavailable())]);
    }
}
