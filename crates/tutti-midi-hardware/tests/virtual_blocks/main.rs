//! Virtual endpoints that declare Function Blocks, on macOS 15's UMP Endpoint.
//!
//! The endpoint is a main-thread object (Apple documents every use of a
//! `MIDIUMPMutableEndpoint` as main-thread only), so this target sets
//! `harness = false` and owns `main`, the process's main thread; the default
//! harness would run each case on a worker thread. nextest runs each case in its
//! own process, which the cases rely on: with the device watch's run loop the
//! manager's list follows changes, and the process has one CoreMIDI client.
//!
//! The class exists from macOS 15: each case opens the endpoint on 15 or later
//! and asserts the refusal by name before it. Off macOS the target has no case.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[cfg_attr(all(target_os = "macos", feature = "coremidi"), path = "macos.rs")]
#[cfg_attr(not(all(target_os = "macos", feature = "coremidi")), path = "other.rs")]
mod platform;

fn main() {
    platform::cases();
}
