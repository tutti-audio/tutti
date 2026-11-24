//! The Profiles of a MIDI-CI peer, as the session holds and drives them: what
//! the peer supports ([`PeerProfiles`]), the handles an app asks to switch one
//! from ([`Switcher`]), the request itself ([`ProfileSwitch`]) and what
//! happened to its table ([`ProfilesChange`]).
//!
//! The session is the Initiator (M2-101 §7): it asks every peer that declared
//! Profile Configuration for its Profiles when it is found, and keeps the answer
//! up to date from the peer's own reports. It never lists a Profile the peer
//! did not, and a request is made only from a handle the peer's own list gave,
//! so an app cannot name a Profile or an address the peer lacks. The handle
//! is a snapshot: the peer may remove the Profile before the request is sent,
//! and that is refused ([`ProfileError::NotListed`]).
//!
//! What the spec's request fields allow is in the handle's type. The Number
//! Channels Requested of a Set Profile On (M2-101 §7.8) is a count of Channels
//! only where the Profile is on one Channel: a [`ChannelSwitcher`] takes a
//! [`ChannelCount`], and a [`WholeSwitcher`] (on the Group or Function Block)
//! has no count to get wrong, since there the field is the width of what is
//! addressed. A count of 0 on a Channel is unwritable. The limit a peer
//! declares in a Profile Details reply (M2-102 §2.5.1) is not known to this
//! module: it never sends that inquiry, so a count above it is the app's to
//! avoid, and the peer's Disabled Report is how a refusal arrives.

use std::sync::Arc;

use tutti_midi_ci::{
    CiDeviceId, ProfileChange, ProfileChannels, ProfileEntry, ProfileId, ProfileRequest,
    ProfileState, ProfileTable, ProfileTarget, SpecificData, Whole,
};
use tutti_midi_protocol::{ChannelResponse, PitchBendSensitivity, ProfileZones};
use tutti_ump::{MidiChannel, MpeOverlap};

use crate::peer::{PeerRef, PeerRequestError};

tutti_units::ranged! {
    /// The Channels a Profile is asked to use when it is enabled on a Channel: the
    /// Manager and its Members, at least 1 and at most 14 bits (M2-101 §7.8).
    /// [`ProfileChannels`] is the wire field, which also holds 0 (the width of
    /// the address); a request to enable names a count, never 0.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    ChannelCount(u16), NotAChannelCount, "a Channel count (1 to 16383)",
    |raw| raw >= 1 && raw <= 0x3FFF
}

impl ChannelCount {
    /// One Channel: a Profile on a single Channel.
    pub const ONE: Self = Self::of(1);
}

/// What a peer's Profiles are at one address, as of when it was read.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Listed {
    at: PeerRef,
    profile: ProfileId,
    entry: ProfileEntry,
}

/// A Profile the peer lists on one Channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelSwitcher {
    listed: Listed,
    channel: MidiChannel,
}

/// A Profile the peer lists on the whole Group or Function Block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WholeSwitcher {
    listed: Listed,
    whole: Whole,
}

/// A Profile a peer lists, at the address it lists it: the handle a
/// [`ProfileSwitch`] is made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Switcher {
    /// On one Channel: enabling it says how many Channels it is to use.
    Channel(ChannelSwitcher),
    /// On the Group or Function Block: enabling it has no count.
    Whole(WholeSwitcher),
}

/// What a switch does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Switch {
    On(ProfileTarget),
    Off(CiDeviceId),
}

/// A request that a peer enable or disable one Profile it lists
/// ([`MidiSession::set_profile`](crate::MidiSession::set_profile)), whose end
/// is an [`Answer::Switch`](crate::Answer::Switch). Made only
/// from a [`Switcher`], and it names the input device that handle was read
/// from, so it cannot be sent to another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSwitch {
    pub(crate) at: PeerRef,
    pub(crate) profile: ProfileId,
    pub(crate) switch: Switch,
}

impl ProfileSwitch {
    /// The peer, as every handle of MIDI-CI names one.
    #[must_use]
    pub const fn at(&self) -> &PeerRef {
        &self.at
    }

    /// What the peer is asked: the Profile and the address, on or off.
    #[must_use]
    pub const fn request(&self) -> ProfileRequest {
        match self.switch {
            Switch::On(target) => ProfileRequest::SetOn {
                address: target.address(),
                profile: self.profile,
            },
            Switch::Off(address) => ProfileRequest::SetOff {
                address,
                profile: self.profile,
            },
        }
    }

    /// Whether `peers`, the Profiles an input's peers list now, still hold the
    /// peer and the Profile at the address the switch names.
    ///
    /// # Errors
    ///
    /// [`PeerRequestError::NoSuchPeer`] (as [`ProfileError::Peer`]) when no peer of
    /// `peers` is the switch's,
    /// [`ProfileError::NotListed`] when it no longer lists the Profile there.
    pub fn check<'a>(
        &self,
        peers: impl IntoIterator<Item = &'a PeerProfiles>,
    ) -> Result<(), ProfileError> {
        let peer = self.at.find(peers, |p| &p.at)?;
        let address = match self.switch {
            Switch::On(target) => target.address(),
            Switch::Off(address) => address,
        };
        if peer.table().get(address, self.profile).is_none() {
            return Err(ProfileError::NotListed);
        }
        Ok(())
    }
}

impl Listed {
    fn on(&self, target: ProfileTarget) -> ProfileSwitch {
        ProfileSwitch {
            at: self.at.clone(),
            profile: self.profile,
            switch: Switch::On(target),
        }
    }

    fn off(&self, address: CiDeviceId) -> ProfileSwitch {
        ProfileSwitch {
            at: self.at.clone(),
            profile: self.profile,
            switch: Switch::Off(address),
        }
    }
}

impl ChannelSwitcher {
    /// Asks the peer to enable the Profile on this Channel, as the Manager of
    /// `channels` Channels in all.
    #[must_use]
    pub fn enable(&self, channels: ChannelCount) -> ProfileSwitch {
        self.listed.on(ProfileTarget::Channel {
            channel: self.channel,
            channels: ProfileChannels::of(channels.get()),
        })
    }

    /// Asks the peer to disable the Profile on this Channel.
    #[must_use]
    pub fn disable(&self) -> ProfileSwitch {
        self.listed.off(CiDeviceId::Channel(self.channel))
    }

    /// The Channel.
    #[must_use]
    pub const fn channel(&self) -> MidiChannel {
        self.channel
    }

    /// The Profile.
    #[must_use]
    pub const fn profile(&self) -> ProfileId {
        self.listed.profile
    }

    /// Whether the peer had it enabled when this was read.
    #[must_use]
    pub const fn state(&self) -> ProfileState {
        self.listed.entry.state()
    }

    /// The Channel count of the peer's last Enabled or Disabled Report of it, if
    /// a Version 2 peer sent one.
    #[must_use]
    pub const fn channels(&self) -> Option<ProfileChannels> {
        self.listed.entry.channels()
    }
}

impl WholeSwitcher {
    /// Asks the peer to enable the Profile on the whole Group or Function Block.
    #[must_use]
    pub fn enable(&self) -> ProfileSwitch {
        self.listed.on(self.whole.into())
    }

    /// Asks the peer to disable the Profile on the whole Group or Function Block.
    #[must_use]
    pub fn disable(&self) -> ProfileSwitch {
        self.listed.off(self.whole.into())
    }

    /// Whether it is the Group's or the Function Block's.
    #[must_use]
    pub const fn whole(&self) -> Whole {
        self.whole
    }

    /// The Profile.
    #[must_use]
    pub const fn profile(&self) -> ProfileId {
        self.listed.profile
    }

    /// Whether the peer had it enabled when this was read.
    #[must_use]
    pub const fn state(&self) -> ProfileState {
        self.listed.entry.state()
    }

    /// The Channel count of the peer's last Enabled or Disabled Report of it, if
    /// a Version 2 peer sent one.
    #[must_use]
    pub const fn channels(&self) -> Option<ProfileChannels> {
        self.listed.entry.channels()
    }
}

impl Switcher {
    /// The Profile.
    #[must_use]
    pub const fn profile(&self) -> ProfileId {
        match self {
            Self::Channel(p) => p.profile(),
            Self::Whole(p) => p.profile(),
        }
    }

    /// Whether the peer had it enabled when this was read.
    #[must_use]
    pub const fn state(&self) -> ProfileState {
        match self {
            Self::Channel(p) => p.state(),
            Self::Whole(p) => p.state(),
        }
    }

    /// The Channel count of the peer's last Enabled or Disabled Report of it, if
    /// a Version 2 peer sent one.
    #[must_use]
    pub const fn channels(&self) -> Option<ProfileChannels> {
        match self {
            Self::Channel(p) => p.channels(),
            Self::Whole(p) => p.channels(),
        }
    }

    /// Asks the peer to disable the Profile at its address.
    #[must_use]
    pub fn disable(&self) -> ProfileSwitch {
        match self {
            Self::Channel(p) => p.disable(),
            Self::Whole(p) => p.disable(),
        }
    }
}

/// The Profiles one found peer supports (M2-101 §7), as the session last read
/// them. Shared by pointer, so a held one stays as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerProfiles {
    pub(crate) at: PeerRef,
    pub(crate) known: bool,
    pub(crate) table: ProfileTable,
}

impl PeerProfiles {
    /// The peer, as every handle of MIDI-CI names one.
    #[must_use]
    pub const fn at(&self) -> &PeerRef {
        &self.at
    }

    /// Whether the peer's Profile Inquiry ended with its final Reply, so the
    /// table is its whole declaration; `false` while the inquiry is out or
    /// after it timed out or was refused.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        self.known
    }

    /// The table: per address, each Profile and its state.
    #[must_use]
    pub const fn table(&self) -> &ProfileTable {
        &self.table
    }

    /// The zones of the MPE Profile (M2-120) the peer has enabled, lowest
    /// Manager first: one per Channel it lists the Profile enabled on, with the
    /// Number of Channels its Enabled Report gave. A Profile enabled without
    /// a count (a Version 1 peer sends none), or with one that makes no zone
    /// (fewer than two Channels, or past Channel 16), is not a zone, and a
    /// zone sharing a Channel with a lower one is left out (section 3.4);
    /// [`refused_mpe_zones`](Self::refused_mpe_zones) lists those.
    #[must_use]
    pub fn mpe_zones(&self) -> Option<ProfileZones> {
        self.table
            .mpe_zones()
            .zones
            .map(|set| ProfileZones::new(&set, PitchBendSensitivity::MPE_PROFILE_DEFAULT))
    }

    /// The zones the peer enabled that [`mpe_zones`](Self::mpe_zones) leaves
    /// out, each as the overlap with the zone held that refused it.
    #[must_use]
    pub fn refused_mpe_zones(&self) -> Vec<MpeOverlap> {
        self.table.mpe_zones().refused
    }

    /// Every Profile the peer lists, as handles to switch them from.
    pub fn listed(&self) -> impl Iterator<Item = Switcher> + '_ {
        self.table.addresses().flat_map(move |address| {
            self.table.profiles(address).map(move |(profile, entry)| {
                let listed = Listed {
                    at: self.at.clone(),
                    profile,
                    entry: *entry,
                };
                match address {
                    CiDeviceId::Channel(channel) => {
                        Switcher::Channel(ChannelSwitcher { listed, channel })
                    }
                    CiDeviceId::Group => Switcher::Whole(WholeSwitcher {
                        listed,
                        whole: Whole::Group,
                    }),
                    CiDeviceId::FunctionBlock => Switcher::Whole(WholeSwitcher {
                        listed,
                        whole: Whole::FunctionBlock,
                    }),
                }
            })
        })
    }
}

/// A snapshot of every peer's Profiles on one input.
pub(crate) type Snapshot = Arc<[PeerProfiles]>;

/// A change of one peer's Profiles, once each from
/// [`MidiSession::poll`](crate::MidiSession::poll); the [`PeerId`](crate::PeerId)
/// beside it in [`Polled::Profile`](crate::Polled::Profile) names the peer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProfilesChange {
    /// The peer's final Reply to Profile Inquiry arrived: its table is whole.
    Known,
    /// A Report (Added, Removed, Enabled or Disabled) changed one Profile.
    Changed {
        /// Where the Profile is.
        address: CiDeviceId,
        /// Which.
        profile: ProfileId,
        /// What happened to it.
        change: ProfileChange,
    },
    /// The peer did not finish answering Profile Inquiry in time: the table
    /// keeps what it said.
    InquiryTimedOut,
    /// The peer NAKed the Profile Inquiry: it has no Profile Configuration to
    /// ask about.
    InquiryRefused,
    /// The peer sent Profile Specific Data (M2-101 §7.12): a Profile's own
    /// message, which [`mpe_channel_response`](ProfilesChange::mpe_channel_response)
    /// reads for the MPE Profile.
    SpecificData {
        /// The Device ID it came on.
        address: CiDeviceId,
        /// The Profile it belongs to.
        profile: ProfileId,
        /// The data, uninterpreted.
        data: SpecificData,
    },
    /// A zone the peer enabled is not one of its
    /// [`mpe_zones`](PeerProfiles::mpe_zones): it is left out, and this says why.
    /// Sent after the change that enabled it.
    MpeZoneRefused(MpeOverlap),
    /// The peer is gone (lost, or MIDI-CI ended): its Profiles and the
    /// Initiator that kept them went with it.
    Dropped,
}

impl ProfilesChange {
    /// The Channel Response Type Notification of the MPE Profile (M2-120
    /// section 3.6.3) this is, with the Channel it came on (the zone's
    /// Manager), or `None` for any other change, another Profile's data, a
    /// notification on the Group or Function Block, and data that is not one
    /// of the notification's two values.
    #[must_use]
    pub fn mpe_channel_response(&self) -> Option<(MidiChannel, ChannelResponse)> {
        let Self::SpecificData {
            address: CiDeviceId::Channel(channel),
            profile,
            data,
            ..
        } = self
        else {
            return None;
        };
        (*profile == ProfileId::MPE)
            .then(|| ChannelResponse::from_data(data.as_bytes()))
            .flatten()
            .map(|response| (*channel, response))
    }
}

/// A [`ProfileSwitch`] that cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum ProfileError {
    /// The peer cannot be asked at all: the input is closed, or the peer is not
    /// one it lists for Profile Configuration.
    #[error(transparent)]
    Peer(#[from] PeerRequestError),
    /// The peer does not list that Profile at that address now (the handle is
    /// older than a Removed Report).
    #[error("the peer does not list that Profile at that address")]
    NotListed,
}
