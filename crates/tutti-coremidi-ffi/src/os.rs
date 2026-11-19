//! The macOS half: every CoreMIDI call.

mod client;
mod client_retry;
mod destination;
mod endpoint;
mod error;
mod host_clock;
mod objc_exception;
mod source;
mod ump_endpoints;

pub use crate::find::FindError;
pub use crate::notification::Notification;
pub use crate::unique_id::UniqueId;
pub use client::SharedClient;
pub use client_retry::create_client;
pub use destination::UmpDestination;
pub use endpoint::{Endpoint, destinations, find_destination, find_source, sources};
pub use error::{Error, Operation, OsStatus};
pub use host_clock::{Timebase, host_ticks};
pub use objc_exception::ObjcException;
pub use source::UmpSource;
pub use ump_endpoints::{UmpBlockInfo, UmpEndpointInfo, UmpEndpointsError, ump_endpoints};
