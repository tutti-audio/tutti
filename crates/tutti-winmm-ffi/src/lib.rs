#![doc = include_str!("../README.md")]
//! The unsafe half of the Windows MIDI backend. Every `unsafe` call
//! `tutti-midi-hardware` makes into winmm lives here, behind safe handles; the
//! backend above it is `#![forbid(unsafe_code)]` and holds the logic
//! (enumeration identity, byte to UMP packing, timestamp placement).
//!
//! The crate is empty off Windows, except `lifetime`, the open-callback-close
//! protocol with no OS call, which is tested and modelled on every target.
//!
//! ## What is here
//!
//! | | |
//! |---|---|
//! | `ports`, `Port`, `InputPort`, `OutputPort` | the ports winmm lists now, each owning its list position privately, with what the driver reports of it, the device interface path that names it across a replug, and its friendly name (which the ports of one multi-port device share, so the backend above tells them apart by the driver's name) |
//! | `MidiIn` | an open input, opened from an `InputPort`: short messages and SysEx buffers delivered to a closure on the driver's thread, each with winmm's millisecond stamp; closing it returns every buffer first |
//! | `MidiOut` | an open output, opened from an `OutputPort`: a short message, or a SysEx sent whole |
//! | `waitable_timer`, `TimerWaker`, `TimerWaiter` | a high-resolution waitable timer and a wake signal: the one waiter's wait ends at a time or when any clone of the waker wakes it |
//!
//! Each `unsafe` block carries one `// SAFETY:` argument where it is introduced;
//! `undocumented_unsafe_blocks` and `missing_safety_doc` are denied.
//!
//! ## Features
//!
//! - `synth-tests`: the suite that opens the GS Wavetable Synth. It needs an
//!   audio endpoint, which a machine with no sound hardware gets from a virtual
//!   audio driver. Off by default.
// Every `unsafe` block says why it is sound, and every `unsafe fn` what it
// asks of its caller.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![warn(unsafe_op_in_unsafe_fn)]

// The OS-free protocol: tested everywhere, and used by `os`'s input.
#[cfg(any(windows, test))]
mod lifetime;

// The one platform seam: the Windows half, or nothing.
#[cfg(windows)]
mod os;
#[cfg(not(windows))]
mod os {}

// Empty off Windows, where `os` has nothing to export.
#[allow(unused_imports)]
pub use os::*;
