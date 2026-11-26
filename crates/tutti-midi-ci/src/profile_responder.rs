//! [`ProfileOffer`], the Profiles a device offers, and the Responder that answers
//! Profile Configuration from it (M2-101 §7, M2-102 §2.4 to §2.7).
//!
//! The Responder is a pure state machine in the shape of
//! [`ResourceResponder`](super::ResourceResponder): every step returns the
//! [`CiAction`]s to carry out and nothing here reads a clock or sends a byte.
//! A [`CiDevice`](super::CiDevice) holds one and routes it the Profile messages
//! addressed to the device.
//!
//! # The rules
//!
//! - Profile Inquiry (§2.4): on a Channel or the Group, one Reply for that address; on the
//!   Function Block, a Reply for each Channel with Profiles, in Channel order, then the
//!   Group's if it has any, then the Function Block's, which comes last whether or not it lists
//!   any and so shows that no further Replies follow.
//! - Set Profile On and Off (§2.6, §2.7): the Profile at that address is switched and the
//!   Enabled or Disabled Report goes to the Broadcast MUID. A Profile that is not offered at the
//!   address is a NAK, Status `0x04`. A Profile that cannot be switched is answered with the
//!   Report of the state it keeps: an entry that is [`fixed`](ProfileOfferEntry::fixed), a Set On
//!   for more Channels than the entry's maximum (§2.5.1), and an MPE Profile whose zone
//!   shares a Channel with another enabled zone, or is no zone (M2-120 §3.4).
//!   A switch that changed the Profile is a [`CiAction::ProfileSwitched`].
//! - Profile Details Inquiry (§2.5.1): the Registered Target Data `Number of MIDI
//!   Channels` is the Channels in use and the entry's maximum; any other target is a NAK.
//! - A new offer ([`offer`](ProfileResponder::offer)) is diffed entry by entry, keyed by
//!   address and Profile. An entry whose fields changed takes the app's value and drops a peer's
//!   switch of it; an entry left equal keeps the switch. A new entry is Added (§7.4) and, if
//!   enabled, Enabled at once; one gone is Disabled if it was enabled, then Removed (§7.5).
//!   A change of an entry that changes its state or Channels is Reported as
//!   Enabled or Disabled.

use std::collections::BTreeMap;
use std::vec::Vec;

use tutti_ump::{MpeChannels, MpeZoneSet, MpeZoneSpan};

use super::{
    CI_VERSION, ChannelsDetail, CiAction, CiDeviceId, CiHeader, CiMessage, InquiryTarget, Muid,
    NakStatus, ProfileChannels, ProfileEntry, ProfileFormat, ProfileId, ProfileList, ProfileLists,
    ProfileState, ProfileTable, ProfileTarget, nak_message,
};

/// Whether a peer's Set Profile On or Off may switch an offered Profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PeerSwitching {
    /// A peer may switch it.
    #[default]
    Allowed,
    /// Only the app switches it: a peer's request is answered with the state kept.
    Fixed,
}

/// One Profile offered at one address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileOfferEntry {
    enabled: Option<ProfileChannels>,
    max: ProfileChannels,
    switching: PeerSwitching,
}

impl ProfileOfferEntry {
    /// A disabled Profile that can use at most `max` Channels (M2-102 §2.5.1): 1
    /// for a Profile on one Channel, [`ProfileChannels::of`]`(0)` on a Group or
    /// Function Block, whose width is the address's.
    #[must_use]
    pub const fn new(max: ProfileChannels) -> Self {
        Self {
            enabled: None,
            max,
            switching: PeerSwitching::Allowed,
        }
    }

    /// The Profile enabled, using `channels` Channels.
    #[must_use]
    pub const fn enabled(self, channels: ProfileChannels) -> Self {
        Self {
            enabled: Some(channels),
            ..self
        }
    }

    /// Peers may not switch the Profile.
    #[must_use]
    pub const fn fixed(self) -> Self {
        Self {
            switching: PeerSwitching::Fixed,
            ..self
        }
    }

    /// The table entry of this entry at `address`: a Group or Function Block has no count.
    fn applied(&self, address: CiDeviceId) -> ProfileEntry {
        let count = self.enabled.unwrap_or(self.max);
        ProfileEntry::new(
            self.enabled
                .map_or(ProfileState::Disabled, |_| ProfileState::Enabled),
            Some(counted(address, count)),
            Some(counted(address, self.max)),
        )
    }
}

/// `count` as the Number of Channels of `address`: 0 for a Group or Function Block.
const fn counted(address: CiDeviceId, count: ProfileChannels) -> ProfileChannels {
    match address {
        CiDeviceId::Channel(_) => count,
        CiDeviceId::Group | CiDeviceId::FunctionBlock => ProfileChannels::BY_ADDRESS,
    }
}

/// The Profiles a device offers, each at a Channel, the Group or the Function
/// Block ([`CiDeviceId`]), by address and Profile Id. Plain data compared by
/// content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileOffer {
    entries: BTreeMap<(CiDeviceId, ProfileId), ProfileOfferEntry>,
}

impl ProfileOffer {
    /// The offer with `entry` for `profile` at `address`, replacing one there.
    #[must_use]
    pub fn with(
        mut self,
        address: CiDeviceId,
        profile: ProfileId,
        entry: ProfileOfferEntry,
    ) -> Self {
        self.entries.insert((address, profile), entry);
        self
    }

    fn get(&self, address: CiDeviceId, profile: ProfileId) -> Option<&ProfileOfferEntry> {
        self.entries.get(&(address, profile))
    }

    /// Whether nothing is offered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The Responder of Profile Configuration: what is offered, and the state it is
/// in with the peers' switches.
#[derive(Clone, Debug, Default)]
pub(crate) struct ProfileResponder {
    offer: ProfileOffer,
    table: ProfileTable,
}

fn header(own: Muid, address: CiDeviceId) -> CiHeader {
    CiHeader {
        device_id: address,
        ci_version: CI_VERSION,
        source: own,
    }
}

/// The Enabled or Disabled Report of `entry`.
fn report(own: Muid, address: CiDeviceId, profile: ProfileId, entry: &ProfileEntry) -> CiAction {
    let target = ProfileTarget::at(
        address,
        entry.channels().unwrap_or(ProfileChannels::BY_ADDRESS),
    );
    let format = ProfileFormat::current(target);
    CiAction::Send(match entry.state() {
        ProfileState::Enabled => CiMessage::ProfileEnabled {
            source: own,
            format,
            profile,
        },
        ProfileState::Disabled => CiMessage::ProfileDisabled {
            source: own,
            format,
            profile,
        },
    })
}

impl ProfileResponder {
    /// The zones of the MPE Profile enabled now, lowest Manager first.
    pub(crate) fn mpe_zones(&self) -> Option<MpeZoneSet> {
        self.table.mpe_zones().zones
    }

    /// Takes `offer` as what the device offers, Reporting what changed.
    pub(crate) fn offer(&mut self, own: Muid, offer: &ProfileOffer) -> Vec<CiAction> {
        if *offer == self.offer {
            return Vec::new();
        }
        let before = std::mem::replace(&mut self.offer, offer.clone());
        let mut actions = Vec::new();
        for (&(address, profile), _) in before
            .entries
            .iter()
            .filter(|(key, _)| !offer.entries.contains_key(key))
        {
            if let Some(entry) = self.table.get(address, profile)
                && entry.state() == ProfileState::Enabled
            {
                let disabled =
                    ProfileEntry::new(ProfileState::Disabled, entry.channels(), entry.max());
                actions.push(report(own, address, profile, &disabled));
            }
            self.table.remove(address, profile);
            actions.push(CiAction::Send(CiMessage::ProfileRemoved {
                header: header(own, address),
                profile,
            }));
        }
        for (&(address, profile), entry) in &offer.entries {
            let was = before.entries.get(&(address, profile));
            if was == Some(entry) {
                continue;
            }
            let kept = self.table.get(address, profile).copied();
            let applied = entry.applied(address);
            self.table.set(address, profile, applied);
            if was.is_none() {
                actions.push(CiAction::Send(CiMessage::ProfileAdded {
                    header: header(own, address),
                    profile,
                }));
            }
            let reported = kept.is_none_or(|kept| {
                kept.state() != applied.state()
                    || (applied.state() == ProfileState::Enabled
                        && kept.channels() != applied.channels())
            });
            if reported && (was.is_some() || applied.state() == ProfileState::Enabled) {
                actions.push(report(own, address, profile, &applied));
            }
        }
        actions
    }

    /// The answer to `message` if it is a Profile Configuration inquiry or request
    /// to the device of `own`.
    pub(crate) fn on_message(&mut self, own: Muid, message: &CiMessage) -> Vec<CiAction> {
        match message {
            CiMessage::ProfileInquiry { header, to } if *to == own => {
                self.inquiry(own, header.device_id, header.source)
            }
            CiMessage::SetProfileOn {
                source,
                format,
                to,
                profile,
            } if *to == own => self.set_on(own, *source, *format, *profile),
            CiMessage::SetProfileOff {
                header,
                to,
                profile,
            } if *to == own => self.set_off(own, header.source, header.device_id, *profile),
            CiMessage::ProfileDetailsInquiry {
                header,
                to,
                profile,
                target,
            } if *to == own => {
                self.details(own, header.source, header.device_id, *profile, *target)
            }
            _ => Vec::new(),
        }
    }

    fn inquiry(&self, own: Muid, address: CiDeviceId, to: Muid) -> Vec<CiAction> {
        let reply = |at: CiDeviceId| {
            CiAction::Send(CiMessage::ProfileReply {
                header: header(own, at),
                to,
                profiles: self.lists(at),
            })
        };
        match address {
            CiDeviceId::FunctionBlock => self
                .table
                .addresses()
                .filter(|at| *at != CiDeviceId::FunctionBlock)
                .chain([CiDeviceId::FunctionBlock])
                .map(reply)
                .collect(),
            _ => vec![reply(address)],
        }
    }

    /// The two lists of the Reply for `address`; a list past what a Reply holds is cut.
    fn lists(&self, address: CiDeviceId) -> ProfileLists {
        let (mut enabled, mut disabled) = (Vec::new(), Vec::new());
        for (profile, entry) in self.table.profiles(address) {
            match entry.state() {
                ProfileState::Enabled => enabled.push(profile),
                ProfileState::Disabled => disabled.push(profile),
            }
        }
        let list = |mut ids: Vec<ProfileId>| {
            ids.truncate(ProfileList::MAX_LEN);
            ProfileList::new(ids).unwrap_or_default()
        };
        ProfileLists::new(list(enabled), list(disabled))
    }

    /// The NAK for a Profile not offered at `address`.
    fn not_offered(
        own: Muid,
        to: Muid,
        address: CiDeviceId,
        original: u8,
        profile: ProfileId,
    ) -> Vec<CiAction> {
        vec![CiAction::Send(nak_message(
            header(own, address),
            to,
            original,
            NakStatus::ProfileNotSupported,
            Some(profile),
        ))]
    }

    fn set_on(
        &mut self,
        own: Muid,
        peer: Muid,
        format: ProfileFormat,
        profile: ProfileId,
    ) -> Vec<CiAction> {
        let address = format.address();
        let (Some(offered), Some(kept)) = (
            self.offer.get(address, profile).copied(),
            self.table.get(address, profile).copied(),
        ) else {
            return Self::not_offered(own, peer, address, super::SUB_ID2_SET_PROFILE_ON, profile);
        };
        // A count of 0, or none from a Version 1 sender, takes the Channels the Profile last used.
        let count = counted(
            address,
            format
                .channels()
                .filter(|count| !count.is_by_address())
                .or(kept.channels())
                .unwrap_or(offered.max),
        );
        if offered.switching == PeerSwitching::Fixed
            || !kept.allows(count)
            || self.overlaps(address, profile, count)
        {
            return vec![report(own, address, profile, &kept)];
        }
        let now = ProfileEntry::new(ProfileState::Enabled, Some(count), kept.max());
        self.switched(own, peer, address, profile, kept, now)
    }

    fn set_off(
        &mut self,
        own: Muid,
        peer: Muid,
        address: CiDeviceId,
        profile: ProfileId,
    ) -> Vec<CiAction> {
        let (Some(offered), Some(kept)) = (
            self.offer.get(address, profile).copied(),
            self.table.get(address, profile).copied(),
        ) else {
            return Self::not_offered(own, peer, address, super::SUB_ID2_SET_PROFILE_OFF, profile);
        };
        if offered.switching == PeerSwitching::Fixed {
            return vec![report(own, address, profile, &kept)];
        }
        let now = ProfileEntry::new(ProfileState::Disabled, kept.channels(), kept.max());
        self.switched(own, peer, address, profile, kept, now)
    }

    /// Takes `now` for the Profile, Reports it, and says so if it changed.
    fn switched(
        &mut self,
        own: Muid,
        peer: Muid,
        address: CiDeviceId,
        profile: ProfileId,
        kept: ProfileEntry,
        now: ProfileEntry,
    ) -> Vec<CiAction> {
        let mut actions = vec![report(own, address, profile, &now)];
        if kept.state() != now.state() || kept.channels() != now.channels() {
            self.table.set(address, profile, now);
            actions.push(CiAction::ProfileSwitched {
                peer,
                profile,
                target: ProfileTarget::at(
                    address,
                    now.channels().unwrap_or(ProfileChannels::BY_ADDRESS),
                ),
                state: now.state(),
            });
        }
        actions
    }

    /// Whether enabling the MPE Profile at `address` for `count` Channels makes no
    /// zone, or a zone that shares a Channel with another enabled one (M2-120
    /// §3.4): any other Profile never does.
    fn overlaps(&self, address: CiDeviceId, profile: ProfileId, count: ProfileChannels) -> bool {
        let (ProfileId::MPE, CiDeviceId::Channel(manager)) = (profile, address) else {
            return false;
        };
        let zone = u8::try_from(count.get())
            .ok()
            .and_then(MpeChannels::new)
            .and_then(|channels| MpeZoneSpan::new(manager, channels));
        if zone.is_none() {
            return true;
        }
        let mut tentative = self.table.clone();
        tentative.set(
            address,
            profile,
            ProfileEntry::new(ProfileState::Enabled, Some(count), None),
        );
        !tentative.mpe_zones().refused.is_empty()
    }

    fn details(
        &self,
        own: Muid,
        to: Muid,
        address: CiDeviceId,
        profile: ProfileId,
        target: InquiryTarget,
    ) -> Vec<CiAction> {
        let Some(entry) = self.table.get(address, profile) else {
            return Self::not_offered(
                own,
                to,
                address,
                super::SUB_ID2_PROFILE_DETAILS_INQUIRY,
                profile,
            );
        };
        if target != InquiryTarget::NUMBER_OF_CHANNELS {
            return vec![CiAction::Send(nak_message(
                header(own, address),
                to,
                super::SUB_ID2_PROFILE_DETAILS_INQUIRY,
                NakStatus::Nak,
                Some(profile),
            ))];
        }
        let in_use = match entry.state() {
            ProfileState::Enabled => entry.channels().unwrap_or(ProfileChannels::BY_ADDRESS),
            ProfileState::Disabled => ProfileChannels::BY_ADDRESS,
        };
        let detail = ChannelsDetail {
            in_use,
            max: entry.max().unwrap_or(ProfileChannels::BY_ADDRESS),
        };
        vec![CiAction::Send(CiMessage::ProfileDetailsReply {
            header: header(own, address),
            to,
            profile,
            target,
            data: detail.to_data(),
        })]
    }
}

#[cfg(test)]
mod tests;
