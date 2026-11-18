#![doc = include_str!("../README.md")]
//! The pure core of MIDI device detection and the MIDI-CI codec.
//!
//! The CI codec ([`CiMessage`], [`Muid`], [`DiscoveryData`])
//! turns Discovery, Invalidate MUID, Inquiry: Endpoint, ACK, NAK, the Profile Configuration messages
//! and the Process Inquiry messages
//! into SysEx payload bytes and back;
//! it holds no state and no transport. [`CiDevice`] is the Management state
//! machine over it (Discovery, MUID collisions, Invalidate MUID, NAK, Endpoint
//! Inquiry), shaped as [`Connection`] is, and [`ProfileInitiator`] the Initiator of Profile
//! Configuration with one peer, and [`ProcessInitiator`] the Initiator of Process Inquiry.
//!
//! What a peer declares about itself becomes values the rest of tutti shares:
//! [`Dialect::of`] reads an [`Endpoint`] and a [`ProfileTable`] and says what
//! to send to a Function Block (MIDI 1.0 or MIDI 2.0 channel voice, and the MPE
//! zones enabled on it), and [`SuggestedCc`] is the table of the Default Control
//! Change Mapping Profile, the functions it suggests for each Control Change.
//! The Default Drum Note Map ([`DrumInstrument`]), Drawbar Organ ([`Drawbar`]) and
//! Rotary Speaker ([`RotaryControl`]) Profiles are the same kind of table, and
//! [`Gm2FunctionBlock`] and [`Gm2SingleChannel`] say whether a peer has General
//! MIDI 2 enabled.
//!
//! A backend reports what the OS says as an [`OsChange`]; [`DeviceTable::apply`]
//! returns the [`DeviceEvent`] that change causes, or `None` when it alters
//! nothing. The table holds no OS handle, thread or clock, so every transition
//! is a plain test.
//!
//! ```
//! use tutti_midi_device::{
//!     DeviceEvent, DeviceTable, PortDirection, EndpointId, Identity, OsChange, OsDevice,
//! };
//!
//! let mut table = DeviceTable::default();
//! let keys = OsDevice {
//!     handle: EndpointId::from_raw(1),
//!     identity: Identity::alsa("Keystep", "Keystep MIDI 1"),
//!     name: "Keystep: Keystep MIDI 1".into(),
//!     direction: PortDirection::Input,
//! };
//! let Some(DeviceEvent::Added(info)) = table.apply(OsChange::Present(keys.clone())) else {
//!     unreachable!()
//! };
//! assert!(table.list().eq([&info]));
//! // The same report again changes nothing.
//! assert_eq!(table.apply(OsChange::Present(keys)), None);
//! assert!(matches!(
//!     table.apply(OsChange::Gone(EndpointId::from_raw(1))),
//!     Some(DeviceEvent::Removed(_))
//! ));
//! ```

//! The UMP [`Endpoint`] and [`Connection`] are the other half: an endpoint's declared
//! state with its answers ([`Endpoint::reply`]), and the discoverer's steps over
//! received messages and time as a value (M2-104 §7.1, M2-100 Table 4).
//!
//! ```
//! use std::time::{Duration, Instant};
//! use tutti_midi_device::{Action, Connection, Ending, STEP_TIMEOUT};
//!
//! let start = Instant::now();
//! let (mut connection, first) = Connection::new(start);
//! // `first` is `Action::Send(discovery)`: the driver loop handles it like any
//! // other action. A peer that never answers is sent the discovery again after
//! // each `STEP_TIMEOUT`, and is a MIDI 1.0 device after the third.
//! assert_eq!(connection.on_timeout(start + STEP_TIMEOUT), first);
//! connection.on_timeout(start + 2 * STEP_TIMEOUT);
//! assert_eq!(
//!     connection.on_timeout(start + 3 * STEP_TIMEOUT + Duration::from_millis(1)),
//!     Action::Settled(Ending::Midi1Fallback)
//! );
//! ```
//!
#![forbid(unsafe_code)]

mod ci;
mod connection;
mod dialect;
mod endpoint;
mod entropy;
mod id;
mod inquiry;
mod jr;
mod learned;
mod name;
mod table;

pub use ci::{
    Ack, ActionError, Arrival, ArticulationDetails, ArticulationDetailsError, ArticulationFeatures,
    ArticulationLevel, BitmapPosition, BodyLimit, BufferLimit, CI_SUB_ID_1, CI_VERSION, CallError,
    CallId, CanSet, CcFunction, CcIndex, CcPart, CcReset, CcValue, ChangeKind, ChannelControllers,
    ChannelsDetail, ChunkNo, ChunkPosition, ChunkTotal, CiAction, CiCategories, CiDevice,
    CiDeviceId, CiHeader, CiMessage, CiPeer, CiVersion, ClusterKind, ColumnSource, CommonState,
    ConfirmIcon, ConfirmType, Confirmation, CtrlControl, CtrlKind, CtrlRange, CtrlValueKind,
    CustomResource, CustomSounds, DISCOVERY_WINDOW, DataControl, Decoded, DefaultCcLevel,
    Destination, DetailsData, DiscoveryData, Drawbar, DrawbarSetting, DrumControl, DrumFeatures,
    DrumInstrument, DrumNoteMapLevel, DrumSounds, EndpointInfo, EndpointStatus, EntryError,
    GetStateError, Gm2ChannelCount, Gm2FunctionBlock, Gm2Role, Gm2SingleChannel, InquiryTarget,
    JsonPointer, LinkRole, ListTooLong, ListedTwice, MAX_LIST_ENTRIES, ManufacturerProfile,
    MaxSysexSize, MediaType, MessageText, ModeId, Muid, MutuallyExclusiveSet, Nak, NotABodyLimit,
    NotABufferLimit, NotAChunkNo, NotADataControl, NotADeviceId, NotADrawbarSetting,
    NotAGm2ChannelCount, NotAMuid, NotAMutuallyExclusiveSet, NotAPeChannel, NotAPriority,
    NotARequestId, NotASimultaneousRequests, NotAStateSize, NotAStepCount, NotAStreamLimit,
    NotASysexSize, NotATimeoutWait, NotAVersion, NotProfileChannels, NotSetProfileOn,
    NotSignificantBits, NotUnixSeconds, NotValid, NotVariationCount, NotVendorName, NoteData,
    ORGAN_HIGHEST_NOTE, ORGAN_LOWEST_NOTE, ORGAN_REFERENCE_NOTE, OpaqueBody, OrganFeatures,
    OrganLevel, OrganOption, OrganValue, Outcome, PE_JSON_MAX_DEPTH, PE_MAX_CHUNK_DATA_LEN,
    PE_MAX_HEADER_LEN, PE_TIMEOUT, PEER_REPLY_TIMEOUT, PaginationFault, ParamPath, PartialError,
    PartialUpdate, PartialValue, PeAction, PeAllCtrlEntry, PeAllCtrlList, PeAnswer, PeBankProgram,
    PeBasicChannelRx, PeBasicChannelTx, PeBodyError, PeBuffered, PeCall, PeCapabilities,
    PeCapabilitiesFailure, PeChCtrlEntry, PeChCtrlList, PeChange, PeChannel, PeChannelEntry,
    PeChannelList, PeChannelMode, PeChunk, PeChunkError, PeCiMessage, PeCluster, PeColumn,
    PeController, PeCtrl, PeCtrlMapEntry, PeCtrlMapList, PeCurrentMode, PeData, PeDataError,
    PeDataFault, PeDecimal, PeDeviceInfo, PeEncoding, PeEndedBy, PeError, PeExpired, PeExtensions,
    PeExternalSync, PeExtra, PeExtraError, PeFlowControl, PeGet, PeHeader, PeHeaderError,
    PeHeaderTooLong, PeHeld, PeInitiator, PeJson, PeJsonError, PeJsonSchema, PeKind, PeLink,
    PeLocalOn, PeMessage, PeMessageKind, PeMidiMode, PeModeEntry, PeModeList, PeNotification,
    PeObject, PeOpaque, PeOutcome, PePage, PeProgramEntry, PeProgramList, PeReassembler,
    PeReassemblyError, PeReassemblyLimits, PeReceived, PeRefused, PeReply, PeRequest, PeRequestId,
    PeResource, PeResourceList, PeResponse, PeSet, PeSetScope, PeSettings, PeSignal, PeState,
    PeStateEntry, PeStateList, PeStatus, PeStatusClass, PeStep, PeSubscribed, PeSubscription,
    PeTerminator, PeText, PeTransaction, PeUnreadable, PeUpdateCommand, PeVersion, Priority,
    ProcessAction, ProcessError, ProcessFeatures, ProcessInitiator, ProfileAction, ProfileChange,
    ProfileChannels, ProfileDataError, ProfileEntry, ProfileId, ProfileInitiator, ProfileList,
    ProfileLists, ProfileOnRequest, ProfileSpecific, ProfileState, ProfileTable, ProfileTarget,
    PropertyName, PublishError, RefusedAt, ReportRequest, ReportScope, Request, RequestError,
    RequestRefused, ResIdUse, ResourceAction, ResourceDefaults, ResourceEntry,
    ResourceEntryBuilder, ResourceEvent, ResourceFailure, ResourceGet, ResourceId,
    ResourceInitiator, ResourceName, ResourceResponder, ResourceSet, ResourceStore, ResourceTarget,
    ResponderAction, ResponderEvent, ResponderSettings, RestoreError, RoleName, RotaryControl,
    RotarySpeaker, RotaryValue, SUB_ID2_ACK, SUB_ID2_DISCOVERY, SUB_ID2_DISCOVERY_REPLY,
    SUB_ID2_ENDPOINT_INQUIRY, SUB_ID2_ENDPOINT_REPLY, SUB_ID2_INVALIDATE_MUID,
    SUB_ID2_MESSAGE_REPORT_END, SUB_ID2_MESSAGE_REPORT_INQUIRY, SUB_ID2_MESSAGE_REPORT_REPLY,
    SUB_ID2_NAK, SUB_ID2_PE_CAPABILITIES_INQUIRY, SUB_ID2_PE_CAPABILITIES_REPLY, SUB_ID2_PE_NOTIFY,
    SUB_ID2_PROCESS_CAPABILITIES_INQUIRY, SUB_ID2_PROCESS_CAPABILITIES_REPLY,
    SUB_ID2_PROFILE_ADDED, SUB_ID2_PROFILE_DETAILS_INQUIRY, SUB_ID2_PROFILE_DETAILS_REPLY,
    SUB_ID2_PROFILE_DISABLED, SUB_ID2_PROFILE_ENABLED, SUB_ID2_PROFILE_INQUIRY,
    SUB_ID2_PROFILE_REMOVED, SUB_ID2_PROFILE_REPLY, SUB_ID2_PROFILE_SPECIFIC_DATA,
    SUB_ID2_SET_PROFILE_OFF, SUB_ID2_SET_PROFILE_ON, SavedState, SelectorAction, SetBody,
    SignificantBits, SimpleAction, SimultaneousRequests, Sound, SoundSet, SpecificData,
    StandardProfile, StateDevice, StateId, StateRev, StateSize, StateStamp, StepCount, StoreError,
    StreamLimit, SubclassCounts, SubscribeId, SuggestedCc, SwitchMeaning, SystemMessages,
    TimeoutWait, TwoState, TypeHint, UnixSeconds, Unread, UnreadReason, UpdateKind, VariationCount,
    Verdict, VibratoChorus, WrongDevice, decode_body, encode_body, is_property_exchange,
    is_set_profile_on, mcoded7, send_level, split_message, volume_gain,
};

pub use connection::{Action, Connection, Ending, MAX_ATTEMPTS, NAME_GRACE, STEP_TIMEOUT};
pub use dialect::{ChannelVoice, Dialect, Expression, JrTimestamps, Midi1Rate, NoDialect};
pub use endpoint::{
    BlockError, Capabilities, ChangeError, Configuration, Endpoint, EndpointChange, FunctionBlock,
    FunctionBlockBuilder, MAX_FUNCTION_BLOCKS, MidiIdentity, NoProtocol, Reply, Supports,
    UMP_VERSION,
};
pub use entropy::{EntropyError, EntropyWords, random_bytes};
pub use id::{DeviceId, DeviceInfo, EndpointId, Identity, PortDirection, WinMmDevice};
pub use inquiry::{INQUIRY_ATTEMPTS, INQUIRY_TIMEOUT, IdentityInquiry, InquiryAction};
pub use jr::{ClockHeard, JrLock, JrReceiver, JrTime, TICK_HZ, Unmapped};
pub use learned::{Learned, Match, SavedDevice};
pub use name::{BlockName, EndpointName, Name, NameError, ProductInstanceId};
pub use table::{BindPolicy, DeviceEvent, DeviceTable, OsChange, OsDevice, Resolved};
