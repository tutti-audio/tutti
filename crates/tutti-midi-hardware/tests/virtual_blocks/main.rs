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

#[cfg_attr(target_os = "macos", path = "macos.rs")]
#[cfg_attr(not(target_os = "macos"), path = "other.rs")]
mod platform;

fn main() {
    platform::cases();
}
