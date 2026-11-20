//! The OS-free checks and decisions: tested everywhere, and used by `os`. Off macOS
//! they build for their tests only, so what only `os` uses is unused there.

pub(crate) mod blocks;
pub(crate) mod find;
pub(crate) mod notification;
pub(crate) mod unique_id;
