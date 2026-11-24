//! [`ProfileInitiator`]: the Initiator's side of Profile Configuration with one
//! discovered peer (M2-101 §7, M2-102 §2).
//!
//! A pure state machine in the shape of [`CiDevice`](super::CiDevice): every step
//! returns the [`ProfileAction`]s to carry out, time is an [`Instant`] argument,
//! and nothing here reads a clock or sends a byte. The caller makes one per peer
//! the Management core found ([`CiAction::Found`](super::CiAction::Found)), routes
//! to it every decoded message of that Group, sends each
//! [`ProfileAction::Send`] on that Group, and drops it when the peer is
//! [`Lost`](super::CiAction::Lost) or the device's MUID changes. CI messages carry
//! no Group, so the Group is the caller's.
//!
//! # The rules
//!
//! - The inquiry (M2-102 §2.4, the `0x21` Reply to Profile Inquiry): [`new`](ProfileInitiator::new)
//!   sends a Profile Inquiry to the Function Block (`0x7F`), which asks for every Profile
//!   whatever the MIDI Channel structure. The peer answers with one Reply per address that
//!   has Profiles, in the order Channels, then Groups (`0x7E`), and then a final Reply on
//!   `0x7F`, reporting no Profiles to show that no further replies will follow,
//!   when it has none. That last Reply is the rule for the set being complete
//!   ([`ProfileAction::ProfilesKnown`]); an address that replied is replaced by its Reply, and
//!   an address that did not is dropped from the table then. Neither spec gives a time limit,
//!   so a peer that never sends the final Reply is given up on after [`PEER_REPLY_TIMEOUT`]
//!   ([`ProfileAction::InquiryTimedOut`]): the table keeps what it heard, and nothing is
//!   pruned, since the set is not known to be complete. A NAK of the Inquiry
//!   ([`ProfileAction::InquiryRefused`]) ends it too: the peer has no Profile Configuration.
//! - The table ([`ProfileTable`]): per address (a Channel, the Group, the Function Block),
//!   the Profiles the peer supports, each enabled or disabled, and the number of Channels it
//!   reported while enabled. The reports update it at any time after, from the peer only:
//!   Added (§7.4, disabled), Removed (§7.5), Enabled (§7.10) and Disabled (§7.11), each a
//!   [`ProfileAction::Changed`] when it changed something. An Enabled or Disabled Report for a
//!   Profile the table lacks adds it: the peer supports it, since it says so.
//! - Set On and Set Off ([`set_on`](ProfileInitiator::set_on),
//!   [`set_off`](ProfileInitiator::set_off)): the Enabled Report acknowledges a Set
//!   Profile On (§7.10), so the request resolves on the Enabled or Disabled Report of
//!   that Profile at that address, whichever state it reports (M2-102 §2.6 and §2.7: a peer that
//!   cannot comply reports the state it kept), on a NAK of that message for that Profile
//!   (Status `0x04`, Profile not supported on the requested Channel, Group, or Function Block,
//!   or any other; the NAK's five detail bytes are the Profile Id, §5.11.3), or on
//!   [`PEER_REPLY_TIMEOUT`]. A Report of another Profile (M2-102 §2.8: a Profile Disabled
//!   Report for the Profile it replaced) is a change and resolves nothing.
//! - Details ([`details`](ProfileInitiator::details)): the Reply that echoes the Profile
//!   and Inquiry Target at that address resolves it; so does a NAK or the timeout.
//! - Profile Specific Data from the peer, to this device or to the Broadcast MUID, is
//!   handed out as it is ([`ProfileAction::SpecificData`]).
//!
//! - Timeout Wait (M2-101 §5.10, Table 14): an ACK with Status Code `0x10` from the peer, to
//!   this device, says to wait for Status Data times 100 ms (0 to 12.7 s). It names its transaction
//!   by the Original Transaction Sub-ID#2 and the address it is on (§5.10.1, along with other
//!   fields), and for a Set or a Details request by the Profile Id in its five detail bytes
//!   (§5.10.3; zeroes from a peer with none to give, as for a NAK). A Profile Inquiry names no
//!   Profile, so its details are not read; it is matched on the Function Block address it was sent
//!   to. The wait is added to the transaction's current deadline (M2-103 §12.2.1): a wait of 0
//!   leaves the deadline where it is, and each new wait adds again. An ACK for nothing in flight, and an ACK with any other status, resolve and extend
//!   nothing: a success ACK (`0x00`) does not end a Set, because the answer to a Set is the
//!   Enabled or Disabled Report (the Enabled Report is the acknowledgement of a Set Profile On,
//!   §7.10; M2-102 §2.6, §2.7), and §5.10 reserves the ACK for new mechanisms that other
//!   specifications describe, none of which M2-102 does for Profiles.
//!
//! A message from another source, or a Reply, Details Reply, ACK or NAK not addressed to this device,
//! is not this peer's and changes nothing.
//!
//! # The timeout
//!
//! An inquiry waits [`PEER_REPLY_TIMEOUT`] for its reply.

use std::collections::{BTreeMap, BTreeSet};
use std::vec::Vec;
use tutti_platform::Instant;

use super::Initiator;
use super::TimeoutWait;
use super::handshake::Handshake;
use super::link::Link;
use crate::deadline::Deadline;

use tutti_ump::{MpeChannels, MpeOverlap, MpeZoneSet, MpeZoneSpan, U7};

use super::{
    Ack, CiDeviceId, CiHeader, CiMessage, Decoded, Destination, DetailsData, InquiryTarget, Muid,
    Nak, PEER_REPLY_TIMEOUT, ProfileChannels, ProfileFormat, ProfileId, ProfileLists, ProfileState,
    ProfileTarget, SUB_ID2_PROFILE_DETAILS_INQUIRY, SUB_ID2_PROFILE_INQUIRY,
    SUB_ID2_SET_PROFILE_OFF, SUB_ID2_SET_PROFILE_ON, SpecificData,
};

/// Whether the five detail bytes of an ACK or a NAK name `profile` (§5.10.3, §5.11.3), or
/// are zeroes, which a peer with nothing to name sends.
fn details_name(details: [u8; 5], profile: ProfileId) -> bool {
    details == profile.to_bytes() || details == [0; 5]
}

/// A request in flight: what [`ProfileAction::Resolved`] names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileRequest {
    /// A Set Profile On.
    SetOn {
        /// Where.
        address: CiDeviceId,
        /// What.
        profile: ProfileId,
    },
    /// A Set Profile Off.
    SetOff {
        /// Where.
        address: CiDeviceId,
        /// What.
        profile: ProfileId,
    },
    /// A Profile Details Inquiry.
    Details {
        /// Where.
        address: CiDeviceId,
        /// What.
        profile: ProfileId,
        /// The target asked.
        target: InquiryTarget,
    },
}

impl ProfileRequest {
    fn address(&self) -> CiDeviceId {
        match self {
            Self::SetOn { address, .. }
            | Self::SetOff { address, .. }
            | Self::Details { address, .. } => *address,
        }
    }

    fn profile(&self) -> ProfileId {
        match self {
            Self::SetOn { profile, .. }
            | Self::SetOff { profile, .. }
            | Self::Details { profile, .. } => *profile,
        }
    }

    fn sub_id2(&self) -> u8 {
        match self {
            Self::SetOn { .. } => SUB_ID2_SET_PROFILE_ON,
            Self::SetOff { .. } => SUB_ID2_SET_PROFILE_OFF,
            Self::Details { .. } => SUB_ID2_PROFILE_DETAILS_INQUIRY,
        }
    }

    fn is_set(&self) -> bool {
        !matches!(self, Self::Details { .. })
    }
}

/// How a [`ProfileRequest`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProfileOutcome {
    /// A Set Profile On or Off was answered by a Report: the state the peer says
    /// the Profile is in now (it is the state asked for unless the peer could not
    /// comply) and its Channel count: 0 for a Group or Function Block address,
    /// none from a Version 1 peer, whose Report has no such field.
    Reported {
        /// Enabled or disabled.
        state: ProfileState,
        /// The Number Channels field of the Report.
        channels: Option<ProfileChannels>,
    },
    /// A Details Inquiry was answered with this data.
    Details(DetailsData),
    /// The peer sent a NAK: [`Nak::status`] says why ([`NakStatus::ProfileNotSupported`](super::NakStatus::ProfileNotSupported)
    /// for a Profile it does not have at that address).
    Refused(Nak),
    /// [`PEER_REPLY_TIMEOUT`] passed with no answer.
    TimedOut,
}

/// What a Report changed ([`ProfileAction::Changed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProfileChange {
    /// Profile Added Report: newly supported, disabled.
    Added,
    /// Profile Removed Report: no longer supported.
    Removed,
    /// Profile Enabled Report.
    Enabled,
    /// Profile Disabled Report.
    Disabled,
}

/// What the driver does next.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProfileAction {
    /// Send this message to the peer, on the Group of the connection.
    Send(CiMessage),
    /// The final Reply (address `0x7F`) arrived: the table holds every Profile the
    /// peer declared.
    ProfilesKnown,
    /// [`PEER_REPLY_TIMEOUT`] passed before the final Reply: the table holds what was
    /// heard.
    InquiryTimedOut,
    /// The peer NAKed the Profile Inquiry.
    InquiryRefused(Nak),
    /// A Report changed the table.
    Changed {
        /// Where.
        address: CiDeviceId,
        /// Which Profile.
        profile: ProfileId,
        /// What happened to it.
        change: ProfileChange,
    },
    /// A request ended.
    Resolved {
        /// Which.
        request: ProfileRequest,
        /// How.
        outcome: ProfileOutcome,
    },
    /// Profile Specific Data arrived (§7.12), uninterpreted.
    SpecificData {
        /// The Device ID it came on.
        address: CiDeviceId,
        /// The Profile it belongs to.
        profile: ProfileId,
        /// The data.
        data: SpecificData,
    },
}

/// A request that cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ProfileRequestError {
    /// The same request, or a Set On or Off of the same Profile at the same
    /// address, is still waiting for its answer.
    #[error("a request for that Profile at that address is already in flight")]
    AlreadyAsking,
}

/// One Profile of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileEntry {
    state: ProfileState,
    channels: Option<ProfileChannels>,
}

impl ProfileEntry {
    /// Enabled or disabled.
    #[must_use]
    pub const fn state(&self) -> ProfileState {
        self.state
    }

    /// The Number Channels of the last Enabled or Disabled Report of a Version 2
    /// peer, `None` while none arrived (a Reply to Profile Inquiry carries no count).
    #[must_use]
    pub const fn channels(&self) -> Option<ProfileChannels> {
        self.channels
    }
}

/// The MPE Profile zones a [`ProfileTable`] has enabled, and the ones it left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnabledMpeZones {
    /// The zones held together, or `None` with no zone.
    pub zones: Option<MpeZoneSet>,
    /// Each zone left out, as the overlap with the zone held that refused it.
    pub refused: Vec<MpeOverlap>,
}

/// The Profiles a peer supports, per address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileTable {
    addresses: BTreeMap<CiDeviceId, BTreeMap<ProfileId, ProfileEntry>>,
}

impl ProfileTable {
    /// The addresses with at least one Profile, Channels first, then the Group,
    /// then the Function Block.
    pub fn addresses(&self) -> impl Iterator<Item = CiDeviceId> + '_ {
        self.addresses.keys().copied()
    }

    /// The Profiles at one address, by Profile Id; none for an address with none.
    pub fn profiles(
        &self,
        address: CiDeviceId,
    ) -> impl Iterator<Item = (ProfileId, &ProfileEntry)> + '_ {
        self.addresses
            .get(&address)
            .into_iter()
            .flat_map(|profiles| profiles.iter().map(|(id, entry)| (*id, entry)))
    }

    /// One Profile at one address.
    #[must_use]
    pub fn get(&self, address: CiDeviceId, profile: ProfileId) -> Option<&ProfileEntry> {
        self.addresses.get(&address)?.get(&profile)
    }

    /// Whether the table has no Profile.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }

    /// The zones of the MPE Profile (M2-120) enabled, lowest Manager first: one
    /// per Channel the Profile is enabled on, with the Number of Channels its
    /// Enabled Report gave. A Profile enabled without a count (a Version 1 peer
    /// sends none), or with one that makes no zone (fewer than two Channels, or
    /// past Channel 16), is not a zone, and a zone sharing a Channel with a lower
    /// one is left out (section 3.4) and listed in
    /// [`refused`](EnabledMpeZones::refused).
    ///
    /// This is the lenient reading, for a caller that follows the zones a peer
    /// enabled and skips the rest (a live input). A caller that must refuse a
    /// peer whose Profiles make no zone (choosing what to send to a Function Block)
    /// wants the strict one.
    #[must_use]
    pub fn mpe_zones(&self) -> EnabledMpeZones {
        let mut found = EnabledMpeZones::default();
        for address in self.addresses() {
            let CiDeviceId::Channel(manager) = address else {
                continue;
            };
            let Some(entry) = self.get(address, ProfileId::MPE) else {
                continue;
            };
            let zone = (entry.state() == ProfileState::Enabled)
                .then(|| entry.channels())
                .flatten()
                .and_then(|count| u8::try_from(count.get()).ok())
                .and_then(MpeChannels::new)
                .and_then(|count| MpeZoneSpan::new(manager, count));
            let Some(zone) = zone else { continue };
            found.zones = match found.zones {
                None => Some(MpeZoneSet::single(zone)),
                Some(held) => match held.with(zone) {
                    Ok(joined) => Some(joined),
                    Err(overlap) => {
                        found.refused.push(overlap);
                        Some(held)
                    }
                },
            };
        }
        found
    }

    /// Sets one entry; whether it differs from what was there.
    fn set(&mut self, address: CiDeviceId, profile: ProfileId, entry: ProfileEntry) -> bool {
        self.addresses
            .entry(address)
            .or_default()
            .insert(profile, entry)
            != Some(entry)
    }

    fn remove(&mut self, address: CiDeviceId, profile: ProfileId) -> bool {
        let Some(profiles) = self.addresses.get_mut(&address) else {
            return false;
        };
        let removed = profiles.remove(&profile).is_some();
        if profiles.is_empty() {
            self.addresses.remove(&address);
        }
        removed
    }

    /// An address as a Reply declares it: the lists replace what was there, and a
    /// Profile that stays enabled keeps its Channel count.
    fn replace(&mut self, address: CiDeviceId, lists: &ProfileLists) {
        let before = self.addresses.remove(&address).unwrap_or_default();
        let mut after = BTreeMap::new();
        for (list, state) in [
            (lists.disabled(), ProfileState::Disabled),
            (lists.enabled(), ProfileState::Enabled),
        ] {
            for profile in list {
                let kept = before
                    .get(profile)
                    .filter(|entry| entry.state == state)
                    .and_then(|entry| entry.channels);
                after.insert(
                    *profile,
                    ProfileEntry {
                        state,
                        channels: kept,
                    },
                );
            }
        }
        if !after.is_empty() {
            self.addresses.insert(address, after);
        }
    }
}

#[derive(Clone, Debug)]
struct InFlight {
    request: ProfileRequest,
    wait: Deadline,
}

/// The Initiator of Profile Configuration with one peer.
#[derive(Clone, Debug)]
pub struct ProfileInitiator {
    link: Link,
    table: ProfileTable,
    /// The Profile Inquiry: waiting with the addresses that replied, or answered
    /// by the final Reply.
    inquiry: Handshake<(), BTreeSet<CiDeviceId>>,
    pending: Vec<InFlight>,
}

impl ProfileInitiator {
    /// An Initiator for `peer`, from this device's MUID `own`, and the actions that
    /// start it at `now`: a Profile Inquiry to the Function Block.
    #[must_use]
    pub fn new(own: Muid, peer: Muid, now: Instant) -> (Self, Vec<ProfileAction>) {
        let mut initiator = Self {
            link: Link { own, peer },
            table: ProfileTable::default(),
            inquiry: Handshake::Unanswered,
            pending: Vec::new(),
        };
        let actions = initiator.inquire(now);
        (initiator, actions)
    }

    /// The peer's MUID.
    #[must_use]
    pub fn peer(&self) -> Muid {
        self.link.peer
    }

    /// The Profiles the peer is known to support.
    #[must_use]
    pub fn table(&self) -> &ProfileTable {
        &self.table
    }

    /// Whether an inquiry has ended with the final Reply, so the table is the peer's
    /// whole declaration (and the Reports since). It is not while an inquiry
    /// waits: its Replies replace the table address by address, so the table is
    /// then partly the new declaration and partly the old.
    #[must_use]
    pub fn is_known(&self) -> bool {
        matches!(self.inquiry, Handshake::Answered(()))
    }

    /// Asks again, to the Function Block: a new inquiry starts (one already waiting
    /// is replaced). The table is kept until the final Reply replaces it.
    pub fn inquire(&mut self, now: Instant) -> Vec<ProfileAction> {
        self.inquiry.ask(now, PEER_REPLY_TIMEOUT, BTreeSet::new());
        vec![self.send(CiDeviceId::FunctionBlock, |header, to| {
            CiMessage::ProfileInquiry { header, to }
        })]
    }

    /// Asks the peer to enable `profile` at `target`; the answer is a
    /// [`ProfileAction::Resolved`].
    ///
    /// # Errors
    ///
    /// [`ProfileRequestError::AlreadyAsking`] when a Set On or Off of that Profile at
    /// that address is waiting.
    pub fn set_on(
        &mut self,
        target: ProfileTarget,
        profile: ProfileId,
        now: Instant,
    ) -> Result<Vec<ProfileAction>, ProfileRequestError> {
        let address = target.address();
        self.begin(ProfileRequest::SetOn { address, profile }, now)?;
        Ok(vec![self.send(address, |header, to| {
            CiMessage::SetProfileOn {
                source: header.source,
                format: ProfileFormat::current(target),
                to,
                profile,
            }
        })])
    }

    /// Asks the peer to disable `profile` at `address`.
    ///
    /// # Errors
    ///
    /// [`ProfileRequestError::AlreadyAsking`] as for [`set_on`](Self::set_on).
    pub fn set_off(
        &mut self,
        address: CiDeviceId,
        profile: ProfileId,
        now: Instant,
    ) -> Result<Vec<ProfileAction>, ProfileRequestError> {
        self.begin(ProfileRequest::SetOff { address, profile }, now)?;
        Ok(vec![self.send(address, |header, to| {
            CiMessage::SetProfileOff {
                header,
                to,
                profile,
            }
        })])
    }

    /// Asks the peer for a detail of `profile` at `address`.
    ///
    /// # Errors
    ///
    /// [`ProfileRequestError::AlreadyAsking`] when the same inquiry is waiting.
    pub fn details(
        &mut self,
        address: CiDeviceId,
        profile: ProfileId,
        target: InquiryTarget,
        now: Instant,
    ) -> Result<Vec<ProfileAction>, ProfileRequestError> {
        self.begin(
            ProfileRequest::Details {
                address,
                profile,
                target,
            },
            now,
        )?;
        Ok(vec![self.send(address, |header, to| {
            CiMessage::ProfileDetailsInquiry {
                header,
                to,
                profile,
                target,
            }
        })])
    }

    fn on_reply(
        &mut self,
        address: CiDeviceId,
        lists: &ProfileLists,
        actions: &mut Vec<ProfileAction>,
    ) {
        // A Reply is only an answer to an Inquiry.
        let Some(replied) = self.inquiry.progress_mut() else {
            return;
        };
        replied.insert(address);
        self.table.replace(address, lists);
        if address == CiDeviceId::FunctionBlock {
            // The final reply, with no Profiles, shows that no further replies will follow (M2-102 §2.4).
            let replied = std::mem::take(replied);
            self.inquiry = Handshake::Answered(());
            self.table.addresses.retain(|at, _| replied.contains(at));
            actions.push(ProfileAction::ProfilesKnown);
        }
    }

    fn on_state(
        &mut self,
        profile: ProfileId,
        state: ProfileState,
        format: ProfileFormat,
        actions: &mut Vec<ProfileAction>,
    ) {
        let address = format.address();
        let entry = ProfileEntry {
            state,
            // A Version 1 sender has no count to report.
            channels: format.channels(),
        };
        if self.table.set(address, profile, entry) {
            actions.push(ProfileAction::Changed {
                address,
                profile,
                change: match state {
                    ProfileState::Enabled => ProfileChange::Enabled,
                    ProfileState::Disabled => ProfileChange::Disabled,
                },
            });
        }
        let at = self.pending.iter().position(|p| {
            p.request.is_set() && p.request.address() == address && p.request.profile() == profile
        });
        if let Some(at) = at {
            let pending = self.pending.remove(at);
            actions.push(ProfileAction::Resolved {
                request: pending.request,
                outcome: ProfileOutcome::Reported {
                    state,
                    channels: format.channels(),
                },
            });
        }
    }

    /// A Timeout Wait: the time asked is added to the deadline of the transaction it
    /// names.
    fn on_wait(&mut self, address: CiDeviceId, ack: &Ack, wait: TimeoutWait) {
        let original = ack.ack_sub_id2.get();
        if original == SUB_ID2_PROFILE_INQUIRY {
            if address == CiDeviceId::FunctionBlock
                && let Some(pending) = self.inquiry.wait_mut()
            {
                wait.extend(pending);
            }
            return;
        }
        let details = ack.details.map(U7::get);
        if let Some(pending) = self.pending.iter_mut().find(|p| {
            p.request.sub_id2() == original
                && p.request.address() == address
                && details_name(details, p.request.profile())
        }) {
            wait.extend(&mut pending.wait);
        }
    }

    fn on_nak(&mut self, address: CiDeviceId, nak: &Nak, actions: &mut Vec<ProfileAction>) {
        let original = nak.nak_sub_id2.get();
        if original == SUB_ID2_PROFILE_INQUIRY {
            if self.inquiry.refuse() {
                actions.push(ProfileAction::InquiryRefused(nak.clone()));
            }
            return;
        }
        // The details are the Profile Id (§5.11.3); a peer with none to give sends
        // zeroes.
        let details = nak.details.map(U7::get);
        let at = self.pending.iter().position(|p| {
            p.request.sub_id2() == original
                && p.request.address() == address
                && details_name(details, p.request.profile())
        });
        if let Some(at) = at {
            let pending = self.pending.remove(at);
            actions.push(ProfileAction::Resolved {
                request: pending.request,
                outcome: ProfileOutcome::Refused(nak.clone()),
            });
        }
    }

    fn begin(&mut self, request: ProfileRequest, now: Instant) -> Result<(), ProfileRequestError> {
        let clash = self.pending.iter().any(|pending| {
            pending.request == request
                || (request.is_set()
                    && pending.request.is_set()
                    && pending.request.address() == request.address()
                    && pending.request.profile() == request.profile())
        });
        if clash {
            return Err(ProfileRequestError::AlreadyAsking);
        }
        self.pending.push(InFlight {
            request,
            wait: Deadline::after(now, PEER_REPLY_TIMEOUT),
        });
        Ok(())
    }

    fn send(
        &self,
        address: CiDeviceId,
        message: impl FnOnce(CiHeader, Muid) -> CiMessage,
    ) -> ProfileAction {
        ProfileAction::Send(self.link.message(address, message))
    }
}

impl Initiator for ProfileInitiator {
    type Action = ProfileAction;

    fn deadline(&self) -> Option<Instant> {
        Deadline::earliest(
            self.inquiry
                .wait()
                .into_iter()
                .chain(self.pending.iter().map(|pending| pending.wait)),
        )
    }

    fn on_message(&mut self, decoded: &Decoded, _now: Instant) -> Vec<ProfileAction> {
        let mut actions = Vec::new();
        let Decoded::Message(message) = decoded else {
            return actions;
        };
        let from_peer = self.link.is_from_peer(message);
        let to_us = self.link.is_to_us(message);
        let address = message.header().device_id;
        match message {
            CiMessage::ProfileReply { profiles, .. } if from_peer && to_us => {
                self.on_reply(address, profiles, &mut actions);
            }
            CiMessage::ProfileEnabled {
                format, profile, ..
            } if from_peer => self.on_state(*profile, ProfileState::Enabled, *format, &mut actions),
            CiMessage::ProfileDisabled {
                format, profile, ..
            } if from_peer => {
                self.on_state(*profile, ProfileState::Disabled, *format, &mut actions)
            }
            CiMessage::ProfileAdded { profile, .. } if from_peer => {
                let entry = ProfileEntry {
                    state: ProfileState::Disabled,
                    channels: None,
                };
                // A Profile already listed keeps its state: the Report says only
                // that it is supported.
                if self.table.get(address, *profile).is_none() {
                    self.table.set(address, *profile, entry);
                    actions.push(ProfileAction::Changed {
                        address,
                        profile: *profile,
                        change: ProfileChange::Added,
                    });
                }
            }
            CiMessage::ProfileRemoved { profile, .. } if from_peer => {
                if self.table.remove(address, *profile) {
                    actions.push(ProfileAction::Changed {
                        address,
                        profile: *profile,
                        change: ProfileChange::Removed,
                    });
                }
            }
            CiMessage::ProfileDetailsReply {
                profile,
                target,
                data,
                ..
            } if from_peer && to_us => {
                let at = self.pending.iter().position(|pending| {
                    pending.request
                        == ProfileRequest::Details {
                            address,
                            profile: *profile,
                            target: *target,
                        }
                });
                if let Some(at) = at {
                    let pending = self.pending.remove(at);
                    actions.push(ProfileAction::Resolved {
                        request: pending.request,
                        outcome: ProfileOutcome::Details(data.clone()),
                    });
                }
            }
            CiMessage::ProfileSpecificData(specific)
                if from_peer && (specific.to() == Destination::Broadcast || to_us) =>
            {
                actions.push(ProfileAction::SpecificData {
                    address,
                    profile: specific.profile(),
                    data: specific.data().clone(),
                });
            }
            CiMessage::Ack { ack, .. } if from_peer && to_us => {
                if let Some(wait) = TimeoutWait::from_ack(ack) {
                    self.on_wait(address, ack, wait);
                }
            }
            CiMessage::Nak { nak, .. } if from_peer && to_us => {
                self.on_nak(address, nak, &mut actions);
            }
            _ => {}
        }
        actions
    }

    fn on_timeout(&mut self, now: Instant) -> Vec<ProfileAction> {
        let mut actions = Vec::new();
        if self.inquiry.expire(now) {
            actions.push(ProfileAction::InquiryTimedOut);
        }
        let (expired, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|pending| pending.wait.expired(now));
        self.pending = waiting;
        actions.extend(expired.into_iter().map(|pending| ProfileAction::Resolved {
            request: pending.request,
            outcome: ProfileOutcome::TimedOut,
        }));
        actions
    }
}

#[cfg(test)]
mod tests;
