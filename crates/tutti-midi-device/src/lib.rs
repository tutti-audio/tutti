#![doc = include_str!("../README.md")]
//! The pure core of MIDI device detection.
//!
//! What a peer declares about itself becomes values the rest of tutti shares:
//! [`Dialect::of`] reads an [`Endpoint`] and a `tutti_midi_ci::ProfileTable` and says
//! what to send to a Function Block (MIDI 1.0 or MIDI 2.0 channel voice, and the MPE
//! zones enabled on it). The MIDI-CI codec and state machines are `tutti-midi-ci`.
//!
//! A backend reports what the OS says as an [`OsChange`]; [`DeviceTable::apply`]
//! returns the [`DeviceEvent`] that change causes, or `None` when it alters
//! nothing. The table holds no OS handle, thread or clock, so every transition
//! is a plain test.
//!
//! ```
//! use tutti_midi_device::{
//!     DeviceEvent, DeviceTable, PortDirection, EndpointId, OsEndpointIdentity, OsChange, OsDevice,
//! };
//!
//! let mut table = DeviceTable::default();
//! let keys = OsDevice {
//!     handle: EndpointId::from_raw(1),
//!     identity: OsEndpointIdentity::alsa("Keystep", "Keystep MIDI 1"),
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

//! The UMP [`Endpoint`] and [`EndpointDiscovery`] are the other half: an endpoint's declared
//! state with its answers ([`Endpoint::reply`]), and the discoverer's steps over
//! received messages and time as a value (M2-104 §7.1, M2-100 Table 4).
//!
//! ```
//! use std::time::Duration;
//! use tutti_platform::Instant;
//! use tutti_midi_device::{DiscoveryAction, EndpointDiscovery, DiscoveryEnding, STEP_TIMEOUT};
//!
//! let start = Instant::now();
//! let (mut connection, first) = EndpointDiscovery::new(start);
//! // `first` is `DiscoveryAction::Send(discovery)`: the driver loop handles it like any
//! // other action. A peer that never answers is sent the discovery again after
//! // each `STEP_TIMEOUT`, and is a MIDI 1.0 device after the third.
//! assert_eq!(connection.on_timeout(start + STEP_TIMEOUT), first);
//! connection.on_timeout(start + 2 * STEP_TIMEOUT);
//! assert_eq!(
//!     connection.on_timeout(start + 3 * STEP_TIMEOUT + Duration::from_millis(1)),
//!     DiscoveryAction::Ended(DiscoveryEnding::Midi1Fallback)
//! );
//! ```
//!
#![forbid(unsafe_code)]

mod deadline;
mod dialect;
mod endpoint;
mod endpoint_discovery;
mod id;
mod inquiry;
mod jr;
mod learned;
mod table;

pub use dialect::{ChannelVoice, Dialect, Expression, JrTimestamps, Midi1Rate, NoDialect};
#[cfg(feature = "serde")]
pub use endpoint::NotAnEndpoint;
pub use endpoint::{
    BlockError, BlockField, Capabilities, ChangeError, Configuration, Endpoint, EndpointChange,
    EndpointReply, FunctionBlock, FunctionBlockBuilder, MAX_FUNCTION_BLOCKS, MidiIdentity,
    NoProtocol, Supports, UMP_VERSION,
};
pub use endpoint_discovery::{
    DiscoveryAction, DiscoveryEnding, EndpointDiscovery, MAX_ATTEMPTS, NAME_GRACE, STEP_TIMEOUT,
};
pub use id::{
    DeviceInfo, EndpointId, MidiDeviceId, OsEndpointIdentity, PortDirection, WinMmDevice,
};
pub use inquiry::{INQUIRY_ATTEMPTS, INQUIRY_TIMEOUT, IdentityInquiry, InquiryAction};
pub use jr::{ClockHeard, JrLock, JrReceiver, JrTime, Unmapped};
pub use learned::{Learned, Match, SavedDevice};
pub use table::{BindPolicy, DeviceEvent, DeviceTable, OsChange, OsDevice, Resolved};
