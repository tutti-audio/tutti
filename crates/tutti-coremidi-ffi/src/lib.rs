#![doc = include_str!("../README.md")]
//! The unsafe half of the macOS MIDI backend. Every `unsafe` call
//! `tutti-midi-hardware` makes into CoreMIDI and the Mach clock lives here, behind
//! safe handles; the backend above it is `#![forbid(unsafe_code)]` and holds the
//! logic (enumeration, the device watch, the packet to `Instant` placement).
//!
//! The crate sits one layer above `tutti-ump` and takes and returns its types for what
//! the UMP spec defines (protocol, Function Block fields); a value CoreMIDI holds
//! that the spec does not define is that field's `Reserved`.
//!
//! The crate is empty off macOS, except the checks and decisions with no OS call
//! (`Notification`'s mapping, the identity check of a take), which are tested on every
//! target.
//!
//! ## What is here
//!
//! | | |
//! |---|---|
//! | `UmpSource` | a native-UMP virtual source in the MIDI 1.0 or MIDI 2.0 protocol (a `tutti_ump::Protocol`; a reserved one is refused): it disposes its endpoint on drop, and sends a UMP message as an event list. |
//! | `UmpDestination` | a native-UMP virtual destination: it hands its callback each UMP message of each packet with its time as an `Option<Duration>` since the host clock's origin, and counts a truncated tail it drops |
//! | `UmpEndpoint` | a virtual UMP Endpoint that declares Function Blocks (macOS 15's `MIDIUMPMutableEndpoint`): it registers the blocks as static, enables the endpoint, sends on its source and hands its callback what is written to its destination as `UmpDestination` does, and disables the endpoint on drop. A block CoreMIDI cannot carry (a MIDI-CI version, a number that is not its position) is `Error::Block`, and before macOS 15 the creation is `Error::UmpEndpointUnsupported`. |
//! | `ump_endpoints` | the UMP Endpoints and Function Blocks CoreMIDI reports (macOS 15's `MIDIUMPEndpoint`, Objective-C classes bound through `objc2-core-midi`), as `tutti-ump` values; `UmpEndpointsError::Unsupported` before macOS 15, where the classes do not exist, and `Exception` for an Objective-C exception CoreMIDI raises (as seen when no MIDI server is reachable) |
//! | `UniqueId`, `OsStatus` | CoreMIDI's `kMIDIPropertyUniqueID` and the `OSStatus` of a failed call |
//! | `SharedClient` | the process's one CoreMIDI client: only `SharedClient::start` makes it (a second start fails with `Error::ClientExists`), it retries the status CoreMIDI returns while its server exits, for a bounded time, delivers each `Notification` to the closure it was started with, and is what the virtual endpoints and ports are created on, so the process has this one client |
//! | `Notification`, `changes_devices` | a client notification by its `messageID`, and whether it changes the device list |
//! | `sources`, `destinations`, `find_source`, `find_destination`, `Endpoint` | the sources and destinations CoreMIDI lists now, each owning its object reference privately with its `UniqueId` read from that same object, its name and its protocol; `find_*` takes one by `UniqueId` and checks the object's type and id (`FindError`), so a list that shifted fails and never yields another device |
//! | `InputPort` | a native-UMP input port connected to an `Endpoint` (a source) on the `SharedClient`: it makes a state for each delivered list and hands its callback that state with each UMP message of each packet with its time as an `Option<Duration>` since the host clock's origin, as `UmpDestination` does, counts a truncated tail it drops, and disconnects on drop |
//! | `OutputPort` | an output port on the `SharedClient` that sends a list of `(Option<Duration>, Ump)` to an `Endpoint` (a destination) in one event list in a `tutti_ump::Protocol`; `None` is delivery now, a time is held until then, and a packet that does not fit the list is `Error::EventListFull` |
//! | `run_notifications` | runs the calling thread's run loop, which is where the `SharedClient` started on that thread receives its notifications; where the loop has no source it runs a `CFRunLoopTimer` that calls a closure every period, so the thread stays in the loop |
//! | `Timebase`, `host_ticks` | the Mach host clock CoreMIDI timestamps its packets with, and the ratio from its ticks to nanoseconds |
//!
//! Each `unsafe` block carries one `// SAFETY:` argument where it is introduced;
//! `undocumented_unsafe_blocks` and `missing_safety_doc` are denied. An event list
//! is built over a zeroed word buffer and passed to CoreMIDI by pointer, never as a
//! reference to a partly written `MIDIEventList`.
// Every `unsafe` block says why it is sound, and every `unsafe fn` what it
// asks of its caller.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![warn(unsafe_op_in_unsafe_fn)]

// The OS-free checks and decisions: tested everywhere, and used by `os`. Off
// macOS they build for their tests only, so what only `os` uses is unused there.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod blocks;
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod find;
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod notification;
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod unique_id;

// The one platform seam: the macOS half, or nothing.
#[cfg(target_os = "macos")]
mod os;
#[cfg(not(target_os = "macos"))]
mod os {}

// Empty off macOS, where `os` has nothing to export.
#[allow(unused_imports)]
pub use os::*;
