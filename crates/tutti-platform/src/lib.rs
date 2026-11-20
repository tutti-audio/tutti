#![doc = include_str!("../README.md")]
//!
//! ## What this is
//!
//! The code that differs by operating system or architecture, in one crate at the
//! bottom of tutti, so that every crate above it builds on every target. It names no
//! tutti crate; its dependencies are `std`, `async-executor`, `async-task`, `web-time` and the OS
//! bindings (`libc` on Unix, `windows` on Windows, `thread-priority` everywhere but the
//! web).
//!
//! - [`Instant`] and [`SystemTime`] — std's types on a native target (the same
//!   type, so an app's own `Instant` keeps working), `performance.now()` and
//!   `Date.now()` on the web. `Duration` stays `std::time::Duration`.
//! - [`monotonic_ns`] / [`sleep_until_ns`] — the OS monotonic clock, in
//!   nanoseconds, and a sleep until an absolute deadline on it (it returns at once on
//!   the web, which cannot block).
//! - [`request_priority`] — asks the OS to raise the calling thread's priority
//!   and returns what it granted ([`PriorityOutcome`], [`Scheduling`]) or the
//!   refusal (never real time on Linux; always a refusal on the web).
//! - [`spawn_named`] — starts a named, optionally prioritised control-side
//!   thread whose body gets a [`StopToken`]; its [`ThreadHandle`] stops and
//!   joins on drop (or within a bound, `stop_within`) and reports a panic once
//!   ([`Panicked`]); on the web it fails as `Unsupported`.
//! - [`Executor`] — where tutti's background work runs: a run queue and a deadline queue
//!   that the caller's [`run_until_stalled`](Executor::run_until_stalled) and
//!   [`try_tick`](Executor::try_tick), or the [`Threads`] of
//!   [`spawn_threads`](Executor::spawn_threads) (native), drive.
//!   [`spawn`](Executor::spawn) returns a [`Task`] that cancels on drop;
//!   [`unblock`](Executor::unblock) runs a blocking closure off the drive;
//!   [`spawn_dedicated`](Executor::spawn_dedicated) gives a long-lived future a thread
//!   of its own; [`Executor::sleep_until`] is a deadline as a future ([`Sleep`]).
//! - [`MainThreadMarker`] — proof that the holder is on the process's main
//!   thread, made only where the OS says so (`!Send`, `!Sync`).
//! - [`Asset`] — where bytes come from, as every file entry point takes it
//!   (`impl Into<Asset>`): a path or shared bytes, or an app's own [`Open`]
//!   source that opens again and again, with an [`Identity`] to cache by (a
//!   path's carries its [`Stamp`]). [`Destination`] is where one write goes: a
//!   path, written staged and replacing by default (or refusing an existing file
//!   with [`create_new`](Destination::create_new)), or a borrowed writer;
//!   [`Staged`] opens the temporary beside a path that a destination writes
//!   through, a [`StagedFile`] that syncs, closes and renames on `commit`, keeping the file in a [`StagedCommitError`] if that fails. The path implementations fail at `open` on
//!   the web.
//! - [`with_no_denormals`] — runs a closure with subnormals flushed to zero and
//!   restores the mode after, unwinding included.
//!
//! An item belongs here only if it needs a `#[cfg]` on the platform or
//! architecture: each is a module whose platform version one `cfg_attr` chooses.
//! The asset edge is the exception: it has no `cfg`, because std's file calls
//! fail at run time on the web, and it sits here so every reader reaches it.
//! **The root is the API.** Every item is re-exported at the root
//! (`tutti_platform::monotonic_ns`); the modules are private.
//!
//! ## Safety
//!
//! This crate's `unsafe` is the OS calls and nothing else; each sits behind a safe
//! function, and a caller never sees it:
//!
//! - `monotonic_ns` (`clock/`): `clock_gettime` on Unix, `QueryPerformanceCounter`
//!   and `QueryPerformanceFrequency` on Windows.
//! - `sleep_until_ns` (`sleep/absolute.rs`): the `clock_nanosleep` call on Linux and
//!   Android.
//! - `MainThreadMarker::new` (`main_thread/`): the OS's "is this the main thread"
//!   query, one module per OS (on Windows, a `.CRT$XCU` initializer entry), and
//!   `MainThreadMarker::new_unchecked`, whose `# Safety` names the caller's duty (a
//!   test harness's constructor).
//! - `with_no_denormals` (`denormals/`): saves, sets and restores the FPU control
//!   register with `asm!`, which has no safe spelling; its `# Soundness` docs say
//!   what it does and does not guarantee.
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

mod asset;
mod clock;
mod denormals;
mod executor;
mod main_thread;
mod priority;
mod sleep;
mod thread;

pub use asset::{
    Asset, Destination, Identity, Open, ReadSeek, Staged, StagedCommitError, StagedFile,
    StagedKeep, Stamp,
};
pub use clock::monotonic_ns;
pub use denormals::with_no_denormals;
pub use executor::*;
pub use main_thread::MainThreadMarker;
pub use priority::{PriorityOutcome, PriorityRequest, Scheduling, request_priority};
pub use sleep::sleep_until_ns;
pub use thread::{Panicked, StopToken, ThreadHandle, spawn_named};
pub use web_time::{Instant, SystemTime};
