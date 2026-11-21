//! The OS-free checks and decisions: tested everywhere, and used by `os`. Off macOS
//! they build for their tests only, so what only `os` uses is unused there.

pub(crate) mod backoff;
pub(crate) mod blocks;
pub(crate) mod client_start;
pub(crate) mod endpoint_phases;
pub(crate) mod find;
pub(crate) mod notification;
pub(crate) mod os_status;
pub(crate) mod unique_id;
