//! The macOS half: every CoreMIDI call.

mod client;
mod endpoint;
mod error;
mod event_list;
mod host_clock;
mod input_port;
mod objc_exception;
mod output_port;
mod receive;
mod run_loop;
mod source;
mod ump_destination;
mod ump_endpoint;
mod ump_endpoints;

pub use crate::checks::blocks::{BlockField, BlockRefusal};
pub use crate::checks::find::FindError;
pub use crate::checks::notification::Notification;
pub use crate::checks::unique_id::UniqueId;
pub use client::{SharedClient, StartClient};
pub use endpoint::{Endpoint, destinations, find_destination, find_source, sources};
pub use error::{Error, Operation, OsStatus};
pub use host_clock::{Timebase, host_ticks};
pub use input_port::InputPort;
pub use objc_exception::ObjcException;
pub use output_port::OutputPort;
pub use run_loop::run_notifications;
pub use source::UmpSource;
pub use ump_destination::UmpDestination;
pub use ump_endpoint::{CreateUmpEndpoint, UmpEndpoint, UmpSender};
pub use ump_endpoints::{UmpBlockInfo, UmpEndpointInfo, UmpEndpointsError, ump_endpoints};
