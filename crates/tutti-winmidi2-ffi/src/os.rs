//! The Windows half: every module that calls the service. `lib.rs` compiles it on
//! Windows only and re-exports the public surface from here.

mod call;
mod clock;
mod connection;
mod endpoint;
mod error;
mod session;
mod virtual_device;
mod watcher;

pub use crate::checks::declared::{Block, DeclaredField, Refusal};
use clock::Clock;
pub use connection::Connection;
pub use endpoint::{Endpoint, Loopback, endpoints, ensure_service_available};
pub use error::Error;
pub use session::Session;
pub use virtual_device::{VirtualDevice, VirtualDeviceSpec};
pub use watcher::{Change, Watcher};
