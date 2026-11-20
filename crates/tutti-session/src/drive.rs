//! Who runs the session's background queue: its own threads, or the app's `update`.
//!
//! A session that builds its own executor starts threads for it where a thread can start
//! (a native target), and the web has none; a session handed an executor leaves it to
//! `update`. The platform's difference is this one module's.

pub(crate) use os::Drive;

#[cfg_attr(not(target_arch = "wasm32"), path = "drive/native.rs")]
#[cfg_attr(target_arch = "wasm32", path = "drive/web.rs")]
mod os;
