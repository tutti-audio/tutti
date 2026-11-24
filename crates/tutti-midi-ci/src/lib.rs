#![doc = include_str!("../README.md")]
//! The MIDI-CI message codec (M2-101): the header, the MUID, and Discovery,
//! Invalidate MUID, Inquiry: Endpoint, ACK and NAK, the Profile Configuration
//! messages, the Process Inquiry messages and the Property Exchange messages;
//! [`CiDevice`], the Management core over it; [`ProfileInitiator`], the
//! Initiator's side of Profile Configuration; [`ProcessInitiator`], that of
//! Process Inquiry; and [`PeInitiator`], that of Property Exchange.
//!
//! MIDI-CI lets two MIDI devices discover each other before either knows what the
//! other speaks. It is **not** a UMP message type: a CI message is a byte payload
//! carried inside a SysEx (sub-ID#1 `0x0D`), so it travels over MIDI 1.0 bytes
//! and over SysEx7 packets alike. The codec is over the payload bytes of a SysEx
//! message:
//!
//! ```text
//!   7E <device_id> 0D <sub_id2> <ci_ver> <src_muid:4> <dst_muid:4> <body...>
//! ```
//!
//! [`CiMessage::encode`] returns those bytes (without `F0` and `F7`); frame them
//! with `tutti_ump::frame` for a byte stream, or into SysEx7 packets, and hand
//! the bytes a `tutti_ump::SysexAssembler` completes to [`CiMessage::decode`].
//! A message the codec cannot read comes back as an [`Unread`] that keeps what
//! its header said, so the NAK that §5.3 and §5.11 require can be addressed.
//! The codec holds no state and no transport; the state machine over it is
//! [`CiDevice`], and nothing reads a clock: time is an argument.
//!
//! What a peer declares about itself becomes values the rest of tutti shares:
//! [`SuggestedCc`] is the table of the Default Control Change Mapping Profile, the
//! functions it suggests for each Control Change. The Default Drum Note Map
//! ([`DrumInstrument`]), Drawbar Organ ([`Drawbar`]) and Rotary Speaker
//! ([`RotaryControl`]) Profiles are the same kind of table, and [`Gm2FunctionBlock`]
//! and [`Gm2SingleChannel`] say whether a peer has General MIDI 2 enabled.
//!
#![forbid(unsafe_code)]

use std::vec::Vec;

use tutti_ump::{
    CiVersion, MidiChannel, MidiGroup, ProductInstanceId, Sysex7Packets, SysexError, U7, U28,
    UNIVERSAL_NON_REAL_TIME, Ump,
};

mod articulation;
mod curve;
mod deadline;
mod default_cc;
mod device;
mod discovery;
mod drawbar;
mod drum_map;
mod endpoint;
mod escape;
mod gm2;
mod handshake;
mod initiate;
mod initiator;
mod link;
mod pe;
mod process;
mod process_initiator;
mod profile;
mod profile_details;
mod read;
mod rotary;
mod status;
mod text;
mod two_state;
mod wait;

pub use articulation::{
    ArticulationDetails, ArticulationDetailsError, ArticulationFeatures, ArticulationLevel,
    CustomSounds, NotVariationCount, Sound, SoundSet, SubclassCounts, VariationCount,
};
pub use curve::{send_level, volume_gain};
pub use deadline::Deadline;
pub use default_cc::{CcFunction, CcPart, CcReset, CcValue, DefaultCcLevel, SuggestedCc};
pub use device::{CiAction, CiDevice, CiPeer, DISCOVERY_WINDOW};
pub use discovery::{
    CiCategories, DeviceBody, DiscoveryData, MaxSysexSize, NotASysexSize, ReplyData, ReplyVersion2,
    SUB_ID2_ACK, SUB_ID2_DISCOVERY, SUB_ID2_DISCOVERY_REPLY, SUB_ID2_ENDPOINT_INQUIRY,
    SUB_ID2_ENDPOINT_REPLY, SUB_ID2_INVALIDATE_MUID, SUB_ID2_NAK,
};
pub use drawbar::{
    Drawbar, DrawbarSetting, NotADrawbarSetting, ORGAN_HIGHEST_NOTE, ORGAN_LOWEST_NOTE,
    ORGAN_REFERENCE_NOTE, OrganFeatures, OrganLevel, OrganOption, OrganValue, VibratoChorus,
};
pub use drum_map::{
    BitmapPosition, DrumControl, DrumFeatures, DrumInstrument, DrumNoteMapLevel, DrumSounds,
    MutuallyExclusiveSet, NotABitmapPosition, NotAMutuallyExclusiveSet,
};
pub use endpoint::{EndpointInfo, EndpointStatus};
pub use gm2::{Gm2ChannelCount, Gm2FunctionBlock, Gm2Role, Gm2SingleChannel, NotAGm2ChannelCount};
pub use initiate::Initiator;
pub use initiator::{
    EnabledMpeZones, ProfileAction, ProfileChange, ProfileEntry, ProfileInitiator, ProfileOutcome,
    ProfileRequest, ProfileRequestError, ProfileTable,
};
#[cfg(feature = "serde")]
pub use pe::SavedStateError;
pub use pe::{
    ActionError, Arrival, BodyLimit, BufferLimit, CallError, CallId, CanSet, CcIndex, ChangeKind,
    ChunkNo, ChunkPosition, ChunkTotal, ClusterKind, ColumnSource, CommonState, ConfirmIcon,
    ConfirmType, Confirmation, ControllerIndexError, CtrlControl, CtrlKind, CtrlRange,
    CtrlValueKind, CustomResource, EntryError, ExtensionError, GetStateError, InvalidValue,
    JsonPointer, LinkRole, ListedTwice, MAX_LIST_ENTRIES, MediaType, ModeId, NotABodyLimit,
    NotABufferLimit, NotAChunkNo, NotAPeChannel, NotAPeMidiMode, NotAPriority, NotARequestId,
    NotASimultaneousRequests, NotAStateSize, NotAStepCount, NotAStreamLimit, NotSignificantBits,
    NotUnixSeconds, OpaqueBody, PE_JSON_MAX_DEPTH, PE_MAX_CHUNK_DATA_LEN, PE_MAX_HEADER_LEN,
    PE_TIMEOUT, PaginationFault, ParamPath, ParamPathError, PartialError, PartialUpdate,
    PartialValue, PeAction, PeAllCtrlEntry, PeAllCtrlList, PeAnswer, PeBankProgram,
    PeBasicChannelRx, PeBasicChannelTx, PeBodyError, PeBuffered, PeCacheScope, PeCacheTime, PeCall,
    PeCapabilities, PeCapabilitiesFailure, PeChCtrlEntry, PeChCtrlList, PeChange, PeChannel,
    PeChannelEntry, PeChannelList, PeChannelMode, PeChunk, PeChunkError, PeCiMessage, PeCluster,
    PeColumn, PeController, PeCtrl, PeCtrlMapEntry, PeCtrlMapList, PeCurrentMode, PeData,
    PeDataError, PeDataFault, PeDecimal, PeDeviceInfo, PeEncoding, PeEndedBy, PeError, PeExpired,
    PeExtensions, PeExternalSync, PeExtra, PeExtraError, PeFlowControl, PeGet, PeHeader,
    PeHeaderError, PeHeaderTooLong, PeHeld, PeInitiator, PeJson, PeJsonError, PeJsonSchema, PeKind,
    PeLink, PeLocalOn, PeMessage, PeMessageKind, PeMidiMode, PeModeEntry, PeModeList,
    PeNotification, PeObject, PeOpaque, PeOutcome, PePage, PeProgramEntry, PeProgramList,
    PeReassembler, PeReassemblyError, PeReassemblyLimits, PeReceived, PeRefused, PeReply,
    PeRequest, PeRequestId, PeResource, PeResourceList, PeResponse, PeSet, PeSetScope, PeSettings,
    PeSignal, PeState, PeStateEntry, PeStateList, PeStatus, PeStatusClass, PeStep, PeSubscribed,
    PeSubscription, PeTerminator, PeText, PeTextError, PeTransaction, PeUnreadable,
    PeUpdateCommand, PeVersion, Place, Priority, PropertyName, PropertyNameError, PublishError,
    RefusedAt, RequestRefused, ResIdUse, ResourceAction, ResourceDefaults, ResourceEntry,
    ResourceEntryBuilder, ResourceEvent, ResourceFailure, ResourceGet, ResourceId,
    ResourceInitiator, ResourceName, ResourceResponder, ResourceSet, ResourceStore, ResourceTarget,
    ResponderAction, ResponderEvent, ResponderSettings, RestoreError, RoleName,
    SUB_ID2_PE_CAPABILITIES_INQUIRY, SUB_ID2_PE_CAPABILITIES_REPLY, SUB_ID2_PE_NOTIFY, SavedState,
    SelectorAction, SetBody, SignificantBits, SimpleAction, SimultaneousRequests, StateDevice,
    StateId, StateRev, StateSize, StateStamp, StateVerdict, StepCount, StoreError, StreamLimit,
    SubscribeId, TypeHint, UnixSeconds, UnlistedText, UpdateKind, WrongDevice, decode_body,
    encode_body, mcoded7, split_message,
};
pub use process::{
    ChannelControllers, DataControl, NotADataControl, NoteData, ProcessFeatures, ReportScope,
    SUB_ID2_MESSAGE_REPORT_END, SUB_ID2_MESSAGE_REPORT_INQUIRY, SUB_ID2_MESSAGE_REPORT_REPLY,
    SUB_ID2_PROCESS_CAPABILITIES_INQUIRY, SUB_ID2_PROCESS_CAPABILITIES_REPLY, SystemMessages,
};
pub use process_initiator::{ProcessAction, ProcessError, ProcessInitiator, ReportRequest};
pub use profile::{
    ChannelsDetail, DetailsData, InquiryTarget, ListTooLong, ManufacturerProfile,
    NotProfileChannels, NotSetProfileOn, ProfileChannels, ProfileDataError, ProfileId, ProfileList,
    ProfileLists, ProfileOnRequest, ProfileSpecific, ProfileState, ProfileTarget,
    SUB_ID2_PROFILE_ADDED, SUB_ID2_PROFILE_DETAILS_INQUIRY, SUB_ID2_PROFILE_DETAILS_REPLY,
    SUB_ID2_PROFILE_DISABLED, SUB_ID2_PROFILE_ENABLED, SUB_ID2_PROFILE_INQUIRY,
    SUB_ID2_PROFILE_REMOVED, SUB_ID2_PROFILE_REPLY, SUB_ID2_PROFILE_SPECIFIC_DATA,
    SUB_ID2_SET_PROFILE_OFF, SUB_ID2_SET_PROFILE_ON, SpecificData, StandardProfile,
};
pub use profile_details::ProfileDetails;
pub use rotary::{RotaryControl, RotarySpeaker, RotaryValue};
pub use status::{Ack, AckStatus, Nak, NakStatus};
pub use text::MessageText;
pub use two_state::{SwitchMeaning, TwoState};
pub use wait::{NotATimeoutWait, PEER_REPLY_TIMEOUT, TimeoutWait};

/// Sub-ID#1 identifying a MIDI-CI message (M2-101 §5.2.1).
pub const CI_SUB_ID_1: u8 = 0x0D;

/// Whether the SysEx7 `payload` (the bytes after `F0`, or the data of the
/// first packet of a message) begins a Property Exchange message: Universal
/// Non-Real Time, any Device ID, Sub-ID#1 `0D`, and a Sub-ID#2 in the Property
/// Exchange range `30` to `3F` (M2-101 Appendix E). It reads only that
/// preamble, so it answers for the first packet of a divided message and for
/// a message of a Version this codec does not decode.
#[must_use]
pub fn is_property_exchange(payload: &[u8]) -> bool {
    matches!(
        payload,
        [
            UNIVERSAL_NON_REAL_TIME,
            _,
            CI_SUB_ID_1,
            SUB_ID2_PE_CAPABILITIES_INQUIRY..=SUB_ID2_PE_NOTIFY,
            ..
        ]
    )
}

/// Whether the SysEx7 `payload` begins a Set Profile On message: Universal
/// Non-Real Time, any Device ID, Sub-ID#1 `0D`, Sub-ID#2 `22` (M2-101 Appendix E).
/// Like [`is_property_exchange`] it reads only the preamble.
#[must_use]
pub fn is_set_profile_on(payload: &[u8]) -> bool {
    matches!(
        payload,
        [
            UNIVERSAL_NON_REAL_TIME,
            _,
            CI_SUB_ID_1,
            SUB_ID2_SET_PROFILE_ON,
            ..
        ]
    )
}

/// The MIDI-CI Message Format Version this codec speaks: **0x02** (MIDI-CI v1.2).
///
/// M2-101 §5.3 gives version 1.2 the number 0x02, and requires a device to
/// always use its own Message Format Version, so this may only be raised alongside the message
/// bodies, which carry every Version-2 field of the messages here: Discovery's
/// Output Path ID (Table 6), the reply's Output Path Instance ID and Function
/// Block (Table 8) and NAK's details, message length and text (Table 15).
///
/// Older peers still interoperate: §5.3 requires a receiver to process only the
/// fields, values and bits of the received version when it is lower
/// than its own, and the decoders default each absent Version-2 field rather
/// than rejecting the message.
///
/// Changing the version a device sends is not a free edit (§5.3): the device
/// must invalidate its MUID and start a new Discovery Transaction.
pub const CI_VERSION: CiVersion = CiVersion::of(0x02);

/// The Device ID byte of a message (M2-101 §3.2 and Table 5): which Channel,
/// Group or Function Block it is to or from.
///
/// `00` to `0F` address MIDI Channels 1 to 16, `7E` the Group and `7F` the
/// Function Block; `10` to `7D` are reserved, and a reserved value is not one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CiDeviceId {
    /// One MIDI Channel of the Group (`0x00..=0x0F`).
    Channel(MidiChannel),
    /// The whole Group, not channelized (`0x7E`).
    Group,
    /// The whole Function Block, not channelized (`0x7F`), the default of an
    /// inquiry and its reply.
    FunctionBlock,
}

impl CiDeviceId {
    /// The Device ID, or `None` for a reserved value (`0x10..=0x7D`) or one with
    /// the top bit set.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        match raw {
            0x7E => Some(Self::Group),
            0x7F => Some(Self::FunctionBlock),
            _ => match MidiChannel::new(raw) {
                Some(channel) => Some(Self::Channel(channel)),
                None => None,
            },
        }
    }

    /// The Device ID byte.
    #[must_use]
    pub const fn get(self) -> u8 {
        match self {
            Self::Channel(channel) => channel.get(),
            Self::Group => 0x7E,
            Self::FunctionBlock => 0x7F,
        }
    }
}

/// A Device ID that is no Channel: the whole Group or the whole Function Block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Whole {
    /// The Group, not channelized (`0x7E`).
    Group,
    /// The Function Block, not channelized (`0x7F`).
    FunctionBlock,
}

impl From<Whole> for CiDeviceId {
    fn from(whole: Whole) -> Self {
        match whole {
            Whole::Group => Self::Group,
            Whole::FunctionBlock => Self::FunctionBlock,
        }
    }
}

/// A raw value that is not a Device ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{:#04x} is not a MIDI-CI Device ID (reserved)", .0)]
pub struct NotADeviceId(pub u8);

impl TryFrom<u8> for CiDeviceId {
    type Error = NotADeviceId;

    fn try_from(raw: u8) -> Result<Self, NotADeviceId> {
        Self::new(raw).ok_or(NotADeviceId(raw))
    }
}

impl From<CiDeviceId> for u8 {
    fn from(id: CiDeviceId) -> u8 {
        id.get()
    }
}

tutti_units::ranged! {
    /// A 28-bit **MIDI Unique Identifier** (M2-101 §3.3): the random address a CI
    /// device gives itself. Four 7-bit bytes, least significant first, on the wire.
    ///
    /// It holds a MUID a device can *own*: the reserved range `0x0FFF_FF00` to
    /// `0x0FFF_FFFE` and the broadcast
    /// value `0x0FFF_FFFF` are refused. The broadcast
    /// address is a [`Destination`], not a `Muid`. A MUID is drawn from an OS random
    /// source by whoever owns the device; this type only checks one.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    Muid(u32), NotAMuid, "a MUID (past 28 bits, reserved, or broadcast)",
    |raw| raw < 0x0FFF_FF00
}

impl Muid {
    /// The broadcast value, `0x0FFF_FFFF` (M2-101 §3.3.3).
    pub(crate) const BROADCAST_RAW: u32 = 0x0FFF_FFFF;

    /// A MUID from a caller's random word: its low 28 bits, or `None` when
    /// they fall in the reserved range or on the broadcast value, so the caller
    /// draws again. No random source is named here: a MUID should come from a
    /// good one (M2-101 Appendix A.1), and whose is the owner's.
    #[must_use]
    pub const fn from_entropy(word: u32) -> Option<Self> {
        Self::new(word & Self::BROADCAST_RAW)
    }

    /// The four little-endian 7-bit wire bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 4] {
        // The range holds 28 bits, so nothing is masked off.
        U28::masked(self.get()).to_le7()
    }

    /// The MUID of four little-endian 7-bit wire bytes, or `None` for a byte
    /// with its top bit set, a reserved value or the broadcast value.
    #[must_use]
    pub fn from_bytes(b: [u8; 4]) -> Option<Self> {
        Self::new(U28::from_le7(b)?.get())
    }
}

/// Who a CI message is for: one device, or all of them (M2-101 §3.3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Destination {
    /// The broadcast MUID, `0x0FFF_FFFF`.
    Broadcast,
    /// One device.
    Muid(Muid),
}

impl Destination {
    fn to_bytes(self) -> [u8; 4] {
        match self {
            Destination::Broadcast => U28::of(Muid::BROADCAST_RAW).to_le7(),
            Destination::Muid(muid) => muid.to_bytes(),
        }
    }

    fn from_bytes(b: [u8; 4]) -> Option<Self> {
        let raw = U28::from_le7(b)?.get();
        if raw == Muid::BROADCAST_RAW {
            Some(Destination::Broadcast)
        } else {
            Muid::new(raw).map(Destination::Muid)
        }
    }
}

/// The fixed MIDI-CI preamble shared by every message (M2-101 §3.3 and §5.2.1):
/// the Device ID, the CI version and the sender's MUID. The sub-ID#2 (message
/// type) is **not** stored here: the [`CiMessage`] variant determines it, and so
/// it determines the destination MUID too, which is why neither is a field.
///
/// M2-101 §3.3.3 limits the Broadcast MUID to Discovery, Invalidate MUID,
/// the Profile Added, Removed, Enabled and Disabled Reports, and Profile
/// Specific Data. Discovery and Invalidate MUID are defined with the Broadcast
/// MUID as destination (Tables 6 and 12); Discovery Reply, ACK and NAK are not
/// on the list, so they go to one device. A [`CiMessage`] holds exactly that
/// and no other: a variant that is broadcast has no destination field, and one
/// that is not has a [`Muid`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CiHeader {
    /// The Device ID: a Channel, the Group, or the Function Block.
    pub device_id: CiDeviceId,
    /// CI message version (normally [`CI_VERSION`]).
    pub ci_version: CiVersion,
    /// MUID of the sender.
    pub source: Muid,
}

impl CiHeader {
    /// A header from `source`, to the whole Function Block, at [`CI_VERSION`]:
    /// the default of an inquiry and its reply (§5.2.1).
    #[must_use]
    pub const fn new(source: Muid) -> Self {
        Self {
            device_id: CiDeviceId::FunctionBlock,
            ci_version: CI_VERSION,
            source,
        }
    }

    /// Append the preamble bytes to `out`, stamping the given `sub_id2` and
    /// `destination`.
    fn encode(&self, sub_id2: u8, destination: Destination, out: &mut impl Extend<u8>) {
        encode_preamble(
            self.device_id,
            sub_id2,
            self.ci_version,
            self.source.to_bytes(),
            destination,
            out,
        );
    }
}

/// `payload` as SysEx7 packets on `group`, or the first byte that is not 7-bit.
/// Every byte a CI message encodes is 7-bit (the fields are `U7`, the MUIDs
/// and sizes are cut to 7-bit bytes and a text is ASCII), so a refusal is a
/// codec fault, and it is returned rather than sent short.
fn sysex7_packets(group: MidiGroup, payload: &[u8]) -> Result<std::vec::IntoIter<Ump>, SysexError> {
    Ok(Sysex7Packets::from_payload(group, payload)?
        .collect::<Vec<_>>()
        .into_iter())
}

/// Appends the preamble (§5.2.1): `7E device_id 0D sub_id2 version`, the Source
/// MUID's four bytes and the Destination MUID.
fn encode_preamble(
    device_id: CiDeviceId,
    sub_id2: u8,
    version: CiVersion,
    source: [u8; 4],
    destination: Destination,
    out: &mut impl Extend<u8>,
) {
    out.extend([
        UNIVERSAL_NON_REAL_TIME,
        device_id.get(),
        CI_SUB_ID_1,
        sub_id2,
        version.get(),
    ]);
    out.extend(source);
    out.extend(destination.to_bytes());
}

/// A decoded MIDI-CI message: its [`CiHeader`] plus a typed body. Non-exhaustive:
/// a message kind this codec does not model decodes to an [`Unread`] with
/// [`UnreadReason::UnsupportedMessage`], and one may be added.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CiMessage {
    /// Discovery, the opening handshake advertising identity and capabilities
    /// (M2-101 §5.5). Always to the Broadcast MUID.
    Discovery {
        /// Addressing preamble.
        header: CiHeader,
        /// Advertised identity and capabilities, and the initiator's Output Path ID.
        data: DiscoveryData,
    },
    /// Reply to Discovery (M2-101 §5.6): the same device body, to the initiator.
    DiscoveryReply {
        /// Addressing preamble.
        header: CiHeader,
        /// The initiator, whose MUID the Discovery came from.
        to: Muid,
        /// Advertised identity and capabilities, the echoed Output Path ID and
        /// the Function Block.
        data: ReplyData,
    },
    /// Invalidate MUID: tells a device its MUID collided and it must pick a new
    /// one (M2-101 §5.9). Always to the Broadcast MUID. Carries the target MUID.
    InvalidateMuid {
        /// Addressing preamble.
        header: CiHeader,
        /// The MUID being invalidated: the colliding one, not the sender's.
        target: Muid,
    },
    /// Inquiry: Endpoint (M2-101 §5.7): asks the UMP Endpoint behind a Function
    /// Block for one piece of information. To one device, never broadcast.
    EndpointInquiry {
        /// Addressing preamble.
        header: CiHeader,
        /// The device asked.
        to: Muid,
        /// What is asked.
        status: EndpointStatus,
    },
    /// Reply to Endpoint (M2-101 §5.8): the Status and the data it names.
    EndpointReply {
        /// Addressing preamble.
        header: CiHeader,
        /// The device that asked.
        to: Muid,
        /// The answer.
        info: EndpointInfo,
    },
    /// ACK: a positive answer or a notice of activity under way (M2-101 §5.10).
    Ack {
        /// Addressing preamble.
        header: CiHeader,
        /// The device being answered.
        to: Muid,
        /// What it says.
        ack: Ack,
    },
    /// NAK: a device rejects a CI message it cannot handle (M2-101 §5.11).
    Nak {
        /// Addressing preamble.
        header: CiHeader,
        /// The device being answered.
        to: Muid,
        /// Which message was rejected, and why.
        nak: Nak,
    },
    /// Inquiry: Process Inquiry Capabilities (§9.2): asks which Process Inquiry
    /// features a device supports. To one device, on the Function Block: its Device
    /// ID is `0x7F`.
    ProcessCapabilitiesInquiry {
        /// MUID of the sender.
        source: Muid,
        /// CI message version.
        ci_version: CiVersion,
        /// The device asked.
        to: Muid,
    },
    /// Reply to Process Inquiry Capabilities (§9.3), on the Function Block.
    ProcessCapabilitiesReply {
        /// MUID of the sender.
        source: Muid,
        /// CI message version.
        ci_version: CiVersion,
        /// The device that asked.
        to: Muid,
        /// The features supported.
        features: ProcessFeatures,
    },
    /// Inquiry: MIDI Message Report (§9.5): asks a device to report the state of
    /// its parameters as MIDI messages. Its Device ID is the scope's address.
    MessageReportInquiry {
        /// MUID of the sender.
        source: Muid,
        /// CI message version.
        ci_version: CiVersion,
        /// The device asked.
        to: Muid,
        /// How much to report.
        control: DataControl,
        /// Where, and which kinds of message.
        scope: ReportScope,
    },
    /// Reply to MIDI Message Report (§9.6): begins the report, declaring the kinds
    /// of message the Responder is about to send. Its Device ID is the scope's
    /// address, which is the Inquiry's.
    MessageReportReply {
        /// MUID of the sender.
        source: Muid,
        /// CI message version.
        ci_version: CiVersion,
        /// The device that asked.
        to: Muid,
        /// What the Responder will report.
        scope: ReportScope,
    },
    /// End of MIDI Message Report (§9.8): the report is complete. Its Device ID, in
    /// the header, is the Inquiry's.
    MessageReportEnd {
        /// Addressing preamble.
        header: CiHeader,
        /// The device that asked.
        to: Muid,
    },
    /// Profile Inquiry (M2-101 §7.2): asks which Profiles a device supports at
    /// the address in the header. To one device.
    ProfileInquiry {
        /// Addressing preamble; its Device ID is the address asked.
        header: CiHeader,
        /// The device asked.
        to: Muid,
    },
    /// Reply to Profile Inquiry (§7.3): the Profiles enabled and disabled at the
    /// address in the header.
    ProfileReply {
        /// Addressing preamble; its Device ID is the address described.
        header: CiHeader,
        /// The device that asked.
        to: Muid,
        /// The two lists.
        profiles: ProfileLists,
    },
    /// Set Profile On (§7.8): enables a Profile at the address of the target. The
    /// preamble's Device ID and `Number Channels Requested` are the target's,
    /// so a Group or Function Block address with a count is not writable.
    SetProfileOn {
        /// MUID of the sender.
        source: Muid,
        /// CI message version.
        ci_version: CiVersion,
        /// The device asked.
        to: Muid,
        /// The address and the Channels requested there. A Version 1 sender has
        /// no count field and it reads as 0.
        target: ProfileTarget,
        /// The Profile to enable.
        profile: ProfileId,
    },
    /// Set Profile Off (§7.9): disables a Profile at the address in the header.
    SetProfileOff {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The device asked.
        to: Muid,
        /// The Profile to disable.
        profile: ProfileId,
    },
    /// Profile Enabled Report (§7.10): a Profile is now enabled. Always sent to the
    /// Broadcast MUID.
    ProfileEnabled {
        /// MUID of the sender.
        source: Muid,
        /// CI message version.
        ci_version: CiVersion,
        /// The address and `Number Channels enabled` there: 0 for a Group or
        /// Function Block address, and for a Version 1 sender, which has no
        /// such field.
        target: ProfileTarget,
        /// The Profile.
        profile: ProfileId,
    },
    /// Profile Disabled Report (§7.11): a Profile is now disabled. Always sent to the
    /// Broadcast MUID.
    ProfileDisabled {
        /// MUID of the sender.
        source: Muid,
        /// CI message version.
        ci_version: CiVersion,
        /// The address and `Number Channels disabled` there: as
        /// [`ProfileEnabled`](Self::ProfileEnabled).
        target: ProfileTarget,
        /// The Profile.
        profile: ProfileId,
    },
    /// Profile Added Report (§7.4): a Profile is newly supported. To the
    /// Broadcast MUID.
    ProfileAdded {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The Profile.
        profile: ProfileId,
    },
    /// Profile Removed Report (§7.5): a Profile is no longer supported. To the
    /// Broadcast MUID.
    ProfileRemoved {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The Profile.
        profile: ProfileId,
    },
    /// Profile Details Inquiry (§7.6): asks for a detail of a Profile's
    /// implementation.
    ProfileDetailsInquiry {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The device asked.
        to: Muid,
        /// The Profile.
        profile: ProfileId,
        /// What is asked.
        target: InquiryTarget,
    },
    /// Reply to Profile Details Inquiry (§7.7).
    ProfileDetailsReply {
        /// Addressing preamble; its Device ID is the address.
        header: CiHeader,
        /// The device that asked.
        to: Muid,
        /// The Profile.
        profile: ProfileId,
        /// The target answered, as it was in the Profile Details Inquiry.
        target: InquiryTarget,
        /// The data of that target.
        data: DetailsData,
    },
    /// A Property Exchange message (M2-101 §8): the Capabilities pair, or a chunk
    /// of Get, Set or Subscription or of its reply. To one device.
    PropertyExchange {
        /// Addressing preamble; its Device ID is the Function Block (§8.2).
        header: CiHeader,
        /// The device it is for.
        to: Muid,
        /// The message.
        message: PeCiMessage,
    },
    /// Profile Specific Data (§7.12): data the Profile's specification defines.
    /// To one device or, when the Profile says so, to the Broadcast MUID.
    ProfileSpecificData(ProfileSpecific),
}

impl CiMessage {
    /// The message's preamble.
    #[must_use]
    pub fn header(&self) -> CiHeader {
        match self {
            CiMessage::SetProfileOn {
                source,
                ci_version,
                target,
                ..
            }
            | CiMessage::ProfileEnabled {
                source,
                ci_version,
                target,
                ..
            }
            | CiMessage::ProfileDisabled {
                source,
                ci_version,
                target,
                ..
            } => CiHeader {
                device_id: target.address(),
                ci_version: *ci_version,
                source: *source,
            },
            CiMessage::ProcessCapabilitiesInquiry {
                source, ci_version, ..
            }
            | CiMessage::ProcessCapabilitiesReply {
                source, ci_version, ..
            } => CiHeader {
                device_id: CiDeviceId::FunctionBlock,
                ci_version: *ci_version,
                source: *source,
            },
            CiMessage::MessageReportInquiry {
                source,
                ci_version,
                scope,
                ..
            }
            | CiMessage::MessageReportReply {
                source,
                ci_version,
                scope,
                ..
            } => CiHeader {
                device_id: scope.address(),
                ci_version: *ci_version,
                source: *source,
            },
            CiMessage::Discovery { header, .. }
            | CiMessage::DiscoveryReply { header, .. }
            | CiMessage::InvalidateMuid { header, .. }
            | CiMessage::EndpointInquiry { header, .. }
            | CiMessage::EndpointReply { header, .. }
            | CiMessage::Ack { header, .. }
            | CiMessage::Nak { header, .. }
            | CiMessage::MessageReportEnd { header, .. }
            | CiMessage::ProfileInquiry { header, .. }
            | CiMessage::ProfileReply { header, .. }
            | CiMessage::SetProfileOff { header, .. }
            | CiMessage::ProfileAdded { header, .. }
            | CiMessage::ProfileRemoved { header, .. }
            | CiMessage::ProfileDetailsInquiry { header, .. }
            | CiMessage::ProfileDetailsReply { header, .. }
            | CiMessage::PropertyExchange { header, .. } => *header,
            CiMessage::ProfileSpecificData(specific) => specific.header(),
        }
    }

    /// The Destination MUID: the Broadcast MUID for Discovery, Invalidate MUID and
    /// the Profile reports, the one device for the rest (Profile Specific Data
    /// either).
    #[must_use]
    pub fn destination(&self) -> Destination {
        match self {
            CiMessage::Discovery { .. }
            | CiMessage::InvalidateMuid { .. }
            | CiMessage::ProfileEnabled { .. }
            | CiMessage::ProfileDisabled { .. }
            | CiMessage::ProfileAdded { .. }
            | CiMessage::ProfileRemoved { .. } => Destination::Broadcast,
            CiMessage::ProfileSpecificData(specific) => specific.to(),
            CiMessage::DiscoveryReply { to, .. }
            | CiMessage::EndpointInquiry { to, .. }
            | CiMessage::EndpointReply { to, .. }
            | CiMessage::Ack { to, .. }
            | CiMessage::Nak { to, .. }
            | CiMessage::ProcessCapabilitiesInquiry { to, .. }
            | CiMessage::ProcessCapabilitiesReply { to, .. }
            | CiMessage::MessageReportInquiry { to, .. }
            | CiMessage::MessageReportReply { to, .. }
            | CiMessage::MessageReportEnd { to, .. }
            | CiMessage::ProfileInquiry { to, .. }
            | CiMessage::ProfileReply { to, .. }
            | CiMessage::SetProfileOn { to, .. }
            | CiMessage::SetProfileOff { to, .. }
            | CiMessage::ProfileDetailsInquiry { to, .. }
            | CiMessage::ProfileDetailsReply { to, .. }
            | CiMessage::PropertyExchange { to, .. } => Destination::Muid(*to),
        }
    }

    /// The message's sub-ID#2.
    #[must_use]
    pub fn sub_id2(&self) -> u8 {
        match self {
            CiMessage::Discovery { .. } => SUB_ID2_DISCOVERY,
            CiMessage::DiscoveryReply { .. } => SUB_ID2_DISCOVERY_REPLY,
            CiMessage::InvalidateMuid { .. } => SUB_ID2_INVALIDATE_MUID,
            CiMessage::EndpointInquiry { .. } => SUB_ID2_ENDPOINT_INQUIRY,
            CiMessage::EndpointReply { .. } => SUB_ID2_ENDPOINT_REPLY,
            CiMessage::Ack { .. } => SUB_ID2_ACK,
            CiMessage::Nak { .. } => SUB_ID2_NAK,
            CiMessage::ProcessCapabilitiesInquiry { .. } => SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
            CiMessage::ProcessCapabilitiesReply { .. } => SUB_ID2_PROCESS_CAPABILITIES_REPLY,
            CiMessage::MessageReportInquiry { .. } => SUB_ID2_MESSAGE_REPORT_INQUIRY,
            CiMessage::MessageReportReply { .. } => SUB_ID2_MESSAGE_REPORT_REPLY,
            CiMessage::MessageReportEnd { .. } => SUB_ID2_MESSAGE_REPORT_END,
            CiMessage::ProfileInquiry { .. } => SUB_ID2_PROFILE_INQUIRY,
            CiMessage::ProfileReply { .. } => SUB_ID2_PROFILE_REPLY,
            CiMessage::SetProfileOn { .. } => SUB_ID2_SET_PROFILE_ON,
            CiMessage::SetProfileOff { .. } => SUB_ID2_SET_PROFILE_OFF,
            CiMessage::ProfileEnabled { .. } => SUB_ID2_PROFILE_ENABLED,
            CiMessage::ProfileDisabled { .. } => SUB_ID2_PROFILE_DISABLED,
            CiMessage::ProfileAdded { .. } => SUB_ID2_PROFILE_ADDED,
            CiMessage::ProfileRemoved { .. } => SUB_ID2_PROFILE_REMOVED,
            CiMessage::ProfileDetailsInquiry { .. } => SUB_ID2_PROFILE_DETAILS_INQUIRY,
            CiMessage::ProfileDetailsReply { .. } => SUB_ID2_PROFILE_DETAILS_REPLY,
            CiMessage::PropertyExchange { message, .. } => message.sub_id2(),
            CiMessage::ProfileSpecificData { .. } => SUB_ID2_PROFILE_SPECIFIC_DATA,
        }
    }

    /// The CI byte payload: the bytes carried *inside* a SysEx, starting at
    /// `0x7E`, without the `0xF0` and `0xF7` delimiters.
    ///
    /// [`encode_into`](Self::encode_into) writes them into a buffer of the
    /// caller's, and [`packets`](Self::packets) cuts them into UMP packets.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Appends the CI byte payload (what [`encode`](Self::encode) returns) to
    /// `out`: a `Vec<u8>`, a `VecDeque`, any collection of bytes.
    pub fn encode_into(&self, out: &mut impl Extend<u8>) {
        self.header()
            .encode(self.sub_id2(), self.destination(), out);
        match self {
            CiMessage::Discovery { data, .. } => data.encode_body(out),
            CiMessage::DiscoveryReply { data, .. } => data.encode_body(out),
            CiMessage::InvalidateMuid { target, .. } => out.extend(target.to_bytes()),
            CiMessage::EndpointInquiry { status, .. } => out.extend([status.get().get()]),
            CiMessage::EndpointReply { info, .. } => info.encode_body(out),
            CiMessage::Ack { ack, .. } => ack.encode_body(out),
            CiMessage::Nak { nak, .. } => nak.encode_body(out),
            CiMessage::ProcessCapabilitiesInquiry { .. }
            | CiMessage::MessageReportEnd { .. }
            | CiMessage::ProfileInquiry { .. } => {}
            CiMessage::ProcessCapabilitiesReply { features, .. } => out.extend([features.bits()]),
            CiMessage::MessageReportInquiry { control, scope, .. } => {
                out.extend([control.get()]);
                out.extend(scope.bitmap_bytes());
            }
            CiMessage::MessageReportReply { scope, .. } => out.extend(scope.bitmap_bytes()),
            CiMessage::ProfileReply { profiles, .. } => profiles.encode_body(out),
            CiMessage::SetProfileOn {
                profile, target, ..
            } => profile::encode_with_count(*profile, target.channels(), out),
            // Two reserved bytes (Table 25).
            CiMessage::SetProfileOff { profile, .. } => {
                out.extend(profile.to_bytes());
                out.extend([0, 0]);
            }
            CiMessage::ProfileEnabled {
                profile, target, ..
            }
            | CiMessage::ProfileDisabled {
                profile, target, ..
            } => profile::encode_with_count(*profile, target.channels(), out),
            CiMessage::ProfileAdded { profile, .. } | CiMessage::ProfileRemoved { profile, .. } => {
                out.extend(profile.to_bytes());
            }
            CiMessage::ProfileDetailsInquiry {
                profile, target, ..
            } => {
                out.extend(profile.to_bytes());
                out.extend([target.get().get()]);
            }
            CiMessage::ProfileDetailsReply {
                profile,
                target,
                data,
                ..
            } => {
                out.extend(profile.to_bytes());
                out.extend([target.get().get()]);
                // Table 23: 14-bit length, least significant byte first.
                out.extend(data.wire_len().to_le7());
                out.extend(data.as_bytes().iter().copied());
            }
            CiMessage::PropertyExchange { message, .. } => message.encode_body(out),
            CiMessage::ProfileSpecificData(specific) => {
                let (profile, data) = (specific.profile(), specific.data());
                out.extend(profile.to_bytes());
                // Table 28: a 4-byte length of the Profile Specific Data that follows, LSB first.
                out.extend(data.wire_len().to_le7());
                out.extend(data.as_bytes().iter().copied());
            }
        }
    }

    /// The message as SysEx7 packets on `group` (M2-104 section 7.7), ready to
    /// send on a UMP stream: the payload of [`encode`](Self::encode) cut into a
    /// `Complete`, or a `Start`, `Continue`s and an `End`. It allocates once,
    /// for the payload; a MIDI 1.0 byte stream frames
    /// [`encode`](Self::encode)'s bytes with `tutti_ump::frame` instead.
    ///
    /// # Errors
    ///
    /// [`SysexError::NotSevenBit`] when a byte of the encoding is 128 or more,
    /// which no message of this type encodes: a message is never sent short.
    pub fn packets(&self, group: MidiGroup) -> Result<std::vec::IntoIter<Ump>, SysexError> {
        sysex7_packets(group, &self.encode())
    }

    /// Decodes a CI byte payload (as [`encode`](Self::encode) produces, or a
    /// `tutti_ump::SysexAssembler` completed) into a typed message, or into
    /// what could be read of one that is not ([`Decoded`]).
    pub fn decode(bytes: &[u8]) -> Decoded {
        // Not a CI message at all, or too short to hold the preamble: there is
        // no header to keep, and no sender a NAK could be addressed to.
        let Some((preamble, body)) = read::preamble(bytes) else {
            return Decoded::NotCi;
        };
        let read::Preamble {
            device_id,
            sub_id2,
            version,
            source,
            destination,
        } = preamble;
        let unread = |reason| {
            Decoded::Unread(Unread {
                device_id,
                sub_id2,
                version,
                source,
                destination,
                reason,
            })
        };

        // The header first, in the order a reply would have to take it: §5.3's
        // reserved version bits, §5.4's version 0, Table 5's reserved Device IDs.
        let Some(ci_version) = CiVersion::new(version) else {
            return unread(if version == 0 {
                UnreadReason::VersionZero
            } else {
                UnreadReason::ReservedVersionBits
            });
        };
        let Some(device_id) = CiDeviceId::new(device_id) else {
            return unread(UnreadReason::ReservedDeviceId);
        };
        let Some(source) = source else {
            return unread(UnreadReason::BadSource);
        };
        let Some(destination) = destination else {
            return unread(UnreadReason::BadDestination);
        };
        let header = CiHeader {
            device_id,
            ci_version,
            source,
        };

        // The destination each message is defined with (§3.3.3): `Some(to)` is
        // the one device, `None` the Broadcast MUID.
        let unicast = match destination {
            Destination::Muid(muid) => Some(muid),
            Destination::Broadcast => None,
        };
        let message = match (sub_id2, unicast) {
            (SUB_ID2_DISCOVERY, None) => {
                DiscoveryData::decode_body(body).map(|data| CiMessage::Discovery { header, data })
            }
            (SUB_ID2_INVALIDATE_MUID, None) => {
                read::target_body(body).map(|target| CiMessage::InvalidateMuid { header, target })
            }
            (SUB_ID2_DISCOVERY_REPLY, Some(to)) => ReplyData::decode_body(body)
                .map(|data| CiMessage::DiscoveryReply { header, to, data }),
            (SUB_ID2_ENDPOINT_INQUIRY, Some(to)) => match read::endpoint_inquiry_body(body) {
                None => None,
                Some(raw) => match EndpointStatus::new(raw) {
                    Some(status) => Some(CiMessage::EndpointInquiry { header, to, status }),
                    None => return unread(UnreadReason::ReservedStatus),
                },
            },
            (SUB_ID2_ENDPOINT_REPLY, Some(to)) => match read::endpoint_reply_body(body) {
                None => None,
                Some((raw, data)) => match EndpointStatus::new(raw) {
                    Some(EndpointStatus::ProductInstanceId) => ProductInstanceId::from_bytes(data)
                        .map(|id| CiMessage::EndpointReply {
                            header,
                            to,
                            info: EndpointInfo::ProductInstanceId(id),
                        }),
                    None => return unread(UnreadReason::ReservedStatus),
                },
            },
            (SUB_ID2_PROCESS_CAPABILITIES_INQUIRY, Some(to)) => {
                if header.device_id != CiDeviceId::FunctionBlock {
                    return unread(UnreadReason::Malformed);
                }
                Some(CiMessage::ProcessCapabilitiesInquiry {
                    source: header.source,
                    ci_version: header.ci_version,
                    to,
                })
            }
            (SUB_ID2_PROCESS_CAPABILITIES_REPLY, Some(to)) => {
                if header.device_id != CiDeviceId::FunctionBlock {
                    return unread(UnreadReason::Malformed);
                }
                read::features_body(body).map(|features| CiMessage::ProcessCapabilitiesReply {
                    source: header.source,
                    ci_version: header.ci_version,
                    to,
                    features,
                })
            }
            (SUB_ID2_MESSAGE_REPORT_INQUIRY, Some(to)) => {
                read::report_inquiry_body(header.device_id, body).map(|(control, scope)| {
                    CiMessage::MessageReportInquiry {
                        source: header.source,
                        ci_version: header.ci_version,
                        to,
                        control,
                        scope,
                    }
                })
            }
            (SUB_ID2_MESSAGE_REPORT_REPLY, Some(to)) => {
                read::report_reply_body(header.device_id, body).map(|scope| {
                    CiMessage::MessageReportReply {
                        source: header.source,
                        ci_version: header.ci_version,
                        to,
                        scope,
                    }
                })
            }
            (SUB_ID2_MESSAGE_REPORT_END, Some(to)) => {
                Some(CiMessage::MessageReportEnd { header, to })
            }
            (SUB_ID2_PROFILE_INQUIRY, Some(to)) => Some(CiMessage::ProfileInquiry { header, to }),
            (SUB_ID2_PROFILE_REPLY, Some(to)) => {
                read::profile_reply_body(body).map(|profiles| CiMessage::ProfileReply {
                    header,
                    to,
                    profiles,
                })
            }
            (SUB_ID2_SET_PROFILE_ON, Some(to)) => read::profile_target_body(header.device_id, body)
                .map(|(target, profile)| CiMessage::SetProfileOn {
                    source: header.source,
                    ci_version: header.ci_version,
                    to,
                    target,
                    profile,
                }),
            (SUB_ID2_SET_PROFILE_OFF, Some(to)) => {
                read::profile_body(body).map(|profile| CiMessage::SetProfileOff {
                    header,
                    to,
                    profile,
                })
            }
            (SUB_ID2_PROFILE_ENABLED, None) => read::profile_target_body(header.device_id, body)
                .map(|(target, profile)| CiMessage::ProfileEnabled {
                    source: header.source,
                    ci_version: header.ci_version,
                    target,
                    profile,
                }),
            (SUB_ID2_PROFILE_DISABLED, None) => read::profile_target_body(header.device_id, body)
                .map(|(target, profile)| CiMessage::ProfileDisabled {
                    source: header.source,
                    ci_version: header.ci_version,
                    target,
                    profile,
                }),
            (SUB_ID2_PROFILE_ADDED, None) => {
                read::profile_body(body).map(|profile| CiMessage::ProfileAdded { header, profile })
            }
            (SUB_ID2_PROFILE_REMOVED, None) => read::profile_body(body)
                .map(|profile| CiMessage::ProfileRemoved { header, profile }),
            (SUB_ID2_PROFILE_DETAILS_INQUIRY, Some(to)) => {
                read::details_inquiry_body(body).map(|(profile, target)| {
                    CiMessage::ProfileDetailsInquiry {
                        header,
                        to,
                        profile,
                        target,
                    }
                })
            }
            (SUB_ID2_PROFILE_DETAILS_REPLY, Some(to)) => {
                read::details_reply_body(body).map(|(profile, target, data)| {
                    CiMessage::ProfileDetailsReply {
                        header,
                        to,
                        profile,
                        target,
                        data,
                    }
                })
            }
            (SUB_ID2_PROFILE_SPECIFIC_DATA, _) => {
                read::specific_data_body(body).map(|(profile, data)| {
                    CiMessage::ProfileSpecificData(ProfileSpecific::read(
                        header,
                        destination,
                        profile,
                        data,
                    ))
                })
            }
            (0x30 | 0x31 | 0x34..=0x39 | 0x3F, Some(to)) => {
                PeCiMessage::decode(sub_id2, body).map(|message| CiMessage::PropertyExchange {
                    header,
                    to,
                    message,
                })
            }
            (SUB_ID2_ACK, Some(to)) => {
                Ack::decode_body(body).map(|ack| CiMessage::Ack { header, to, ack })
            }
            (SUB_ID2_NAK, Some(to)) => {
                Nak::decode_body(body).map(|nak| CiMessage::Nak { header, to, nak })
            }
            // A message sent to the one kind of destination it is not defined
            // with: Broadcast where it must not be, a device where it must be
            // Broadcast.
            (
                SUB_ID2_DISCOVERY
                | SUB_ID2_INVALIDATE_MUID
                | SUB_ID2_DISCOVERY_REPLY
                | SUB_ID2_ENDPOINT_INQUIRY
                | SUB_ID2_ENDPOINT_REPLY
                | SUB_ID2_ACK
                | SUB_ID2_NAK
                | SUB_ID2_PROCESS_CAPABILITIES_INQUIRY
                | SUB_ID2_PROCESS_CAPABILITIES_REPLY
                | SUB_ID2_MESSAGE_REPORT_INQUIRY
                | SUB_ID2_MESSAGE_REPORT_REPLY
                | SUB_ID2_MESSAGE_REPORT_END
                | SUB_ID2_PROFILE_INQUIRY
                | SUB_ID2_PROFILE_REPLY
                | SUB_ID2_SET_PROFILE_ON
                | SUB_ID2_SET_PROFILE_OFF
                | SUB_ID2_PROFILE_ENABLED
                | SUB_ID2_PROFILE_DISABLED
                | SUB_ID2_PROFILE_ADDED
                | SUB_ID2_PROFILE_REMOVED
                | SUB_ID2_PROFILE_DETAILS_INQUIRY
                | SUB_ID2_PROFILE_DETAILS_REPLY
                | 0x30
                | 0x31
                | 0x34..=0x39
                | 0x3F,
                _,
            ) => return unread(UnreadReason::WrongDestination),
            _ => return unread(UnreadReason::UnsupportedMessage),
        };
        match message {
            Some(message) => Decoded::Message(message),
            None => unread(UnreadReason::Malformed),
        }
    }
}

/// What [`CiMessage::decode`] made of a payload.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub enum Decoded {
    /// A message this codec models.
    Message(CiMessage),
    /// A MIDI-CI message that could not be read, with what its header said.
    Unread(Unread),
    /// Not a MIDI-CI message (not `7E <id> 0D`), or too short to hold the 13-byte
    /// preamble.
    NotCi,
}

/// A MIDI-CI message the codec could not read, with the fields of its preamble
/// that were there: what a device needs to address the NAK that §5.3 and §5.11
/// require: a reply to a message the device does not support, one of a version
/// or format it does not support, a malformed one, or one whose Message Format
/// Version has reserved bits set.
///
/// The fields are the raw bytes, because the reason may be that one of them was
/// not valid: the Device ID may be reserved, the version may be 0, the source may
/// be a MUID nobody can own (`None`, and then there is nobody to answer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unread {
    /// The Device ID byte as sent.
    pub device_id: u8,
    /// The sub-ID#2 byte as sent.
    pub sub_id2: u8,
    /// The Message Format Version byte as sent.
    pub version: u8,
    /// The sender's MUID, or `None` when the four bytes are not one a device can
    /// own.
    pub source: Option<Muid>,
    /// The Destination MUID, or `None` when it is not a MUID or the Broadcast
    /// MUID.
    pub destination: Option<Destination>,
    /// Why it was not read.
    pub reason: UnreadReason,
}

/// Why a MIDI-CI message was not read ([`Unread::reason`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UnreadReason {
    /// A reserved bit (7 to 5) of the Message Format Version is set (§5.3).
    ReservedVersionBits,
    /// Message Format Version `0x00`, which is deprecated (§5.4).
    VersionZero,
    /// The Device ID is in the reserved range `0x10..=0x7D` (Table 5).
    ReservedDeviceId,
    /// The source MUID is reserved or the Broadcast MUID: a device cannot own
    /// either (§3.3).
    BadSource,
    /// The destination MUID is reserved (§3.3).
    BadDestination,
    /// A sub-ID#2 this codec does not model.
    UnsupportedMessage,
    /// An Inquiry: Endpoint or its reply with a Status that is reserved
    /// (`0x01..=0x7F`, Table 10): the Device cannot give that information.
    ReservedStatus,
    /// A message of a kind it models, sent to the wrong kind of destination
    /// (§3.3.3): Discovery, Invalidate MUID or a Profile report to one device, or a
    /// reply, an Endpoint message, a Profile inquiry or Set, ACK or NAK to the
    /// Broadcast MUID.
    WrongDestination,
    /// The body is malformed: too short, a byte with its top bit set, or a field
    /// out of range (a Receivable Maximum SysEx Message Size under 128, a
    /// Function Block that is not 0 to 31 or `0x7F`, a text that runs past the
    /// message).
    Malformed,
}

impl UnreadReason {
    /// The NAK status Table 16 gives this: `0x02` (version not
    /// supported) for a version the Device cannot read (§5.3 names reserved
    /// bits; version 0 is tutti's reading), `0x01` (message not
    /// supported), else `0x41` (message malformed).
    #[must_use]
    pub(crate) fn nak_status(self) -> NakStatus {
        match self {
            Self::ReservedVersionBits | Self::VersionZero => NakStatus::VersionNotSupported,
            Self::UnsupportedMessage => NakStatus::MessageNotSupported,
            // §5.8: a device that cannot provide the information for the given
            // Status replies with a NAK: the generic one, which is not retried.
            Self::ReservedStatus => NakStatus::Nak,
            _ => NakStatus::Malformed,
        }
    }
}

impl Unread {
    /// The NAK that answers this message from the device `own`: to the sender,
    /// on the Device ID it used (the Function Block when that was reserved), in
    /// this codec's [`CI_VERSION`] (the Receiver's own Message Format
    /// Version, §5.4), with the Status Code of `UnreadReason::nak_status` and
    /// the failed sub-ID#2 as the Original Transaction.
    ///
    /// `None` when there is nobody to answer (the source was not a MUID), when
    /// the sub-ID#2 is not seven bits, and for an ACK or a NAK, which are never
    /// answered with a NAK: a pair of devices would trade them forever (the spec
    /// does not say; this is tutti's rule).
    #[must_use]
    pub fn nak(&self, own: Muid) -> Option<CiMessage> {
        if matches!(self.sub_id2, SUB_ID2_ACK | SUB_ID2_NAK) {
            return None;
        }
        Some(CiMessage::Nak {
            header: CiHeader {
                device_id: CiDeviceId::new(self.device_id).unwrap_or(CiDeviceId::FunctionBlock),
                ci_version: CI_VERSION,
                source: own,
            },
            to: self.source?,
            nak: Nak::new(U7::new(self.sub_id2)?, self.reason.nak_status()),
        })
    }
}

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;
