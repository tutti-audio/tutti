#![doc = include_str!("../README.md")]
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
//! The `unsafe` of Tutti's plugin process edge, each piece behind a safe call,
//! so `tutti-plugin-ipc` and `tutti-plugin-server` forbid `unsafe`: it lives in
//! one self-contained place behind a safe wrapper, as in the per-format `-ffi`
//! crates.
//!
//! - **The audio slab's shared memory** ([`MappingWriter`], [`SharedMapping`],
//!   [`SlabHeader`], [`SlabHandle`]): the memory (a sealed `memfd` on Linux, an
//!   unlinked POSIX shared memory object on macOS, a temp file on Windows), its
//!   descriptor sent over the control socket, the mapping, the header both
//!   processes map (its layout is the ABI between them), and the only copies in
//!   and out. A mapping exists only with its one writer, which is what makes the
//!   copies safe to call. What the data region holds, and where, is
//!   `tutti-plugin-ipc`'s.
//! - **The audio lane's wire** ([`LaneWire`], and on unix [`lane_pair`],
//!   [`send_lane`], [`receive_lane`]): the lane's socket end travels over the
//!   broker connection as a descriptor on unix, and a fresh pipe's name on Windows.
//! - **The host's socket on a server's stdin** ([`adopt_host_socket`], unix): the
//!   stream socket the host handed the server as descriptor 0 moves to a descriptor of
//!   its own, and `/dev/null` takes descriptor 0's place.
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
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

#[cfg(unix)]
mod descriptor;
mod exit;
mod harden;
mod header;
mod lane;
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

pub use exit::ExitWait;
// A loom build leaves the executor out (see `tutti-platform`), and the host watch with it.
#[cfg(not(loom))]
pub use exit::{HOST_PID_ENV, Host, host_pid, watch_host, watch_spawner};
pub use harden::protect_host_memory;
pub use header::{Direction, RING_SLOTS, SLAB_HEADER_BYTES, SlabHeader, slot_for};
pub use lane::*;
pub use lifetime::{bind_to_host, has_ended, kill_group, own_group};
pub use mapping::{
    Blob, BlobWire, Connection, Lease, MappingWriter, Received, SharedMapping, SlabHandle, SlabWire,
};
pub use os::*;
pub use reaped::Reaped;
pub use wakeups::{FdId, Interest, Ready, Watch};

/// Why a slab or a blob could not be made, opened or trusted.
#[derive(Debug, thiserror::Error)]
pub enum SlabError {
    /// The slab's memory could not be created, opened, sized, sealed or mapped.
    #[error("failed to {op} the slab memory: {source}")]
    File {
        /// What was being done to the file, as a verb phrase.
        op: &'static str,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// The operating system's random source could not name a new slab (macOS and
    /// Windows, where a slab is briefly or lastingly named).
    #[error("failed to draw a name for the slab: {0}")]
    Name(getrandom::Error),
    /// The peer's slab is shorter than the layout this side expects.
    #[error("slab is {actual} bytes, layout needs {expected}")]
    Short {
        /// The slab's length.
        actual: u64,
        /// The length the layout needs.
        expected: usize,
    },
    /// The peer's slab is longer than the layout this side expects: a sealed
    /// slab is exactly the layout's length.
    #[error("slab is {actual} bytes, layout is exactly {expected}")]
    Long {
        /// The slab's length.
        actual: u64,
        /// The length the layout needs.
        expected: usize,
    },
    /// The descriptor received is not a memory file sealed against shrinking and
    /// growing: whoever holds it could resize the memory under the mapping.
    #[error("the slab descriptor is not a memfd sealed against shrinking and growing")]
    NotSealed,
    /// The descriptor received is not a shared memory object (macOS): it is a pipe,
    /// socket, directory or device.
    #[error("the descriptor is not a shared memory object")]
    NotSharedMemory,
    /// The byte that carries a descriptor (a slab's, a lane's) arrived without
    /// one, or did not arrive.
    #[error("the announced descriptor did not arrive")]
    NoDescriptor,
    /// More than one descriptor arrived with the byte, or the control
    /// message was cut short: all that arrived is closed.
    #[error("more than one descriptor, or a cut control message, arrived with the byte")]
    ExtraDescriptors,
    /// The byte that carries a descriptor is not the mark: the stream is
    /// out of step with the sender, and any descriptor that came is closed.
    #[error("the descriptor byte is {found:#x}, not the mark")]
    BadMark {
        /// The byte received.
        found: u8,
    },
    /// The OS failed or the peer closed while a descriptor was received.
    #[error("failed to receive the descriptor: {source}")]
    Transfer {
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// A blob's declared length is over the limit the receiver allows.
    #[error("blob is declared {declared} bytes, over the {limit}-byte limit")]
    BlobTooLarge {
        /// The length the frame declared.
        declared: u64,
        /// The limit it exceeded.
        limit: usize,
    },
    /// A blob's object is not the length its frame declared.
    #[error("blob object is {actual} bytes, its frame declared {declared}")]
    BlobSize {
        /// The length the frame declared.
        declared: u64,
        /// The object's length.
        actual: u64,
    },
    /// The memory for a blob's copy could not be allocated.
    #[error("no room for a blob of {bytes} bytes")]
    NoRoom {
        /// The length asked for.
        bytes: usize,
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
