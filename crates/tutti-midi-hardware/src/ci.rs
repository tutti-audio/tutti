//! MIDI-CI on a device's wire: the Management side of
//! [`CiDevice`] (Discovery, Invalidate MUID, NAK)
//! run on an open input, the peers it finds
//! ([`CiPeerInfo`], reported as [`CiChange`]), and Profile Configuration
//! with those that declare it (reported as [`ProfilesChange`]), Process Inquiry with
//! those that declare that (reported as [`ProcessChange`]), and Property Exchange with
//! those that declare it (reported as [`PropertyChange`], and a call's end as an
//! [`Answer`]).
//!
//! # Presenting an identity
//!
//! **It runs only while the session presents an identity**
//! ([`MidiSession::present`](crate::MidiSession::present)). A Discovery
//! announces a System Exclusive manufacturer id, and tutti has none of its own:
//! M1 reserves `7D` for non-commercial use, never for a released product, and
//! tutti is a library that apps ship. So no device is started, and the port
//! copies no SysEx for MIDI-CI, until the app presents who it is, with the id it
//! registered. It reaches every input (a device starts at the first poll after
//! the input's negotiation settled and any Identity Request in progress has
//! ended). A different identity, or none, ends the devices that ran and starts
//! the new identity's in the same poll (see "When it ends").
//!
//! # When it ends
//!
//! A device ends when its Group is no
//! longer called for: a settled Endpoint whose Function Block changes (the
//! discovery keeps hearing them unless the Endpoint declared them static)
//! is read again, and the Groups recomputed. A Group that stays keeps its
//! device and peers; one that goes ends its device (Invalidate MUID, peers
//! lost: M2-101 §3.3.2, a block that changes its Group location or number of
//! Groups invalidates its MUID); a new one starts a device (Discovery). A
//! change of the presented identity ends every device the same way (Invalidate
//! MUID, peers lost, Profiles dropped) and starts the new identity's. **A
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
//! It runs while the session has an identity and the input is open: the Discovery it sends when it
//! starts is only the first round, and it answers a Discovery or an Invalidate
//! MUID from anyone, at any time (M2-101 Appendix E: a Responder). It speaks
//! Management, as the Initiator Profile Configuration, Process Inquiry and Property
//! Exchange, and as the Responder Property Exchange; every other message addressed to
//! it is NAK'd by the core.
//!
//! # Profiles
//!
//! A peer whose Discovery Reply declares Profile Configuration
//! ([`CiCategories::PROFILE_CONFIGURATION`]) gets a [`ProfileInitiator`], made
//! when it is found: it sends a Profile Inquiry to the Function Block, keeps the
//! Profiles the peer answers with and the reports it sends after
//! ([`MidiSession::profiles`](crate::MidiSession::profiles)), and is stepped by
//! the same poll and its time as the device is. It sends on the same output.
//!
//! An app's request to enable or disable a Profile
//! ([`MidiSession::set_profile`](crate::MidiSession::set_profile)) is queued and
//! started by the next poll, which has the time its wait counts from. Its end is
//! an [`Answer::Switch`]. When the peer is lost (an Invalidate MUID, a collision,
//! a changed MUID of the device or its Group ending) its Initiator and Profiles
//! go, [`ProfilesChange::Dropped`] says so, and a switch still waiting ends with
//! an [`Answer::SwitchLost`] if it was not yet sent. Closing the input reports
//! nothing, as for peers.
//!
//! # Process Inquiry
//!
//! A peer whose Discovery Reply declares Process Inquiry
//! ([`CiCategories::PROCESS_INQUIRY`]) gets a [`ProcessInitiator`], made when it
//! is found: it sends an Inquiry: Process Inquiry Capabilities to the Function
//! Block, and a peer whose reply says it supports MIDI Message Report is listed
//! ([`MidiSession::report_peers`](crate::MidiSession::report_peers)).
//!
//! An app's request for a report
//! ([`MidiSession::report`](crate::MidiSession::report)) is queued and started
//! by the next poll. The messages of a report are not CI: they are the peer's
//! MIDI, and reach the graph with the rest of the input, before the
//! [`ReportEnd::Ended`] that is the report's [`Answer`]. A lost peer's Initiator
//! goes with it ([`ProcessChange::Dropped`]).
//!
//! # Property Exchange
//!
//! A peer whose Discovery Reply declares Property Exchange
//! ([`CiCategories::PROPERTY_EXCHANGE`]) gets a [`ResourceInitiator`], made when it is
//! found: it sends an Inquiry: Property Exchange Capabilities, then gets the peer's
//! ResourceList, and a peer that answered is listed
//! ([`MidiSession::property_peers`](crate::MidiSession::property_peers)). An app's call
//! ([`MidiSession::ask_property`](crate::MidiSession::ask_property)) is queued and
//! started by the next poll; its end is an [`Answer::Got`] or [`Answer::Set`]. Each
//! member device also holds one [`ResourceResponder`] serving the store the app gave
//! ([`MidiSession::serve_properties`](crate::MidiSession::serve_properties), empty
//! until then) to the peers that declared the category: the member routes it every
//! message, runs its deadline on the same step as the device's, and sends its replies
//! in the same batch. When the device's MUID changes the Responder is made again for
//! the new one, serves the session's store, handed to it on each call, and holds no Subscription. A lost
//! peer's Initiator and Subscriptions go with it ([`PropertyChange::Dropped`]); a repeated
//! Discovery of a peer keeps its Subscriptions. An update of a Subscription the
//! session holds, and the end of one, go to its Initiator and not to the Responder. The store is served as not subscribable
//! (there is no way to publish to a subscriber through the session), and an inquiry of a
//! peer the Responder does not serve is NAK'd.
//!
//! The port copies the device's SysEx7 packets to the stream ring while any
//! device is to run (`hears_sysex`); the graph still gets them all. They are
//! reassembled per Group, decoded, and routed to that Group's device. **A gap
//! in the ring never joins a message**: the port counts what the ring had no
//! room for, the session tells the reader ([`Heard`]), and the assembler drops
//! its open messages ([`SysexAssembler::lost_tail`]) before the next packets.
//! What a device sends goes out as SysEx7 on its Group, on the output of the
//! negotiation, never the session's MIDI output; a refused send is logged,
//! counted in the `refused` cell that
//! [`MidiSession::connect_input_with`](crate::MidiSession::connect_input_with)
//! is given, and not repeated, since the protocol is built to tolerate loss.
//!
//! # What it declares
//!
//! - **Identity**: the app's, as given to [`MidiSession::open`](crate::MidiSession::open).
//! - **Categories**: Profile Configuration, Process Inquiry and Property Exchange. The
//!   devices initiate all three and answer Property Exchange from their store.
//!   They answer a Profile Inquiry with no Profiles (M2-101 §7.13 allows a device
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
//!   leaves the Groups and returns is reported again) and
//!   the Group is tried again at every poll.
//!
//! Time is an argument: nothing here reads a clock.

use std::collections::BTreeMap;
use std::sync::Arc;
use tutti_platform::Instant;

use tutti_graph::{Losses, Offset};
use tutti_midi_ci::Muid;
use tutti_midi_ci::{
    CiAction, CiCategories, CiDevice, CiHeader, CiMessage, CiPeer, Decoded, Initiator,
    MaxSysexSize, ProcessAction, ProcessError, ProcessFeatures, ProcessInitiator, ProfileAction,
    ProfileInitiator, ProfileRequestError, ResourceInitiator, ResourceResponder, ResourceStore,
};
use tutti_midi_device::DeviceId;
use tutti_rt::CounterWriter;
use tutti_rt::SplitMix64;
use tutti_ump::{DeviceIdentity, MidiGroup, SysexAssembler, SysexStep, Ump};

use crate::answer::{Answer, ReportEnd};
use crate::devices::OpenOutput;
use crate::peer::{PeerError, PeerRef};
use crate::process::{self, ProcessChange, ReportAsk, Reporter};
use crate::profiles::{
    PeerProfiles, ProfileError, ProfileSwitch, ProfilesChange, Snapshot, Switch,
};
use crate::properties::{self, PropertyAsk, PropertyChange, PropertyPeer};
use crate::stream::DeviceStream;

mod property;

/// The Receivable Maximum SysEx Message Size the session declares: 512 bytes
/// (see the module).
pub(crate) const MAX_SYSEX: MaxSysexSize = MaxSysexSize::of(MaxSysexSize::INITIATOR_MIN);

/// What the devices declare: Management, Profile Configuration, Process
/// Inquiry and Property Exchange, which the session initiates with every peer that
/// declares them too (the
/// module's "What it does"). They answer a Profile Inquiry with no Profiles, and NAK a
/// Set Profile On or Off, since they have none to enable; they answer a Process
/// Inquiry with the report of a device with no MIDI state.
const CATEGORIES: CiCategories = CiCategories::PROFILE_CONFIGURATION
    .union(CiCategories::PROCESS_INQUIRY)
    .union(CiCategories::PROPERTY_EXCHANGE);

/// What one step of an input's MIDI-CI changed, for the app.
#[derive(Default)]
pub(crate) struct CiChanges {
    /// Peers found, updated or lost.
    pub(crate) peers: Vec<(PeerId, CiChange)>,
    /// Peers' Profiles.
    pub(crate) profiles: Vec<(PeerId, ProfilesChange)>,
    /// Peers' Process Inquiry.
    pub(crate) process: Vec<(PeerId, ProcessChange)>,
    /// Peers' Property Exchange.
    pub(crate) properties: Vec<(PeerId, PropertyChange)>,
    /// How the requests asked of peers ended.
    pub(crate) answers: Vec<(PeerId, Answer)>,
    /// The Groups whose device could not start, with why.
    pub(crate) unstarted: Vec<(MidiGroup, getrandom::Error)>,
    /// The CI messages the output refused in this step.
    pub(crate) refused: Losses,
}

impl CiChanges {
    fn is_empty(&self) -> bool {
        self.peers.is_empty()
            && self.profiles.is_empty()
            && self.process.is_empty()
            && self.properties.is_empty()
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
    /// own MUID (which a collision changes).
    Lost,
}

/// The MUID source: words from a stream the operating system seeded.
type Entropy = Box<dyn FnMut() -> u32 + Send>;

/// A source of words for MUIDs (the module's "MUIDs"), seeded on its own for
/// each device.
///
/// # Errors
///
/// [`getrandom::Error`] when the operating system has no random bytes to seed it.
fn entropy() -> Result<Entropy, getrandom::Error> {
    let mut seed = [0u8; 8];
    getrandom::fill(&mut seed)?;
    let mut words = SplitMix64::new(u64::from_le_bytes(seed));
    Ok(Box::new(move || words.next_u32()))
}

/// One MIDI-CI Device on its Group.
struct Member {
    group: MidiGroup,
    device: CiDevice<Entropy>,
    /// The Initiator of Profile Configuration with each peer that declared it.
    initiators: BTreeMap<Muid, ProfileInitiator>,
    /// The Initiator of Process Inquiry with each peer that declared it.
    process: BTreeMap<Muid, ProcessInitiator>,
    /// The Initiator of Property Exchange with each peer that declared it.
    properties: BTreeMap<Muid, ResourceInitiator>,
    /// The Subscription messages of a peer still arriving that are the Initiator's
    /// (an update, or the end of a Subscription it holds), by peer and Request ID.
    updates: Vec<(Muid, tutti_midi_ci::PeRequestId)>,
    /// The Responder that serves the device's store to those peers.
    server: ResourceResponder,
}

impl Member {
    fn new(group: MidiGroup, device: CiDevice<Entropy>) -> Self {
        let server = property::responder(device.muid());
        Self {
            group,
            device,
            initiators: BTreeMap::new(),
            process: BTreeMap::new(),
            properties: BTreeMap::new(),
            updates: Vec::new(),
            server,
        }
    }
}

/// What each Initiator of `initiators` does with the message `decoded`, by peer.
fn hear_all<I: Initiator>(
    initiators: &mut BTreeMap<Muid, I>,
    decoded: &Decoded,
    now: Instant,
) -> Vec<(Muid, Vec<I::Action>)> {
    initiators
        .iter_mut()
        .map(|(&muid, initiator)| (muid, initiator.on_message(decoded, now)))
        .collect()
}

/// What each Initiator of `initiators` whose wait has run out does about it, by
/// peer.
fn expire_all<I: Initiator>(
    initiators: &mut BTreeMap<Muid, I>,
    now: Instant,
) -> Vec<(Muid, Vec<I::Action>)> {
    initiators
        .iter_mut()
        .filter(|(_, initiator)| initiator.deadline().is_some_and(|at| now >= at))
        .map(|(&muid, initiator)| (muid, initiator.on_timeout(now)))
        .collect()
}

/// The devices running on one input, and what hears their SysEx.
struct Running {
    /// The identity they declare.
    identity: DeviceIdentity,
    members: Vec<Member>,
    assembler: SysexAssembler<Box<[u8]>>,
    /// The Groups whose device could not start and was reported: reported once
    /// until it starts, tried again at every poll. A Group that leaves the
    /// Groups and returns is reported again.
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
    /// The peers that can be asked for Property Exchange, rebuilt with `peers`.
    property_peers: properties::Snapshot,
    /// Property Exchange calls asked for, started by the next poll.
    calls: Vec<PropertyAsk>,
    /// Counts the CI messages the output has refused: a cell of the port that
    /// owns the conversation, which outlives the connection.
    refused: CounterWriter<Losses>,
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
    pub(crate) fn new(device: DeviceId, refused: CounterWriter<Losses>) -> Self {
        Self {
            device,
            groups: Vec::new(),
            running: None,
            peers: Arc::default(),
            profiles: Arc::default(),
            queue: Vec::new(),
            reporters: Arc::default(),
            reports: Vec::new(),
            property_peers: Arc::default(),
            calls: Vec::new(),
            refused,
        }
    }

    /// MIDI-CI is to run on `groups` when the session has an identity.
    pub(crate) fn decide(&mut self, groups: Vec<MidiGroup>) {
        self.groups = groups;
    }

    /// Whether the port should copy SysEx7 packets to the stream ring: the
    /// session has an identity (`has_identity`) and there is a device to run,
    /// or one runs.
    pub(crate) fn hears_sysex(&self, has_identity: bool) -> bool {
        has_identity && (!self.groups.is_empty() || self.running.is_some())
    }

    /// One step at `now`, with the identity the session runs MIDI-CI with, if
    /// it has one: starts the devices if they are to run and do not, else hands
    /// the running ones what the port heard and the time. What they send goes
    /// out on `out`. Returns the peers' changes.
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        heard: &Heard<'_>,
        identity: Option<DeviceIdentity>,
        store: &mut ResourceStore,
        out: &mut OpenOutput,
    ) -> CiChanges {
        let mut changes = CiChanges::default();
        // Who the session is changed, or it presents no one: the devices that
        // ran under the old identity end (Invalidate MUID, peers lost) and the
        // new identity, if any, starts them again below.
        if self
            .running
            .as_ref()
            .is_some_and(|running| Some(running.identity) != identity)
            && let Some(running) = self.running.take()
        {
            running.stop(out, &mut changes);
        }
        match (&mut self.running, identity) {
            (Some(running), _) => {
                running.reconcile(&self.groups, &mut entropy, now, out, &mut changes);
                running.step(store, now, heard, out, &mut changes);
                running.start_switches(&mut self.queue, now, out, &mut changes);
                running.start_reports(&mut self.reports, now, out, &mut changes);
                running.start_properties(&mut self.calls, now, out, &mut changes);
            }
            (None, Some(identity)) if !self.groups.is_empty() => {
                let (running, started) =
                    Running::start(&self.groups, identity, &mut entropy, now, out);
                self.running = Some(Box::new(running));
                changes.peers.extend(started.peers);
                changes.profiles.extend(started.profiles);
                changes.process.extend(started.process);
                changes.properties.extend(started.properties);
                changes.refused += started.refused;
                changes.unstarted.extend(started.unstarted);
            }
            _ => {}
        }
        self.queue.clear();
        self.reports.clear();
        self.calls.clear();
        self.refused.add(changes.refused);
        if self.running.as_ref().is_some_and(|r| r.is_idle()) {
            self.running = None;
        }
        if !changes.is_empty() {
            self.peers = self.collect_peers();
            self.profiles = self.collect_profiles();
            self.reporters = self.collect_reporters();
            self.property_peers = self.collect_property_peers();
        }
        changes
    }

    /// The input is closing: each running device announces its MUID invalid
    /// on `out` (M2-101 §5.9), best effort. **Nothing is reported to the app**:
    /// it closed the input, so it knows the peers went with it; the input and
    /// this state are dropped by the caller.
    pub(crate) fn close(&mut self, out: &mut OpenOutput) {
        if let Some(running) = self.running.take() {
            running.stop(out, &mut CiChanges::default());
        }
    }

    /// Every peer's Profiles, by Group then MUID: only the peers that declared
    /// Profile Configuration have an entry.
    pub(crate) fn profiles(&self) -> Snapshot {
        Arc::clone(&self.profiles)
    }

    /// Asks that the peer of `switch` be sent it at the next poll, after
    /// checking the peer still lists the Profile at that address.
    pub(crate) fn set_profile(&mut self, switch: ProfileSwitch) -> Result<(), ProfileError> {
        switch.check(self.profiles.iter())?;
        self.queue.push(switch);
        Ok(())
    }

    /// The peers that can be asked for a MIDI Message Report, by Group then MUID.
    pub(crate) fn report_peers(&self) -> process::Snapshot {
        Arc::clone(&self.reporters)
    }

    /// Asks that the peer of `ask` be sent its request at the next poll, after
    /// checking it is still one that supports MIDI Message Report.
    pub(crate) fn report(&mut self, ask: ReportAsk) -> Result<(), PeerError> {
        ask.check(self.reporters.iter())?;
        self.reports.push(ask);
        Ok(())
    }

    /// The peers that can be asked for Property Exchange, by Group then MUID.
    pub(crate) fn property_peers(&self) -> properties::Snapshot {
        Arc::clone(&self.property_peers)
    }

    /// Asks that the call of `ask` be made of its peer at the next poll, after
    /// checking it is still one that answered its Capabilities.
    pub(crate) fn ask_property(&mut self, ask: PropertyAsk) -> Result<(), PeerError> {
        ask.check(self.property_peers.iter())?;
        self.calls.push(ask);
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
                        at: member.peer_ref(&self.device, muid),
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
                    .map(|(&muid, _)| Reporter {
                        at: member.peer_ref(&self.device, muid),
                    })
            })
            .collect()
    }

    fn collect_property_peers(&self) -> properties::Snapshot {
        let Some(running) = &self.running else {
            return Arc::default();
        };
        running
            .members
            .iter()
            .flat_map(|member| {
                member
                    .properties
                    .iter()
                    .filter(|(_, initiator)| initiator.transactions().capabilities().is_some())
                    .map(|(&muid, initiator)| PropertyPeer {
                        at: member.peer_ref(&self.device, muid),
                        resources: Arc::new(initiator.resource_list().cloned().unwrap_or_default()),
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
        entropy: &mut impl FnMut() -> Result<Entropy, getrandom::Error>,
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
        changes.refused += send(out, &sends);
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
    /// whose device cannot start is tried again each time and reported once per
    /// failure episode (until it starts or leaves the Groups); nothing is
    /// rebuilt while the devices are the same.
    fn reconcile(
        &mut self,
        groups: &[MidiGroup],
        entropy: &mut impl FnMut() -> Result<Entropy, getrandom::Error>,
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
        changes.refused += send(out, &sends);
    }
}

/// A device with a MUID of its own and its first actions.
///
/// # Errors
///
/// [`getrandom::Error`] when `entropy` gives none for the MUID: the operating
/// system has no random bytes.
fn start_device(
    identity: DeviceIdentity,
    entropy: &mut impl FnMut() -> Result<Entropy, getrandom::Error>,
    now: Instant,
) -> Result<(CiDevice<Entropy>, Vec<CiAction>), getrandom::Error> {
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
        changes.properties.extend(
            member
                .properties
                .keys()
                .map(|&muid| (peer(muid), PropertyChange::Dropped)),
        );
    }
    changes.refused += send(out, &sends);
}

impl Running {
    /// Hands the devices what the port heard and the time (see [`Ci::poll`]).
    fn step(
        &mut self,
        store: &mut ResourceStore,
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
                member.hear(&decoded, now, &mut sends, changes);
                member.route_property(store, &decoded, now, &mut sends, changes);
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
            member.expire_property(store, now, &mut sends, changes);
        }
        changes.refused += send(out, &sends);
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
            let Some(member) = self
                .members
                .iter_mut()
                .find(|m| m.group == switch.at.peer().group)
            else {
                changes.answers.push((
                    switch.at.peer(),
                    Answer::SwitchLost {
                        request: switch.request(),
                    },
                ));
                continue;
            };
            member.start_switch(&switch, now, &mut sends, changes);
        }
        changes.refused += send(out, &sends);
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
            let Some(member) = self
                .members
                .iter_mut()
                .find(|m| m.group == ask.peer.at.peer().group)
            else {
                changes
                    .answers
                    .push((ask.peer.at.peer(), Answer::ReportLost));
                continue;
            };
            member.start_report(&ask, now, &mut sends, changes);
        }
        changes.refused += send(out, &sends);
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
                    self.initiate_property(muid, now, sends, changes);
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
                    self.forget_property(muid, changes);
                }
                CiAction::MuidChanged { new, .. } => self.renew_property(new),
                // The device's own MUID changing reaches the app as every peer
                // lost (and so every Initiator dropped), and the Responder is
                // made again for it; the window closing is the core's own
                // bookkeeping.
                _ => {}
            }
        }
    }

    fn peer_ref(&self, device: &DeviceId, muid: Muid) -> PeerRef {
        PeerRef::new(device.clone(), self.peer_id(muid))
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

    /// Hands every Profile Configuration and Process Inquiry Initiator the
    /// message, which is theirs to keep or ignore.
    fn hear(
        &mut self,
        decoded: &Decoded,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        for (muid, actions) in hear_all(&mut self.initiators, decoded, now) {
            self.carry_out(muid, actions, sends, changes);
        }
        for (muid, actions) in hear_all(&mut self.process, decoded, now) {
            self.carry_out_process(muid, actions, sends, changes);
        }
    }

    /// Ends every wait of the Profile Configuration and Process Inquiry
    /// Initiators that has run out.
    fn expire(
        &mut self,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        for (muid, actions) in expire_all(&mut self.initiators, now) {
            self.carry_out(muid, actions, sends, changes);
        }
        for (muid, actions) in expire_all(&mut self.process, now) {
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
        let Some(initiator) = self.process.get_mut(&ask.peer.muid()) else {
            changes
                .answers
                .push((self.peer_id(ask.peer.muid()), Answer::ReportLost));
            return;
        };
        match initiator.report(ask.request, now) {
            Ok(actions) => self.carry_out_process(ask.peer.muid(), actions, sends, changes),
            // The session sends a report only once the one in flight has
            // ended; a second here has no answer of its own.
            Err(ProcessError::AlreadyAsking) => {}
            Err(ProcessError::NotSupported) => changes
                .answers
                .push((self.peer_id(ask.peer.muid()), Answer::ReportUnsupported)),
            Err(_) => {}
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
                ProcessAction::ReportEnded => {
                    changes.answers.push((id, Answer::Report(ReportEnd::Ended)));
                    continue;
                }
                ProcessAction::ReportRefused(nak) => {
                    changes
                        .answers
                        .push((id, Answer::Report(ReportEnd::Refused(nak))));
                    continue;
                }
                ProcessAction::ReportTimedOut => {
                    changes
                        .answers
                        .push((id, Answer::Report(ReportEnd::TimedOut)));
                    continue;
                }
                _ => continue,
            };
            changes.process.push((id, change));
        }
    }

    fn start_switch(
        &mut self,
        switch: &ProfileSwitch,
        now: Instant,
        sends: &mut Vec<(MidiGroup, CiMessage)>,
        changes: &mut CiChanges,
    ) {
        let Some(initiator) = self.initiators.get_mut(&switch.at.peer().muid) else {
            changes.answers.push((
                self.peer_id(switch.at.peer().muid),
                Answer::SwitchLost {
                    request: switch.request(),
                },
            ));
            return;
        };
        let started = match switch.switch {
            Switch::On(target) => initiator.set_on(target, switch.profile, now),
            Switch::Off(address) => initiator.set_off(address, switch.profile, now),
        };
        match started {
            Ok(actions) => self.carry_out(switch.at.peer().muid, actions, sends, changes),
            // The session sends a switch of a Profile at an address only once
            // the one in flight has ended; a second here has no answer of its
            // own.
            Err(ProfileRequestError::AlreadyAsking) => {}
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
                    changes
                        .answers
                        .push((id, Answer::Switch { request, outcome }));
                    continue;
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
/// lost one is a peer found later or never. A device takes a batch as a whole
/// message or not at all (`MidiOut::queue_timed`), so a refused message leaves
/// no SysEx7 without its end on the wire; it is logged and counted as refused.
fn send(out: &mut OpenOutput, sends: &[(MidiGroup, CiMessage)]) -> Losses {
    let mut refused = 0;
    for (group, message) in sends {
        let Ok(unsent) = message.packets(*group) else {
            refused += 1;
            tracing::debug!("a MIDI-CI message encoded a byte that is not 7-bit");
            continue;
        };
        let packets: Vec<_> = unsent.map(|ump| (Offset::ZERO, ump)).collect();
        if !out.queue(&packets).is_all(&packets) {
            refused += 1;
            tracing::debug!("a MIDI-CI message was refused by {}", out.name());
        }
    }
    Losses::refused(refused)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tutti_midi_ci::{CiCategories, DiscoveryData};
    use tutti_midi_device::{EndpointId, PortDirection};
    use tutti_ump::{ManufacturerId, SoftwareRevision, U7};

    use super::*;
    use crate::MidiSession;
    use crate::test_support::{FakeBackend, FakeCounters, endpoint};

    fn identity() -> DeviceIdentity {
        DeviceIdentity {
            manufacturer: ManufacturerId::new(0x41).unwrap(),
            family: [U7::of(1), U7::of(2)],
            model: [U7::of(3), U7::of(4)],
            revision: SoftwareRevision::from([U7::of(5), U7::of(6), U7::of(7), U7::of(8)]),
        }
    }

    /// The output of a fake device, and the counters of what it was sent.
    fn output() -> (OpenOutput, FakeCounters) {
        output_taking(usize::MAX)
    }

    /// As [`output`], the device taking `room` packets in all.
    fn output_taking(room: usize) -> (OpenOutput, FakeCounters) {
        let (backend, counters) = FakeBackend::build_with(vec![], vec![], room);
        let inlets = tutti_midi_runtime::InputInlets::new(
            tutti_graph::EventCapacity::of(64),
            Duration::ZERO,
        );
        let session = MidiSession::open_over(
            &tutti_platform::Executor::new(),
            backend,
            inlets,
            crate::MidiHardwareConfig::new(),
        )
        .unwrap();
        counters.plug(endpoint(7, "Wire"), PortDirection::Duplex);
        let id = session.outputs().first().expect("the device").id.clone();
        (
            session
                .connect_output(crate::testing::started(), &id)
                .unwrap(),
            counters,
        )
    }

    // A MIDI-CI message is SysEx7 packets, and a device with room for some of
    // them takes none: a SysEx7 whose end the device never gets is no message.
    #[test]
    fn a_ci_message_the_device_has_no_room_for_whole_is_not_sent_in_part() {
        let message = CiMessage::Discovery {
            header: CiHeader::new(Muid::of(0x0012_3456)),
            data: DiscoveryData::new(identity(), CiCategories::empty(), MaxSysexSize::of(256)),
        };
        let packets = message.packets(G0).unwrap().count();
        assert!(packets >= 2, "a message of several packets");
        let (mut out, counters) = output_taking(packets - 1);
        assert_eq!(send(&mut out, &[(G0, message.clone())]), Losses::refused(1));
        assert!(counters.take_sent(EndpointId::from_raw(7)).is_empty());

        let (mut out, counters) = output_taking(packets);
        assert_eq!(send(&mut out, &[(G0, message)]), Losses::NONE);
        assert_eq!(counters.take_sent(EndpointId::from_raw(7)).len(), packets);
    }

    /// An entropy source that refuses draws `from..to` (counted from 1) and
    /// gives every other, with the count it keeps.
    fn refusing(from: usize, to: usize) -> impl FnMut() -> Result<Entropy, getrandom::Error> {
        let mut draws = 0;
        move || {
            draws += 1;
            if (from..to).contains(&draws) {
                Err(getrandom::Error::UNSUPPORTED)
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
        assert_eq!(first.unstarted, [(G1, getrandom::Error::UNSUPPORTED)]);
        let _ = counters.take_sent(raw);

        // A peer's Discovery arrives on Group 0 in two polls; Group 1 is refused
        // again in both and is not reported again.
        let message = CiMessage::Discovery {
            header: CiHeader::new(Muid::of(0x0012_3456)),
            data: DiscoveryData::new(identity(), CiCategories::empty(), MaxSysexSize::of(256)),
        };
        let packets: Vec<Ump> = message.packets(G0).unwrap().collect();
        assert!(packets.len() >= 2);
        let (head, tail) = packets.split_at(packets.len() / 2);
        for (ms, part) in [(1, head), (2, tail)] {
            let mut changes = CiChanges::default();
            running.reconcile(&[G0, G1], &mut source, at(ms), &mut out, &mut changes);
            running.step(
                &mut ResourceStore::new(),
                at(ms),
                &heard(part),
                &mut out,
                &mut changes,
            );
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

    // Every Group refused: each is reported once over several polls, the set
    // stays not idle (what keeps `Ci::poll` from dropping it), and the retry goes
    // on: the first draw that succeeds starts both Groups.
    #[test]
    fn when_every_group_is_refused_each_is_reported_once_and_the_retry_goes_on() {
        let (mut out, _counters) = output();
        // Two draws at the start and two on each of three polls are refused.
        let mut source = refusing(1, 9);
        let now = Instant::now();
        let (mut running, first) =
            Running::start(&[G0, G1], identity(), &mut source, now, &mut out);
        assert_eq!(
            first.unstarted,
            [
                (G0, getrandom::Error::UNSUPPORTED),
                (G1, getrandom::Error::UNSUPPORTED)
            ]
        );
        assert!(!running.is_idle(), "the Groups still wait to start");
        for _ in 0..3 {
            let mut changes = CiChanges::default();
            running.reconcile(&[G0, G1], &mut source, now, &mut out, &mut changes);
            assert!(changes.unstarted.is_empty(), "reported once");
            assert!(!running.is_idle());
            assert!(running.members.is_empty());
        }
        // The next draws succeed: both Groups start, and nothing is reported.
        let mut changes = CiChanges::default();
        running.reconcile(&[G0, G1], &mut source, now, &mut out, &mut changes);
        assert!(changes.unstarted.is_empty());
        assert_eq!(running.members.len(), 2);
        assert!(running.unstarted.is_empty());
        // No Group is called for: nothing waits and nothing runs.
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
        assert_eq!(changes.unstarted, [(G1, getrandom::Error::UNSUPPORTED)]);
        let mut changes = CiChanges::default();
        running.reconcile(&[G0, G1], &mut source, now, &mut out, &mut changes);
        assert_eq!(changes.unstarted, [(G0, getrandom::Error::UNSUPPORTED)]);
    }

    // A CI message the output cannot take is counted, not dropped without a
    // trace: the first Discovery of each of two Groups goes to an output that
    // takes nothing, so two messages are refused; an output that takes them
    // counts none.
    #[test]
    fn a_ci_message_the_output_refuses_is_counted_and_one_it_takes_is_not() {
        let now = Instant::now();
        let (mut full, _counters) = output_taking(0);
        let (_, changes) = Running::start(&[G0, G1], identity(), &mut entropy, now, &mut full);
        assert_eq!(changes.refused, Losses::refused(2));

        let (mut roomy, counters) = output();
        let (_, changes) = Running::start(&[G0, G1], identity(), &mut entropy, now, &mut roomy);
        assert_eq!(changes.refused, Losses::NONE);
        assert!(counters.sent() > 0, "the messages were sent");
    }

    // A message the output has room for only in part is not sent in part: it
    // is refused whole, and counted.
    #[test]
    fn a_ci_message_the_output_has_room_for_only_in_part_is_counted() {
        let now = Instant::now();
        let (mut short, _counters) = output_taking(1);
        let (_, changes) = Running::start(&[G0], identity(), &mut entropy, now, &mut short);
        assert_eq!(changes.refused, Losses::refused(1));
    }

    /// The initiator `init` and the Responder `server` trade messages until
    /// neither has more to say; the initiator's events are returned.
    fn trade(
        init: &mut tutti_midi_ci::ResourceInitiator,
        server: &mut ResourceResponder,
        store: &mut ResourceStore,
        first: Vec<tutti_midi_ci::ResourceAction>,
        now: Instant,
    ) -> Vec<tutti_midi_ci::ResourceEvent> {
        use tutti_midi_ci::{ResourceAction, ResponderAction};
        let mut events = Vec::new();
        let mut actions = first;
        for _ in 0..40 {
            let mut to_server = Vec::new();
            for action in actions.drain(..) {
                match action {
                    ResourceAction::Send(message) => to_server.push(message),
                    ResourceAction::Event(event) => events.push(*event),
                    _ => {}
                }
            }
            if to_server.is_empty() {
                break;
            }
            for message in to_server {
                let replies = server.on_message(store, &Decoded::Message(message), now);
                for reply in replies {
                    if let ResponderAction::Send(message) = reply {
                        actions.extend(init.on_message(&Decoded::Message(message), now));
                    }
                }
            }
        }
        events
    }

    // Every Group of an input serves the one store the app gave: a Set through
    // one Group is read through another, and by the app.
    #[test]
    fn a_set_through_one_group_is_read_through_another_and_by_the_app() {
        use tutti_midi_ci::{
            CanSet, PeCapabilities, PeData, PeLocalOn, PeReassemblyLimits, PeResource, PeSettings,
            ResourceEntry, ResourceEvent, ResourceGet, ResourceInitiator, ResourceSet,
            ResourceTarget, SimultaneousRequests,
        };
        let (mut out, _counters) = output();
        let now = Instant::now();
        let mut store = ResourceStore::new();
        store.declare(
            ResourceEntry::builder(PeResource::LocalOn)
                .can_set(CanSet::Full)
                .build()
                .unwrap(),
        );
        store
            .put(None, PeData::LocalOn(PeLocalOn(true)))
            .expect("declared");
        let (mut running, _) = Running::start(&[G0, G1], identity(), &mut entropy, now, &mut out);

        let peer = Muid::of(0x0012_3456);
        let settings = |max| PeSettings {
            capabilities: PeCapabilities::new(SimultaneousRequests::of(2)),
            peer_max_sysex: max,
            reassembly: PeReassemblyLimits {
                streams: tutti_midi_ci::StreamLimit::of(4),
                bytes: tutti_midi_ci::BufferLimit::of(1 << 16),
            },
            body_limit: tutti_midi_ci::BodyLimit::of(1 << 16),
        };
        let target = ResourceTarget::try_from(PeResource::LocalOn).unwrap();
        let mut ends = Vec::new();
        for member in &mut running.members {
            member.server.accept(peer, MAX_SYSEX);
            let (init, first) =
                ResourceInitiator::new(peer, member.device.muid(), settings(MAX_SYSEX), now);
            ends.push((init, first));
        }
        let [(a, first_a), (b, first_b)] = &mut ends[..] else {
            panic!("two Groups");
        };
        let [ma, mb] = &mut running.members[..] else {
            panic!("two Groups");
        };
        let _ = trade(a, &mut ma.server, &mut store, std::mem::take(first_a), now);
        let _ = trade(b, &mut mb.server, &mut store, std::mem::take(first_b), now);

        let set = ResourceSet::full(target.clone(), PeData::LocalOn(PeLocalOn(false))).unwrap();
        let (_, actions) = a.set(&set, now).unwrap();
        let _ = trade(a, &mut ma.server, &mut store, actions, now);

        let (_, actions) = b.get(&ResourceGet::new(target.clone()), now).unwrap();
        let events = trade(b, &mut mb.server, &mut store, actions, now);
        let got = events.iter().find_map(|e| match e {
            ResourceEvent::Got { result, .. } => Some(result.clone()),
            _ => None,
        });
        assert_eq!(got, Some(Ok(PeData::LocalOn(PeLocalOn(false)))));
        assert_eq!(
            store.get(&target),
            Some(&PeData::LocalOn(PeLocalOn(false))),
            "the app's store sees it"
        );
    }

    /// The id of a fake device.
    fn device_id() -> DeviceId {
        let (backend, counters) = FakeBackend::build_with(vec![], vec![], usize::MAX);
        let inlets = tutti_midi_runtime::InputInlets::new(
            tutti_graph::EventCapacity::of(64),
            Duration::ZERO,
        );
        let session = MidiSession::open_over(
            &tutti_platform::Executor::new(),
            backend,
            inlets,
            crate::MidiHardwareConfig::new(),
        )
        .unwrap();
        counters.plug(endpoint(7, "Wire"), PortDirection::Duplex);
        session.outputs().first().expect("the device").id.clone()
    }

    // A request whose peer is gone when the poll would send it is reported with
    // the reason, never dropped: for a Group with no device and for a peer the
    // device no longer has, and for each of Profile, Process and Property Exchange.
    #[test]
    fn a_request_for_a_peer_that_is_gone_is_reported_not_dropped() {
        use crate::properties::PropertyCall;
        use tutti_midi_ci::{
            CiDeviceId, DataControl, ProfileId, ReportRequest, ReportScope, ResourceGet,
            ResourceTarget,
        };
        let device = device_id();
        let (mut out, _counters) = output();
        let now = Instant::now();
        let (mut running, _) = Running::start(&[G0], identity(), &mut entropy, now, &mut out);
        let muid = Muid::of(0x0012_3456);
        let profile = ProfileId::from_bytes([0x7E, 1, 0, 1, 1]).unwrap();
        for group in [G1, G0] {
            let id = PeerId { group, muid };
            let mut changes = CiChanges::default();
            running.start_switches(
                &mut vec![ProfileSwitch {
                    at: PeerRef::new(device.clone(), id),
                    profile,
                    switch: Switch::Off(CiDeviceId::Group),
                }],
                now,
                &mut out,
                &mut changes,
            );
            assert_eq!(
                changes.answers,
                [(
                    id,
                    Answer::SwitchLost {
                        request: tutti_midi_ci::ProfileRequest::SetOff {
                            address: CiDeviceId::Group,
                            profile
                        }
                    }
                )],
                "{group:?}"
            );
            let peer = Reporter {
                at: PeerRef::new(device.clone(), id),
            };
            let request = ReportRequest::new(
                DataControl::Full,
                ReportScope::Group {
                    controllers: tutti_midi_ci::ChannelControllers::all(),
                    notes: tutti_midi_ci::NoteData::all(),
                },
            );
            running.start_reports(
                &mut vec![ReportAsk { peer, request }],
                now,
                &mut out,
                &mut changes,
            );
            assert_eq!(
                changes.answers[1..2],
                [(id, Answer::ReportLost)],
                "{group:?}"
            );
            let call = PropertyCall::Get(ResourceGet::new(
                ResourceTarget::try_from(tutti_midi_ci::PeResource::LocalOn).unwrap(),
            ));
            running.start_properties(
                &mut vec![PropertyAsk {
                    peer: PropertyPeer {
                        at: PeerRef::new(device.clone(), id),
                        resources: Arc::default(),
                    },
                    call: call.clone(),
                }],
                now,
                &mut out,
                &mut changes,
            );
            assert_eq!(
                changes.answers[2..],
                [(
                    id,
                    Answer::PropertyLost {
                        call: Box::new(call)
                    }
                )],
                "{group:?}"
            );
        }
    }
}
