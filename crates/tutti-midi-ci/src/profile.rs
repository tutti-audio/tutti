//! The values of Profile Configuration (M2-101 §7, M2-102 §2): a Profile Id, the
//! number of Channels a Profile uses, the two lists of a Reply to Profile Inquiry,
//! and the Inquiry Target and data of the Details messages.
//!
//! The messages themselves are [`CiMessage`](super::CiMessage)'s variants; the
//! Initiator's state over them is [`ProfileInitiator`](super::ProfileInitiator).
//!
//! Each message's destination follows its table in M2-101 §7. The four reports
//! (Added, Removed, Enabled, Disabled) always go to the Broadcast MUID and carry
//! no `to`; the Inquiry, the Reply, Set On, Set Off and the two Details messages
//! go to one MUID; Profile Specific Data carries a [`Destination`],
//! since §7.12 allows the Broadcast MUID only where the Profile defines it. A
//! message of the wrong kind decodes to an [`Unread`](super::Unread) with
//! [`WrongDestination`](super::UnreadReason::WrongDestination). A Message Format
//! Version 1 sender has no Number of Channels field; it reads as 0, which §5.3's rule to
//! read only the fields of the received version asks for.
//!
//! [`ProfileId::from_bytes`] refuses what [`ProfileId::to_bytes`] could not give
//! back, a one-byte id followed by a non-zero byte (§7.3.1 fills the rest
//! with zeroes), and a manufacturer Profile cannot carry the id `7E`, which would
//! read back as a Standard Profile. [`ProfileList`] holds at most 16383 Profiles
//! per list, the most two 7-bit bytes can count, so a Reply it builds is always
//! writable.

use std::vec::Vec;

use tutti_ump::{ManufacturerId, MidiChannel, MidiGroup, U7, U14, U28, Ump, first_non_seven_bit};

use super::{
    CI_VERSION, CiDeviceId, CiHeader, CiVersion, Destination, Whole, encode_preamble, read,
    sysex7_packets,
};

/// Profile Inquiry, Sub-ID#2 `0x20` (§7.2).
pub const SUB_ID2_PROFILE_INQUIRY: u8 = 0x20;
/// Reply to Profile Inquiry (§7.3).
pub const SUB_ID2_PROFILE_REPLY: u8 = 0x21;
/// Set Profile On (§7.8).
pub const SUB_ID2_SET_PROFILE_ON: u8 = 0x22;
/// Set Profile Off (§7.9).
pub const SUB_ID2_SET_PROFILE_OFF: u8 = 0x23;
/// Profile Enabled Report (§7.10).
pub const SUB_ID2_PROFILE_ENABLED: u8 = 0x24;
/// Profile Disabled Report (§7.11).
pub const SUB_ID2_PROFILE_DISABLED: u8 = 0x25;
/// Profile Added Report (§7.4).
pub const SUB_ID2_PROFILE_ADDED: u8 = 0x26;
/// Profile Removed Report (§7.5).
pub const SUB_ID2_PROFILE_REMOVED: u8 = 0x27;
/// Profile Details Inquiry (§7.6).
pub const SUB_ID2_PROFILE_DETAILS_INQUIRY: u8 = 0x28;
/// Reply to Profile Details Inquiry (§7.7).
pub const SUB_ID2_PROFILE_DETAILS_REPLY: u8 = 0x29;
/// Profile Specific Data (§7.12).
pub const SUB_ID2_PROFILE_SPECIFIC_DATA: u8 = 0x2F;

/// The first Profile Id byte of a Standard Defined Profile (§7.3.1).
const STANDARD_BYTE: u8 = 0x7E;

/// A Standard Defined Profile's Id (M2-102 §2.2, Table 5): the Profile Number
/// Bank, Profile Number, Profile Version and Profile Level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StandardProfile {
    bank: U7,
    number: U7,
    version: U7,
    level: U7,
}

impl StandardProfile {
    /// The MPE Profile (M2-120 section 3.2, Table 4): bank `0x31`, number
    /// `0x00`, version `0x01`, level `0x01`.
    pub const MPE: Self = Self::new(U7::of(0x31), U7::of(0x00), U7::of(0x01), U7::of(0x01));

    /// The Note On Orchestral Articulation Profile (M2-123 section 3.2, Table
    /// 4): bank `0x21`, number `0x01`, version `0x01`, level `0x01`.
    pub const ARTICULATION: Self =
        Self::new(U7::of(0x21), U7::of(0x01), U7::of(0x01), U7::of(0x01));

    /// The Rotary Speaker Profile (M2-122 §3.1, Table 4): bank `0x22`, number
    /// `0x00`, version `0x01`, level `0x01`.
    pub const ROTARY_SPEAKER: Self =
        Self::new(U7::of(0x22), U7::of(0x00), U7::of(0x01), U7::of(0x01));

    /// The General MIDI 2 Function Block Profile (M2-118 §3.4, Table 4): bank
    /// `0x00`, number `0x00`, version `0x01`, level `0x01`.
    pub const GM2_FUNCTION_BLOCK: Self =
        Self::new(U7::of(0x00), U7::of(0x00), U7::of(0x01), U7::of(0x01));

    /// The General MIDI 2 Single Channel Profile (M2-119 §3.2, Table 4): bank
    /// `0x20`, number `0x02`, version `0x01`, level `0x01`.
    pub const GM2_SINGLE_CHANNEL: Self =
        Self::new(U7::of(0x20), U7::of(0x02), U7::of(0x01), U7::of(0x01));

    /// A Standard Defined Profile. Every combination of the four bytes is one;
    /// what each means is the Profile's specification's (byte 5, the level, is
    /// M2-102 §3.2's).
    #[must_use]
    pub const fn new(bank: U7, number: U7, version: U7, level: U7) -> Self {
        Self {
            bank,
            number,
            version,
            level,
        }
    }

    /// The Profile Number Bank (byte 2).
    #[must_use]
    pub const fn bank(self) -> U7 {
        self.bank
    }

    /// The Profile Number (byte 3).
    #[must_use]
    pub const fn number(self) -> U7 {
        self.number
    }

    /// The Profile Version (byte 4).
    #[must_use]
    pub const fn version(self) -> U7 {
        self.version
    }

    /// The Profile Level (byte 5).
    #[must_use]
    pub const fn level(self) -> U7 {
        self.level
    }
}

/// A manufacturer's own Profile (M2-101 §7.3.1, Table 19): its System Exclusive
/// id in bytes 1 to 3 and `Manufacturer Specific Info` in bytes 4 and 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManufacturerProfile {
    manufacturer: ManufacturerId,
    info: [U7; 2],
}

impl ManufacturerProfile {
    /// A manufacturer's Profile, or `None` for the id `7E`: a first byte of `0x7E`
    /// is a Standard Defined Profile, so a manufacturer with that id could not be
    /// read back.
    #[must_use]
    pub fn new(manufacturer: ManufacturerId, info: [U7; 2]) -> Option<Self> {
        (manufacturer != ManufacturerId::UNIVERSAL_NON_REAL_TIME)
            .then_some(Self { manufacturer, info })
    }

    /// The manufacturer's System Exclusive id.
    #[must_use]
    pub const fn manufacturer(self) -> ManufacturerId {
        self.manufacturer
    }

    /// The two bytes of `Manufacturer Specific Info`: up to 16384 different
    /// Manufacturer Profile numbers.
    #[must_use]
    pub const fn info(self) -> [U7; 2] {
        self.info
    }
}

/// A Profile Id, five bytes (M2-101 §7.3.1): a Standard Defined Profile (first
/// byte `0x7E`) or a manufacturer's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProfileId {
    /// Adopted by AMEI and MA.
    Standard(StandardProfile),
    /// A manufacturer's own.
    Manufacturer(ManufacturerProfile),
}

impl ProfileId {
    /// The MPE Profile ([`StandardProfile::MPE`]).
    pub const MPE: Self = Self::Standard(StandardProfile::MPE);

    /// The Note On Orchestral Articulation Profile
    /// ([`StandardProfile::ARTICULATION`]).
    pub const ARTICULATION: Self = Self::Standard(StandardProfile::ARTICULATION);

    /// The General MIDI 2 Function Block Profile
    /// ([`StandardProfile::GM2_FUNCTION_BLOCK`]).
    pub const GM2_FUNCTION_BLOCK: Self = Self::Standard(StandardProfile::GM2_FUNCTION_BLOCK);

    /// The General MIDI 2 Single Channel Profile
    /// ([`StandardProfile::GM2_SINGLE_CHANNEL`]).
    pub const GM2_SINGLE_CHANNEL: Self = Self::Standard(StandardProfile::GM2_SINGLE_CHANNEL);

    /// The Rotary Speaker Profile ([`StandardProfile::ROTARY_SPEAKER`]).
    pub const ROTARY_SPEAKER: Self = Self::Standard(StandardProfile::ROTARY_SPEAKER);

    /// The five wire bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 5] {
        match self {
            Self::Standard(p) => [
                STANDARD_BYTE,
                p.bank.get(),
                p.number.get(),
                p.version.get(),
                p.level.get(),
            ],
            Self::Manufacturer(p) => {
                let [a, b, c] = p.manufacturer.sysex_bytes().map(U7::get);
                [a, b, c, p.info[0].get(), p.info[1].get()]
            }
        }
    }

    /// The Profile Id of five wire bytes, or `None` for a byte with its top bit
    /// set, or a manufacturer id that is not one (a one-byte id is followed by
    /// two zeroes, §7.3.1).
    #[must_use]
    pub fn from_bytes(bytes: [u8; 5]) -> Option<Self> {
        let [a, b, c, d, e] = bytes.map(U7::new);
        let (a, b, c, d, e) = (a?, b?, c?, d?, e?);
        if a.get() == STANDARD_BYTE {
            return Some(Self::Standard(StandardProfile::new(b, c, d, e)));
        }
        let manufacturer = ManufacturerId::try_from([a, b, c]).ok()?;
        Some(Self::Manufacturer(ManufacturerProfile::new(
            manufacturer,
            [d, e],
        )?))
    }
}

tutti_units::ranged! {
    /// The number of Channels a Profile uses or is asked to use (§7.8, §7.10,
    /// §7.11): the Manager plus Member Channels, 14 bits.
    ///
    /// Zero is a value the spec gives a meaning: when the Profile Destination is
    /// address 0x7E or 0x7F, the number of Channels is the
    /// width of the Group or Function Block, and the field is then 0
    /// (`BY_ADDRESS`).
    #[derive(Default, Eq, PartialOrd, Ord, Hash)]
    ProfileChannels(u16), NotProfileChannels, "a 14-bit Channel count",
    |raw| raw <= 0x3FFF
}

impl ProfileChannels {
    /// `0x0000`: the width of the Group or Function Block addressed.
    pub(crate) const BY_ADDRESS: Self = Self::of(0);
    /// The most the field holds, 16383.
    pub const MAX: u16 = U14::MAX.get();

    /// Whether the field is 0, the Group or Function Block's own width.
    #[must_use]
    pub const fn is_by_address(self) -> bool {
        self.get() == 0
    }

    pub(crate) const fn from_wire(raw: U14) -> Self {
        Self::of(raw.get())
    }

    /// The two 7-bit wire bytes, least significant first.
    pub(crate) const fn to_le7(self) -> [u8; 2] {
        // The range holds 14 bits, so nothing is masked off.
        U14::masked(self.get()).to_le7()
    }
}

/// Whether a Profile is active (§7.3.2, §7.3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProfileState {
    /// Supported and currently enabled.
    Enabled,
    /// Supported but currently disabled.
    Disabled,
}

/// A list the wire cannot hold: a count is 14 bits (§7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{} Profile Ids do not fit a 14-bit count (at most {})", .len, ProfileList::MAX_LEN)]
pub struct ListTooLong {
    /// The length refused.
    pub len: usize,
}

/// A list of Profile Ids of at most `MAX_LEN` Ids, the largest count a Reply to
/// Profile Inquiry carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileList(Vec<ProfileId>);

impl ProfileList {
    /// The longest list: the count is two 7-bit bytes.
    pub const MAX_LEN: usize = 0x3FFF;

    /// The list of `ids`, or `None` when it is longer than `MAX_LEN`.
    #[must_use]
    pub fn new(ids: Vec<ProfileId>) -> Option<Self> {
        Self::try_from(ids).ok()
    }

    /// The Ids.
    #[must_use]
    pub fn as_slice(&self) -> &[ProfileId] {
        &self.0
    }
}

impl TryFrom<Vec<ProfileId>> for ProfileList {
    type Error = ListTooLong;

    fn try_from(ids: Vec<ProfileId>) -> Result<Self, ListTooLong> {
        if ids.len() > Self::MAX_LEN {
            return Err(ListTooLong { len: ids.len() });
        }
        Ok(Self(ids))
    }
}

/// The two lists of a Reply to Profile Inquiry (§7.3): the Profiles supported and
/// currently enabled, and those supported but currently disabled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileLists {
    enabled: ProfileList,
    disabled: ProfileList,
}

impl ProfileLists {
    /// The lists of the Profiles enabled and of those disabled.
    #[must_use]
    pub fn new(enabled: ProfileList, disabled: ProfileList) -> Self {
        Self { enabled, disabled }
    }

    /// Both lists empty: no Profiles reported (§7.13).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The Profiles currently enabled.
    #[must_use]
    pub fn enabled(&self) -> &[ProfileId] {
        self.enabled.as_slice()
    }

    /// The Profiles supported and currently disabled.
    #[must_use]
    pub fn disabled(&self) -> &[ProfileId] {
        self.disabled.as_slice()
    }

    pub(crate) fn encode_body(&self, out: &mut impl Extend<u8>) {
        for list in [&self.enabled, &self.disabled] {
            // A `ProfileList` holds its length to 14 bits.
            out.extend(U14::saturating_from_len(list.as_slice().len()).to_le7());
            for id in list.as_slice() {
                out.extend(id.to_bytes());
            }
        }
    }
}

/// The Inquiry Target of the Details messages (§7.6.1): `0x00` to `0x3F` is
/// Registered Target Data, `0x40` to `0x7F` Profile Specific Target Data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InquiryTarget(U7);

impl InquiryTarget {
    /// `Number of MIDI Channels`, the one Registered Target of M2-102 §2.5.1.
    pub const NUMBER_OF_CHANNELS: Self = Self(U7::of(0x00));

    /// A target: any 7-bit value is one.
    #[must_use]
    pub const fn new(raw: U7) -> Self {
        Self(raw)
    }

    /// The target byte.
    #[must_use]
    pub const fn get(self) -> U7 {
        self.0
    }

    /// Whether it is Registered Target Data (`0x00` to `0x3F`), whose format is
    /// the same for every Profile.
    #[must_use]
    pub const fn is_registered(self) -> bool {
        self.0.get() < 0x40
    }
}

/// Why bytes are not Profile data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ProfileDataError {
    /// A byte has its top bit set: System Exclusive carries 7-bit data.
    #[error("a data byte has its top bit set")]
    NotSevenBit,
    /// More bytes than the message's length field holds.
    #[error("more data than the length field holds")]
    TooLong,
}

macro_rules! seven_bit_data {
    ($(#[$doc:meta])* $name:ident, $len:ty) => {
        $(#[$doc])*
        #[derive(Clone, Debug, Default, PartialEq, Eq)]
        pub struct $name(Vec<u8>, $len);

        impl $name {
            /// The most bytes the length field holds.
            pub const MAX_LEN: usize = <$len>::MAX.get() as usize;

            /// The data, or why it cannot be carried.
            ///
            /// # Errors
            ///
            /// [`ProfileDataError`] for a byte with its top bit set, or more than
            /// [`MAX_LEN`](Self::MAX_LEN) bytes.
            pub fn new(bytes: Vec<u8>) -> Result<Self, ProfileDataError> {
                let len = <$len>::from_len(bytes.len()).ok_or(ProfileDataError::TooLong)?;
                if first_non_seven_bit(&bytes).is_some() {
                    return Err(ProfileDataError::NotSevenBit);
                }
                Ok(Self(bytes, len))
            }

            /// The bytes.
            #[must_use]
            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }

            /// The length as the wire field that carries it.
            pub(crate) fn wire_len(&self) -> $len {
                self.1
            }

            /// The bytes, by value.
            #[must_use]
            pub fn into_bytes(self) -> Vec<u8> {
                self.0
            }
        }
    };
}

seven_bit_data!(
    /// The Inquiry Target Data of a Reply to Profile Details Inquiry (§7.7.3): at
    /// most 16383 bytes (the length is 14 bits), 7-bit. Its format is the target's.
    DetailsData,
    U14
);

seven_bit_data!(
    /// The data of a Profile Specific Data message (§7.12): 7-bit bytes, at most
    /// 2^28 - 1 of them (the length is four 7-bit bytes). Their meaning is the
    /// Profile's.
    SpecificData,
    U28
);

/// A Profile Specific Data message (§7.12, Table 28): the addressing, the
/// Profile and the data the Profile's specification defines.
///
/// A message to the Broadcast MUID is at most 512 bytes, `F0` and `F7`
/// included, so the buffers of every receiver on the connection hold it
/// (§7.12; §3.3.3). The message has no chunking of its own (a Profile that
/// defines one cuts its data into several messages), so [`new`](Self::new)
/// refuses a larger one. A receiver reads the message whatever its size, as a
/// sender's mistake is not a reason to drop data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSpecific {
    header: CiHeader,
    to: Destination,
    profile: ProfileId,
    data: SpecificData,
}

impl ProfileSpecific {
    /// The most bytes of data a message to the Broadcast MUID carries: 512 less
    /// the 24 that are not data (`F0`, the 13-byte preamble, the Profile Id, the
    /// 4-byte length and `F7`).
    pub const BROADCAST_MAX_DATA: usize = 512 - 24;

    /// The message `header` and `profile` send `data` to `to`, or `None` for data
    /// over [`BROADCAST_MAX_DATA`](Self::BROADCAST_MAX_DATA) bytes to the
    /// Broadcast MUID.
    pub fn new(
        header: CiHeader,
        to: Destination,
        profile: ProfileId,
        data: SpecificData,
    ) -> Option<Self> {
        if to == Destination::Broadcast && data.as_bytes().len() > Self::BROADCAST_MAX_DATA {
            return None;
        }
        Some(Self::read(header, to, profile, data))
    }

    /// The message as it was received, whatever its size.
    pub(super) fn read(
        header: CiHeader,
        to: Destination,
        profile: ProfileId,
        data: SpecificData,
    ) -> Self {
        Self {
            header,
            to,
            profile,
            data,
        }
    }

    /// The addressing preamble; its Device ID is the address.
    #[must_use]
    pub fn header(&self) -> CiHeader {
        self.header
    }

    /// To one device, or to all.
    #[must_use]
    pub fn to(&self) -> Destination {
        self.to
    }

    /// The Profile.
    #[must_use]
    pub fn profile(&self) -> ProfileId {
        self.profile
    }

    /// The data.
    #[must_use]
    pub fn data(&self) -> &SpecificData {
        &self.data
    }
}

/// The Registered Target Data `Number of MIDI Channels` (M2-102 §2.5.1, Table 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelsDetail {
    /// The number of Channels currently in use by this Profile, 0 when it is
    /// not enabled.
    pub in_use: ProfileChannels,
    /// The maximum number of Channels available for use by this Profile.
    pub max: ProfileChannels,
}

impl ChannelsDetail {
    /// The data of a reply to [`InquiryTarget::NUMBER_OF_CHANNELS`]: length 4.
    #[must_use]
    pub fn to_data(self) -> DetailsData {
        let [a, b] = self.in_use.to_le7();
        let [c, d] = self.max.to_le7();
        DetailsData(vec![a, b, c, d], U14::of(4))
    }
}

impl DetailsData {
    /// The data read as [`ChannelsDetail`], or `None` when it is not four bytes.
    /// It means that only for a reply to [`InquiryTarget::NUMBER_OF_CHANNELS`].
    #[must_use]
    pub fn as_channels(&self) -> Option<ChannelsDetail> {
        let [a, b, c, d] = self.0[..] else {
            return None;
        };
        // The bytes are 7-bit: `DetailsData::new` refused any other.
        let count = |lsb: u8, msb: u8| {
            ProfileChannels::from_wire(U14::from_halves(U7::masked(msb), U7::masked(lsb)))
        };
        Some(ChannelsDetail {
            in_use: count(a, b),
            max: count(c, d),
        })
    }
}

/// A Profile Id and its Number of Channels as a Set Profile On, Profile Enabled
/// or Profile Disabled message carries them (§7.8, §7.10, §7.11).
pub(super) fn encode_with_count(
    profile: ProfileId,
    channels: ProfileChannels,
    out: &mut impl Extend<u8>,
) {
    out.extend(profile.to_bytes());
    out.extend(channels.to_le7());
}

/// Where a Set Profile On, Profile Enabled or Profile Disabled is aimed, and how
/// many Channels it names (§7.8, §7.10, §7.11).
///
/// For a Profile Destination of address 0x7E or 0x7F the number of Channels is
/// the width of the Group or Function Block, and the Number of Channels field
/// is 0x0000: so only a Channel has a count to give, and a Group or Function
/// Block has none to get wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProfileTarget {
    /// A Channel, the Manager Channel of a multi-Channel Profile, and the total
    /// number of Channels: requested by a Set Profile On (M2-102 §2.5.1: no
    /// more than the peer's declared maximum), enabled or disabled by a report.
    Channel {
        /// The Channel.
        channel: MidiChannel,
        /// The Manager plus Member Channels.
        channels: ProfileChannels,
    },
    /// The whole Group (`0x7E`).
    Group,
    /// The whole Function Block (`0x7F`).
    FunctionBlock,
}

impl From<Whole> for ProfileTarget {
    fn from(whole: Whole) -> Self {
        match whole {
            Whole::Group => Self::Group,
            Whole::FunctionBlock => Self::FunctionBlock,
        }
    }
}

impl ProfileTarget {
    /// The Device ID the message goes to.
    #[must_use]
    pub const fn address(self) -> CiDeviceId {
        match self {
            Self::Channel { channel, .. } => CiDeviceId::Channel(channel),
            Self::Group => CiDeviceId::Group,
            Self::FunctionBlock => CiDeviceId::FunctionBlock,
        }
    }

    /// The target at `address` with `channels` as the message carries them, or
    /// the count when a Group or Function Block address carries one other than 0.
    ///
    /// # Errors
    ///
    /// The count, for a Group or Function Block address.
    pub(crate) const fn from_wire(
        address: CiDeviceId,
        channels: ProfileChannels,
    ) -> Result<Self, u16> {
        match address {
            CiDeviceId::Channel(channel) => Ok(Self::Channel { channel, channels }),
            CiDeviceId::Group | CiDeviceId::FunctionBlock if !channels.is_by_address() => {
                Err(channels.get())
            }
            CiDeviceId::Group => Ok(Self::Group),
            CiDeviceId::FunctionBlock => Ok(Self::FunctionBlock),
        }
    }

    /// The Number of Channels field.
    #[must_use]
    pub const fn channels(self) -> ProfileChannels {
        match self {
            Self::Channel { channels, .. } => channels,
            Self::Group | Self::FunctionBlock => ProfileChannels::BY_ADDRESS,
        }
    }
}

/// A Set Profile On that names no peer (M2-116 section 6.2): its Source and
/// Destination MUIDs are both Broadcast, as a Clip File's header carries it.
///
/// [`CiMessage::SetProfileOn`](super::CiMessage::SetProfileOn) is the message on a link, between two MUIDs;
/// this is the same message at rest. The [`ProfileTarget`] holds the Device ID
/// and the Number of Channels Requested together as §7.8's note asks: a Group
/// or Function Block address asks for 0 Channels, so only a Channel carries a
/// count.
///
/// ```
/// use tutti_midi_ci::{ProfileChannels, ProfileId, ProfileOnRequest, ProfileTarget};
/// use tutti_ump::MidiChannel;
///
/// let on = ProfileOnRequest {
///     target: ProfileTarget::Channel {
///         channel: MidiChannel::FIRST,
///         channels: ProfileChannels::of(15),
///     },
///     profile: ProfileId::MPE,
/// };
/// assert_eq!(ProfileOnRequest::try_from(&on.encode()[..]), Ok(on));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProfileOnRequest {
    /// Where the Profile is enabled, and how many Channels are requested.
    pub target: ProfileTarget,
    /// The Profile to enable.
    pub profile: ProfileId,
}

/// Why bytes are not a [`ProfileOnRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum NotSetProfileOn {
    /// Not a MIDI-CI Set Profile On: the bytes are not `7E <device> 0D 22` and
    /// a preamble, or are too short for one.
    #[error("not a MIDI-CI Set Profile On")]
    NotThatMessage,
    /// The Message Format Version byte is 0 or has a reserved bit set.
    #[error("{0:#04x} is not a MIDI-CI Message Format Version")]
    Version(u8),
    /// The Device ID is a reserved value (`10` to `7D`).
    #[error("{0:#04x} is not a MIDI-CI Device ID (reserved)")]
    DeviceId(u8),
    /// The Profile Id or the Number of Channels Requested is not one (a byte
    /// with its top bit set, or a manufacturer id that is not one).
    #[error("the Profile Id or Number of Channels of a Set Profile On is not one")]
    Body,
    /// A Group or Function Block address with a Number of Channels other than 0
    /// (§7.8's note).
    #[error("a Set Profile On to the Group or Function Block asks for {0} Channels, not 0")]
    ChannelsForBlock(u16),
}

impl TryFrom<&[u8]> for ProfileOnRequest {
    type Error = NotSetProfileOn;

    /// Reads a CI byte payload. The MUIDs are not read: whatever they are, the
    /// message is a request for the Profile. A Message Format Version 1
    /// message, which has no Number of Channels field, asks for 0.
    ///
    /// # Errors
    ///
    /// [`NotSetProfileOn`], naming the first thing that is wrong.
    fn try_from(bytes: &[u8]) -> Result<Self, NotSetProfileOn> {
        let Some((preamble, body)) = read::preamble(bytes) else {
            return Err(NotSetProfileOn::NotThatMessage);
        };
        if preamble.sub_id2 != SUB_ID2_SET_PROFILE_ON {
            return Err(NotSetProfileOn::NotThatMessage);
        }
        CiVersion::new(preamble.version).ok_or(NotSetProfileOn::Version(preamble.version))?;
        let device_id = CiDeviceId::new(preamble.device_id)
            .ok_or(NotSetProfileOn::DeviceId(preamble.device_id))?;
        let (profile, channels) =
            read::profile_with_count_body(body).ok_or(NotSetProfileOn::Body)?;
        let target = ProfileTarget::from_wire(device_id, channels)
            .map_err(NotSetProfileOn::ChannelsForBlock)?;
        Ok(Self { target, profile })
    }
}

impl ProfileOnRequest {
    /// The CI byte payload, as [`CiMessage::encode`](super::CiMessage::encode)
    /// gives one: Message Format Version [`CI_VERSION`], both MUIDs Broadcast.
    #[must_use]
    pub fn encode(self) -> Vec<u8> {
        let mut out = Vec::new();
        encode_preamble(
            self.target.address(),
            SUB_ID2_SET_PROFILE_ON,
            CI_VERSION,
            Destination::Broadcast.to_bytes(),
            Destination::Broadcast,
            &mut out,
        );
        encode_with_count(self.profile, self.target.channels(), &mut out);
        out
    }

    /// The message as SysEx7 packets on `group` (M2-104 section 7.7).
    pub fn packets(self, group: MidiGroup) -> std::vec::IntoIter<Ump> {
        // Every field is a 7-bit value: the Profile Id and the count are
        // range-checked and the MUIDs are Broadcast.
        sysex7_packets(group, &self.encode())
            .unwrap_or_else(|_| unreachable!("a Set Profile On has only 7-bit fields"))
    }
}

#[cfg(test)]
mod tests;
