#![doc = include_str!("../README.md")]
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
//! The `unsafe` of Tutti's plugin process edge, each piece behind a safe call,
//! so `tutti-plugin-ipc` and `tutti-plugin-server` forbid `unsafe`: it lives in
//! one self-contained place behind a safe wrapper, as in the per-format `-ffi`
//! crates.
//!
//! - **The audio slab's shared memory** ([`MappingWriter`], [`SharedMapping`],
//!   [`SlabHeader`]): the file, the mapping, the header both processes map (its
//!   layout is the ABI between them), and the only copies in and out. A mapping
//!   exists only with its one writer, which is what makes the copies safe to
//!   call. What the data region holds, and where, is `tutti-plugin-ipc`'s.
//! - **The wait for a child's end** ([`ExitWait`]): the host's wait for a server
//!   process to end, which the OS wakes (a pidfd on Linux, a kqueue on macOS, the
//!   process handle on Windows).
//! - **The watch on a host** ([`watch_host`], and [`watch_spawner`] on the host
//!   that spawned this process, found by [`host_pid`] from [`HOST_PID_ENV`]): a
//!   thread the OS wakes when a named process ends, which runs the caller's reply to that, and
//!   [`terminate_process`], the end that runs no cleanup.
//! - **What ties a server's life to its host's** ([`own_group`], [`kill_group`],
//!   [`has_ended`], [`bind_to_host`]): the process group a server leads, whose end reaches the
//!   helpers a plugin spawned (unix), and the kill-on-close Job Object the
//!   kernel ends a server with when the host dies (Windows). And
//!   [`protect_host_memory`], the host's dumpable flag cleared (Linux), so a
//!   same-user plugin cannot `ptrace` the host or write its memory.
//! - **The guard on a child process** ([`Reaped`]): a spawned child, killed with its
//!   process group and reaped when the guard drops, so no path out of the code that holds
//!   it leaves a running process or a zombie. It holds no `unsafe` of its own.
//! - **The OS calls** (`wait_readable`, `wait_readable_either` and `poll_wait`
//!   (the server's waits on its sockets, a bell's descriptor and the
//!   descriptors a plugin asked to have watched, and an in-process host's
//!   zero-timeout look at the same) on unix, `process_exists`,
//!   `parent_process_id`, `terminate_process` and, on Windows,
//!   `pipe_bytes_available`): the server's check that the host still runs and
//!   its end without cleanup, and the host's peek at a named pipe.
//!
//! - **The vocabulary of a wait on descriptors** ([`FdId`], [`Interest`],
//!   [`Watch`], [`Ready`]): the typed values `poll_wait` and `poll_now` take and
//!   return, on every platform.
//!
//! It names no tutti crate.

mod exit;
mod harden;
mod header;
mod lifetime;
mod mapping;
#[cfg(unix)]
#[path = "os/unix.rs"]
mod os;
#[cfg(windows)]
#[path = "os/windows.rs"]
mod os;
mod reaped;
mod wakeups;

pub use exit::{ExitWait, HOST_PID_ENV, Host, host_pid, watch_host, watch_spawner};
pub use harden::protect_host_memory;
pub use header::{Direction, RING_SLOTS, SLAB_HEADER_BYTES, SlabHeader, slot_for};
pub use lifetime::{bind_to_host, has_ended, kill_group, own_group};
pub use mapping::{MappingWriter, SharedMapping, SlabHandle};
pub use os::*;
pub use reaped::Reaped;
pub use wakeups::{FdId, Interest, Ready, Watch};

/// Why a slab could not be made, opened or trusted.
#[derive(Debug, thiserror::Error)]
pub enum SlabError {
    /// The backing file could not be created, opened, sized or mapped.
    #[error("failed to {op} the slab file: {source}")]
    File {
        /// What was being done to the file, as a verb phrase.
        op: &'static str,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// The operating system's random source could not name a new slab.
    #[error("failed to draw a name for the slab: {0}")]
    Name(getrandom::Error),
    /// The peer's slab is shorter than the layout this side expects.
    #[error("slab is {actual} bytes, layout needs {expected}")]
    Short {
        /// The file's length.
        actual: u64,
        /// The length the layout needs.
        expected: usize,
    },
    /// The mapping does not begin with the tutti slab magic: it is not a slab.
    #[error("not a tutti audio slab: magic {found:#x}, expected {expected:#x}")]
    BadMagic {
        /// The magic the mapping holds.
        found: u64,
        /// The magic of a tutti slab.
        expected: u64,
    },
    /// The slab's header is of another version than this build speaks.
    #[error("slab header version {found}, this build speaks {expected}")]
    BadVersion {
        /// The version the mapping holds.
        found: u32,
        /// The version this build speaks.
        expected: u32,
    },
    /// The slab's ring has another depth than this build's.
    #[error("slab has {found} ring slots, this build expects {expected}")]
    BadSlots {
        /// The slot count the mapping holds.
        found: u32,
        /// The slot count of this build.
        expected: usize,
    },
}
