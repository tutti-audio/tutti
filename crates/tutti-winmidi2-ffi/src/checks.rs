//! The OS-free checks: tested everywhere, and used by `os`. Off Windows they build
//! for their tests only, so what only `os` uses is unused there.

pub(crate) mod timestamp;
