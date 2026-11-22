//! `UmpEndpoint` publishes the Function Blocks it is created with.
//!
//! An `UmpEndpoint` is a main-thread object (Apple documents every use of a
//! `MIDIUMPMutableEndpoint` as main-thread only), so this target sets
//! `harness = false` and owns `main`, which is the process's main thread; the
//! default harness would run each case on a worker thread. nextest runs each case
//! in its own process, which the cases rely on: with no run loop in the process
//! the manager's list is the one it first took, so nothing reads it before the
//! endpoint exists, and the process has one CoreMIDI client.
//!
//! Each case that reads CoreMIDI's own listing of UMP Endpoints (`ump_endpoints`)
//! reads it as another app would. The class exists from macOS 15: a case creates
//! the endpoint on 15 or later and asserts the refusal by name before it.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[cfg_attr(target_os = "macos", path = "macos.rs")]
#[cfg_attr(not(target_os = "macos"), path = "other.rs")]
mod platform;

fn main() {
    platform::cases();
}
