//! MPE zone layout: which of the sixteen MIDI 1.0 channels are master and which
//! are members, under MPE 1.1 (RP-053 / M1-100, which calls the master channel
//! the Master) or the MIDI-CI MPE Profile (M2-120, which calls it the Manager),
//! and the MPE Configuration Message that declares an MPE 1.1 zone.
//!
//! Every channel here is a [`MidiChannel`], **0-indexed**: `0` is the channel a
//! device's front panel calls "Ch1", because that is what UMP carries. The one
//! place the distinction bites is the two MPE 1.1 zone anchors: the lower
//! zone's master is `0` and the upper zone's is `15`.

use alloc::vec::Vec;
use tutti_ump::{
    Address, ControlValue, Message, MidiChannel, MidiGroup, MpeChannels, MpeZoneSet, MpeZoneSpan,
    Rpn, Ump, Voice2,
};

use crate::{ChannelResponse, PitchBendSensitivity};

/// Which of the two RP-053 zones an MPE 1.1 configuration describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MpeZone {
    /// Master on channel 0, members counting up from channel 1.
    Lower,
    /// Master on channel 15, members counting down from channel 14.
    Upper,
}

impl MpeZone {
    /// The zone's master channel: `0` for the lower zone, `15` for the upper
    /// one. Zone-wide messages (bend, pressure) arrive here, and so does the
    /// zone's MPE Configuration Message.
    #[inline]
    #[must_use]
    pub const fn master_channel(self) -> MidiChannel {
        match self {
            Self::Lower => MidiChannel::FIRST,
            Self::Upper => MidiChannel::LAST,
        }
    }
}

tutti_units::ranged! {
    /// How many Member Channels an MPE zone claims: `1..=15`, the sixteen
    /// channels less the zone's own master (RP-053). A count of `0` is not a
    /// zone: in an MPE Configuration Message it disables one
    /// ([`Mcm::Disable`]). A Profile zone has at most the channels above its
    /// Manager.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    MemberCount(u8), NotMemberCount, "within 1..=15", |raw| raw >= 1 && raw <= 15
}

/// An MPE Configuration Message (RP-053, M2-104 section 7.4.7) read off the
/// wire: the zone it addresses, and either its Number of Member Channels or the
/// value `0` that disables the zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mcm {
    /// Configure `zone` with `members` Member Channels.
    Configure {
        /// The zone the message addresses, by its master channel.
        zone: MpeZone,
        /// The zone's new member count.
        members: MemberCount,
    },
    /// Disable `zone`: the message carried a count of `0`.
    Disable {
        /// The zone the message addresses, by its master channel.
        zone: MpeZone,
    },
}

/// Where a zone's channels sit: its master (MPE 1.1's Master, the Profile's
/// Manager) and the member channels beside it, which are the part of a zone
/// that every kind shares. The master is the channel directly below the
/// members, or on channel 16 directly above them, and the whole zone ends by
/// channel 16.
///
/// A zone of either kind hands its layout out ([`Mpe11Zone::layout`],
/// [`ProfileZone::layout`]); a [`Midi1Translator`](crate::Midi1Translator)
/// reads nothing else of a zone, so it takes this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "MpeLayoutRaw", into = "MpeLayoutRaw")
)]
pub struct MpeLayout {
    master: MidiChannel,
    first_member: MidiChannel,
    member_count: MemberCount,
}

/// The persisted shape of an [`MpeLayout`], checked by `TryFrom` on the way
/// back in.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct MpeLayoutRaw {
    master: MidiChannel,
    first_member: MidiChannel,
    member_count: MemberCount,
}

/// A persisted layout that is not a layout.
#[cfg(feature = "serde")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("an MPE layout's master is directly below its members, or channel 16 directly above them")]
pub struct InvalidMpeLayout;

#[cfg(feature = "serde")]
impl From<MpeLayout> for MpeLayoutRaw {
    fn from(l: MpeLayout) -> Self {
        Self {
            master: l.master,
            first_member: l.first_member,
            member_count: l.member_count,
        }
    }
}

#[cfg(feature = "serde")]
impl TryFrom<MpeLayoutRaw> for MpeLayout {
    type Error = InvalidMpeLayout;

    fn try_from(raw: MpeLayoutRaw) -> Result<Self, Self::Error> {
        Self::new(raw.master, raw.first_member, raw.member_count).ok_or(InvalidMpeLayout)
    }
}

impl MpeLayout {
    /// The layout of `member_count` members from `first_member` up with
    /// `master` directly below the first (an MPE 1.1 lower zone, a Profile
    /// zone) or on channel 16 directly above the last (an MPE 1.1 upper zone),
    /// or `None` for any other placement.
    /// A persisted layout is read back through it.
    #[cfg(feature = "serde")]
    const fn new(
        master: MidiChannel,
        first_member: MidiChannel,
        member_count: MemberCount,
    ) -> Option<Self> {
        let end = first_member.get() + member_count.get();
        let below = master.get() + 1 == first_member.get();
        let above = master.get() == MidiChannel::LAST.get() && master.get() == end;
        if end <= MidiChannel::COUNT && (below || above) {
            Some(Self::unchecked(master, first_member, member_count))
        } else {
            None
        }
    }

    /// A layout its caller has placed as [`new`](Self::new) requires.
    const fn unchecked(
        master: MidiChannel,
        first_member: MidiChannel,
        member_count: MemberCount,
    ) -> Self {
        Self {
            master,
            first_member,
            member_count,
        }
    }

    /// The zone's master channel. Zone-wide messages (bend, pressure) arrive
    /// here.
    #[inline]
    #[must_use]
    pub const fn master_channel(&self) -> MidiChannel {
        self.master
    }

    /// How many member channels the zone claims.
    #[inline]
    #[must_use]
    pub const fn member_count(&self) -> MemberCount {
        self.member_count
    }

    /// How many channels the zone has, the master and its members.
    #[inline]
    #[must_use]
    pub const fn channel_count(&self) -> MpeChannels {
        MpeChannels::of(self.member_count.get() + 1)
    }

    /// The lowest member channel: the first of
    /// [`member_channels`](Self::member_channels). A zone has at least one
    /// member, so it always exists.
    #[inline]
    #[must_use]
    pub const fn first_member(&self) -> MidiChannel {
        self.first_member
    }

    /// The member channels, in ascending order.
    pub fn member_channels(&self) -> impl Iterator<Item = MidiChannel> + Clone {
        let first = self.first_member.get();
        (first..first + self.member_count.get()).filter_map(MidiChannel::new)
    }

    /// Reports whether `channel` is the zone's master.
    #[inline]
    #[must_use]
    pub fn is_master_channel(&self, channel: MidiChannel) -> bool {
        channel == self.master
    }

    /// Reports whether `channel` is one of the zone's member channels (never
    /// the master).
    #[inline]
    #[must_use]
    pub fn is_member_channel(&self, channel: MidiChannel) -> bool {
        (self.first_member.get()..self.first_member.get() + self.member_count.get())
            .contains(&channel.get())
    }

    /// Reports whether `channel` belongs to the zone in either role.
    #[inline]
    #[must_use]
    pub fn handles_channel(&self, channel: MidiChannel) -> bool {
        self.span().contains(channel)
    }

    /// The channels the zone occupies, master included, as a span from its
    /// lowest channel.
    fn span(&self) -> MpeZoneSpan {
        let lowest = MidiChannel::masked(self.master.get().min(self.first_member.get()));
        // `new` keeps the zone's last channel by 16.
        MpeZoneSpan::new(lowest, self.channel_count()).expect("a zone ends by channel 16")
    }
}

/// One MPE 1.1 zone (RP-053 / M1-100): a side, which fixes the master at
/// channel 1 or 16 and the way the members count, a bend range for the master
/// and one for the members, a member count and a response. It is what an MPE
/// Configuration Message declares ([`to_mcm`](Self::to_mcm),
/// [`from_mcm`](Self::from_mcm)).
///
/// The MIDI-CI MPE Profile's zone is a [`ProfileZone`], which has no
/// Configuration Message:
///
/// ```compile_fail,E0599
/// use tutti_midi_protocol::{PitchBendSensitivity, ProfileZone};
/// use tutti_ump::{MidiChannel, MpeChannels, MpeZoneSpan};
///
/// let span = MpeZoneSpan::new(MidiChannel::of(5), MpeChannels::of(4)).unwrap();
/// let _ = ProfileZone::new(span, PitchBendSensitivity::MPE_PROFILE_DEFAULT).to_mcm();
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Mpe11Zone {
    side: MpeZone,
    member_count: MemberCount,
    master: PitchBendSensitivity,
    member: PitchBendSensitivity,
    response: ChannelResponse,
}

impl Mpe11Zone {
    /// A zone on `side` of `member_count` members, its master channel's bend
    /// range `master` and its members' `member`, with a polyphonic response.
    /// A receiver of an MPE Configuration Message resets the ranges to
    /// [`PitchBendSensitivity::MPE_MASTER_DEFAULT`] and
    /// [`PitchBendSensitivity::MPE_MEMBER_DEFAULT`].
    ///
    /// No zone describes a disabled one ([`Mcm::Disable`]).
    #[must_use]
    pub const fn new(
        side: MpeZone,
        member_count: MemberCount,
        master: PitchBendSensitivity,
        member: PitchBendSensitivity,
    ) -> Self {
        Self {
            side,
            member_count,
            master,
            member,
            response: ChannelResponse::Polyphonic,
        }
    }

    /// [`new`](Self::new) for the lower zone: master Ch1 (0), members from Ch2.
    #[must_use]
    pub const fn lower(
        member_count: MemberCount,
        master: PitchBendSensitivity,
        member: PitchBendSensitivity,
    ) -> Self {
        Self::new(MpeZone::Lower, member_count, master, member)
    }

    /// [`new`](Self::new) for the upper zone: master Ch16 (15), members from
    /// Ch15 counting down.
    #[must_use]
    pub const fn upper(
        member_count: MemberCount,
        master: PitchBendSensitivity,
        member: PitchBendSensitivity,
    ) -> Self {
        Self::new(MpeZone::Upper, member_count, master, member)
    }

    /// Which of the two zones this is, and hence which direction members count
    /// in.
    #[inline]
    #[must_use]
    pub const fn side(&self) -> MpeZone {
        self.side
    }

    /// The zone's master channel ([`MpeZone::master_channel`]).
    #[inline]
    #[must_use]
    pub const fn master_channel(&self) -> MidiChannel {
        self.side.master_channel()
    }

    /// How many member channels the zone claims.
    #[inline]
    #[must_use]
    pub const fn member_count(&self) -> MemberCount {
        self.member_count
    }

    /// The master channel's bend range, which applies to every note in the zone
    /// at once (RP-053 default 2 semitones).
    #[inline]
    #[must_use]
    pub const fn master_range(&self) -> PitchBendSensitivity {
        self.master
    }

    /// The members' bend range, which applies to a channel's single note
    /// (default 48; the master's 2 would bend a per-note gesture a 24th as
    /// far).
    #[inline]
    #[must_use]
    pub const fn member_range(&self) -> PitchBendSensitivity {
        self.member
    }

    /// The bend range that applies to `channel`: the master's on the master
    /// channel, the members' anywhere else.
    ///
    /// Use this rather than assuming a role's range: picking the wrong one is
    /// a silent 24x pitch error.
    #[inline]
    #[must_use]
    pub fn pitch_bend_range_for(&self, channel: MidiChannel) -> PitchBendSensitivity {
        if channel == self.master_channel() {
            self.master
        } else {
            self.member
        }
    }

    /// The mode the members start in: MPE 1.1's MIDI Mode 3 (polyphonic) or
    /// Mode 4 (M1-100 section 2.2.4.3). A Mono Mode On or Poly Mode On from
    /// the wire changes it.
    #[inline]
    #[must_use]
    pub const fn response(&self) -> ChannelResponse {
        self.response
    }

    /// The zone with `response` as the mode its members start in
    /// ([`response`](Self::response)).
    #[must_use]
    pub const fn channel_response(mut self, response: ChannelResponse) -> Self {
        self.response = response;
        self
    }

    /// Where the zone's channels sit: `1..=n` under the master at channel 0
    /// for the lower zone, `15-n..=14` under the master at channel 15 for the
    /// upper one.
    #[must_use]
    pub const fn layout(&self) -> MpeLayout {
        match self.side {
            // `MemberCount` holds at most 15, so the channels end by 16.
            MpeZone::Lower => MpeLayout::unchecked(
                MidiChannel::FIRST,
                MidiChannel::masked(1),
                self.member_count,
            ),
            MpeZone::Upper => MpeLayout::unchecked(
                MidiChannel::LAST,
                MidiChannel::masked(15 - self.member_count.get()),
                self.member_count,
            ),
        }
    }

    /// Encodes this zone as an **MPE Configuration Message** (MCM): a MIDI 2.0
    /// Registered Controller (RPN) on the zone's *master* channel, address
    /// [`Rpn::MPE_CONFIGURATION`], data = member count. Per RP-053 / M2-104, the
    /// master channel (Ch1 lower / Ch16 upper) is what a receiver reads to know
    /// which zone is being configured.
    ///
    /// The 7-bit member count sits in the top 7 bits of the 32-bit data field
    /// (MSB-aligned, the spec's 7 to 32 convention) so it round-trips exactly.
    #[must_use]
    pub fn to_mcm(&self) -> Ump {
        Message::Midi2(
            Address {
                group: MidiGroup::FIRST,
                channel: self.master_channel(),
            },
            Voice2::Registered {
                bank: Rpn::MPE_CONFIGURATION.bank(),
                index: Rpn::MPE_CONFIGURATION.index(),
                value: ControlValue::new(u32::from(self.member_count.get()) << 25),
            },
        )
        .into()
    }

    /// Decodes an MCM into the zone it addresses and what it asks, if `ump` is
    /// an MPE Configuration Message (RPN bank `0x00`, index `0x06`) on a zone's
    /// master channel. `None` for any other message.
    ///
    /// A count of `0` is the spec's disable-this-zone value, an
    /// [`Mcm::Disable`]. A message is rejected, not clamped, when RP-053 does
    /// not allow it: an MCM goes on Ch1 or Ch16 only, and a zone has at most
    /// 15 members. Clamping would silently reconfigure the zone to something
    /// the sender never asked for; `None` lets the caller keep the zone it had.
    #[must_use]
    pub fn from_mcm(ump: &Ump) -> Option<Mcm> {
        let Ok(Message::Midi2(address, Voice2::Registered { bank, index, value })) =
            (*ump).decode()
        else {
            return None;
        };
        if Rpn::new(bank, index) != Rpn::MPE_CONFIGURATION {
            return None;
        }
        let zone = [MpeZone::Lower, MpeZone::Upper]
            .into_iter()
            .find(|z| z.master_channel() == address.channel)?;
        // The member count is the top 7 bits of the 32-bit data.
        match (value.get() >> 25) as u8 {
            0 => Some(Mcm::Disable { zone }),
            count => MemberCount::new(count).map(|members| Mcm::Configure { zone, members }),
        }
    }
}

/// One zone of the MIDI-CI MPE Profile (M2-120): a Manager Channel enabled
/// anywhere, the channels above it, the one bend range of the Manager and
/// every Member (section 3.5), and the Channel Response of its Members
/// (section 3.6). A MIDI-CI peer sets it through the Profile; it has no MPE
/// Configuration Message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "ProfileZoneRaw", into = "ProfileZoneRaw")
)]
pub struct ProfileZone {
    span: MpeZoneSpan,
    range: PitchBendSensitivity,
    response: ChannelResponse,
}

/// The persisted shape of a [`ProfileZone`], checked by `TryFrom` on the way
/// back in.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct ProfileZoneRaw {
    manager: MidiChannel,
    member_count: MemberCount,
    range: PitchBendSensitivity,
    response: ChannelResponse,
}

/// A persisted Profile zone whose members run past channel 16.
#[cfg(feature = "serde")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("a Profile zone has a manager, 1 or more members, and ends by channel 16")]
pub struct InvalidProfileZone;

#[cfg(feature = "serde")]
impl From<ProfileZone> for ProfileZoneRaw {
    fn from(z: ProfileZone) -> Self {
        Self {
            manager: z.manager(),
            member_count: z.layout().member_count(),
            range: z.range,
            response: z.response,
        }
    }
}

#[cfg(feature = "serde")]
impl TryFrom<ProfileZoneRaw> for ProfileZone {
    type Error = InvalidProfileZone;

    fn try_from(raw: ProfileZoneRaw) -> Result<Self, Self::Error> {
        let channels = MpeChannels::new(raw.member_count.get() + 1).ok_or(InvalidProfileZone)?;
        let span = MpeZoneSpan::new(raw.manager, channels).ok_or(InvalidProfileZone)?;
        Ok(Self::new(span, raw.range).channel_response(raw.response))
    }
}

impl ProfileZone {
    /// A Profile zone over `span`'s channels (the Number of Channels of Set
    /// Profile On, appendix A.3), with `range` as the one bend range of the
    /// Manager and every Member (the Profile starts at
    /// [`PitchBendSensitivity::MPE_PROFILE_DEFAULT`]), and a polyphonic
    /// response. Any channel can be a Manager.
    #[must_use]
    pub const fn new(span: MpeZoneSpan, range: PitchBendSensitivity) -> Self {
        Self {
            span,
            range,
            response: ChannelResponse::Polyphonic,
        }
    }

    /// The channels the zone occupies.
    #[inline]
    #[must_use]
    pub const fn span(&self) -> MpeZoneSpan {
        self.span
    }

    /// The Manager Channel, the zone's lowest.
    #[inline]
    #[must_use]
    pub const fn manager(&self) -> MidiChannel {
        self.span.manager()
    }

    /// The one bend range of the Manager and every Member.
    #[inline]
    #[must_use]
    pub const fn range(&self) -> PitchBendSensitivity {
        self.range
    }

    /// How the members take notes (M2-120 section 3.6).
    #[inline]
    #[must_use]
    pub const fn response(&self) -> ChannelResponse {
        self.response
    }

    /// The zone with `response` as how its members take notes
    /// ([`response`](Self::response)).
    #[must_use]
    pub const fn channel_response(mut self, response: ChannelResponse) -> Self {
        self.response = response;
        self
    }

    /// Where the zone's channels sit: the Members count up from the Manager.
    #[must_use]
    pub const fn layout(&self) -> MpeLayout {
        let manager = self.span.manager();
        // A span has two channels at least, so the Manager has a Member, and
        // it ends by channel 16.
        MpeLayout::unchecked(
            manager,
            MidiChannel::masked(manager.get() + 1),
            MemberCount::of(self.span.channels().get() - 1),
        )
    }
}

/// A list of zones that is not a zone layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum InvalidMpeZones {
    /// The list holds no zone.
    #[error("no MPE zone")]
    Empty,
    /// Two zones share a channel.
    #[error("MPE zones share a channel")]
    Overlap,
}

/// The MPE 1.1 zones of an instrument or of one Group: a lower zone, an upper
/// zone or both, sharing no channel.
///
/// Sixteen channels less two masters leave 14 members to share between the
/// lower zone's `1..=l` and the upper zone's `15-u..=14`. The zones are kept
/// by side, so equal sets compare equal whatever order their zones were added
/// in. There is no empty set: no zones is no `Mpe11Zones`
/// ([`without`](Self::without)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "Vec<Mpe11Zone>", into = "Vec<Mpe11Zone>")
)]
pub struct Mpe11Zones {
    lower: Option<Mpe11Zone>,
    upper: Option<Mpe11Zone>,
}

impl From<Mpe11Zones> for Vec<Mpe11Zone> {
    fn from(zones: Mpe11Zones) -> Self {
        zones.zones().copied().collect()
    }
}

impl TryFrom<Vec<Mpe11Zone>> for Mpe11Zones {
    type Error = InvalidMpeZones;

    fn try_from(zones: Vec<Mpe11Zone>) -> Result<Self, Self::Error> {
        let mut zones = zones.into_iter();
        let first = zones.next().ok_or(InvalidMpeZones::Empty)?;
        zones.try_fold(Self::single(first), |set, zone| {
            if set.on(zone.side()).is_some() {
                return Err(InvalidMpeZones::Overlap);
            }
            set.with(zone).ok_or(InvalidMpeZones::Overlap)
        })
    }
}

impl Mpe11Zones {
    /// The set of `zone` alone.
    #[must_use]
    pub const fn single(zone: Mpe11Zone) -> Self {
        match zone.side() {
            MpeZone::Lower => Self {
                lower: Some(zone),
                upper: None,
            },
            MpeZone::Upper => Self {
                lower: None,
                upper: Some(zone),
            },
        }
    }

    /// The set with `zone` on its side, in place of the zone that was there,
    /// or `None` when it shares a channel with the zone on the other side.
    #[must_use]
    pub fn with(self, zone: Mpe11Zone) -> Option<Self> {
        let other = match zone.side() {
            MpeZone::Lower => self.upper,
            MpeZone::Upper => self.lower,
        };
        if other.is_some_and(|held| held.layout().span().overlaps(zone.layout().span())) {
            return None;
        }
        Some(match zone.side() {
            MpeZone::Lower => Self {
                lower: Some(zone),
                ..self
            },
            MpeZone::Upper => Self {
                upper: Some(zone),
                ..self
            },
        })
    }

    /// The set without its zone on `side`, or `None` when that was the only
    /// one. A side with no zone leaves the set as it is.
    #[must_use]
    pub const fn without(self, side: MpeZone) -> Option<Self> {
        let rest = match side {
            MpeZone::Lower => Self {
                lower: None,
                ..self
            },
            MpeZone::Upper => Self {
                upper: None,
                ..self
            },
        };
        if rest.lower.is_none() && rest.upper.is_none() {
            None
        } else {
            Some(rest)
        }
    }

    /// The zone on `side`, if the set has one.
    #[must_use]
    pub const fn on(&self, side: MpeZone) -> Option<&Mpe11Zone> {
        match side {
            MpeZone::Lower => self.lower.as_ref(),
            MpeZone::Upper => self.upper.as_ref(),
        }
    }

    /// The zones, the lower one first.
    pub fn zones(&self) -> impl Iterator<Item = &Mpe11Zone> + Clone {
        self.lower.iter().chain(self.upper.iter())
    }

    /// The zone that owns `channel` (as its master or one of its members), if
    /// any. The zones share no channel, so there is at most one.
    #[must_use]
    pub fn zone_of(&self, channel: MidiChannel) -> Option<&Mpe11Zone> {
        self.zones().find(|z| z.layout().handles_channel(channel))
    }
}

/// The MIDI-CI MPE Profile zones of one Group: at least one, in order of
/// Manager Channel, no two sharing a channel (M2-120 section 3.4). A Manager
/// with at least one Member each fills the sixteen channels with at most
/// [`MAX`](Self::MAX) zones.
///
/// [`with`](Self::with) refuses a zone that shares a channel with one held.
/// The zones are kept in order of their Manager, so equal sets compare equal
/// whatever order their zones were added in. There is no empty set: no zones
/// is no `ProfileZones` ([`without`](Self::without)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "Vec<ProfileZone>", into = "Vec<ProfileZone>")
)]
pub struct ProfileZones {
    zones: [Option<ProfileZone>; Self::MAX],
}

impl From<ProfileZones> for Vec<ProfileZone> {
    fn from(zones: ProfileZones) -> Self {
        zones.zones().copied().collect()
    }
}

impl TryFrom<Vec<ProfileZone>> for ProfileZones {
    type Error = InvalidMpeZones;

    fn try_from(zones: Vec<ProfileZone>) -> Result<Self, Self::Error> {
        let mut zones = zones.into_iter();
        let first = zones.next().ok_or(InvalidMpeZones::Empty)?;
        zones
            .try_fold(Self::single(first), |set, zone| set.with(zone))
            .ok_or(InvalidMpeZones::Overlap)
    }
}

impl ProfileZones {
    /// The most zones: eight, every zone with two channels at least.
    pub const MAX: usize = MpeZoneSet::MAX;

    /// The set of `zone` alone.
    #[must_use]
    pub const fn single(zone: ProfileZone) -> Self {
        let mut zones = [None; Self::MAX];
        zones[0] = Some(zone);
        Self { zones }
    }

    /// The zones of `set`, each at `range`, a polyphonic response.
    #[must_use]
    pub fn new(set: &MpeZoneSet, range: PitchBendSensitivity) -> Self {
        // `set` holds zones that share no channel.
        Self::sorted(set.zones().map(|span| ProfileZone::new(span, range)))
    }

    /// The set with `zone` added, or `None` when it shares a channel with one
    /// of the zones held.
    #[must_use]
    pub fn with(self, zone: ProfileZone) -> Option<Self> {
        if self.zones().any(|held| held.span().overlaps(zone.span())) {
            return None;
        }
        Some(Self::sorted(self.zones().copied().chain([zone])))
    }

    /// The set without the zone whose Manager is `manager`, or `None` when that
    /// was the last. A `manager` that names no zone leaves the set as it is.
    #[must_use]
    pub fn without(self, manager: MidiChannel) -> Option<Self> {
        let mut rest = self
            .zones()
            .copied()
            .filter(|zone| zone.manager() != manager)
            .peekable();
        rest.peek()?;
        Some(Self::sorted(rest))
    }

    /// Builds the set of zones already known not to overlap, ordered by
    /// Manager.
    fn sorted(zones: impl Iterator<Item = ProfileZone>) -> Self {
        let mut held = [None; Self::MAX];
        let mut count = 0;
        for zone in zones {
            held[count] = Some(zone);
            count += 1;
        }
        held[..count].sort_by_key(|zone| zone.map(|z| z.manager().get()));
        Self { zones: held }
    }

    /// The zones, lowest Manager first.
    pub fn zones(&self) -> impl Iterator<Item = &ProfileZone> + Clone {
        self.zones.iter().flatten()
    }

    /// The zone that owns `channel` (as its Manager or one of its Members), if
    /// any. The zones share no channel, so there is at most one.
    #[must_use]
    pub fn zone_of(&self, channel: MidiChannel) -> Option<&ProfileZone> {
        self.zones().find(|z| z.span().contains(channel))
    }
}

/// How an instrument occupies the sixteen channels as a whole.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MpeMode {
    /// No MPE. Channels behave as ordinary MIDI 1.0 channels.
    #[default]
    Disabled,
    /// One or two MPE zones.
    Zones(Mpe11Zones),
    /// Single-channel **Note Number Rotation** (M2-104 Appendix C.3): all notes
    /// on the one channel, with no zones and no member-channel spreading.
    /// Rotation is a *sender* scheme and a receiver need not know it is used,
    /// so a receiver keeps the one channel's messages and ignores the rest.
    SingleChannelRotation {
        /// The one channel every note is sent on.
        channel: MidiChannel,
    },
}

#[cfg(test)]
mod tests;
