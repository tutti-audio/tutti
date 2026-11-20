#![doc = include_str!("../README.md")]
//! The unsafe half of the Linux MIDI backend. Every `unsafe` call
//! `tutti-midi-hardware` makes into alsa-lib's sequencer and UMP API lives here,
//! behind safe handles; the backend above it is `#![forbid(unsafe_code)]` and holds
//! the logic (the pump and watch threads, the device model).
//!
//! The crate sits one layer above `tutti-ump` and returns its types for what the
//! UMP spec defines (protocol, Function Block fields); a value the sequencer holds
//! that the spec does not define is that field's `Reserved`.
//!
//! Linux only, and only where alsa-lib is 1.2.10 or newer (the build script probes
//! it and sets `cfg(alsa_ump)`); on other targets, or with an older or missing
//! alsa-lib, the crate is empty and the backend takes its stub.
//!
//! ## What is here
//!
//! | | |
//! |---|---|
//! | `Client` | an open sequencer client in UMP MIDI-2.0 mode: it closes on drop, and ports, subscriptions, `read_event`, `send_to_subscribers`, `stamp_port` and `list_ports` are methods |
//! | `UmpEndpoint`, `UmpBlock` | what the sequencer records of a client's UMP Endpoint (the protocol in use as a `tutti_ump::Protocol`, the protocols offered as an `EndpointInfoNotification`) and its Function Blocks (a `FunctionBlockInfo` and a name), from `Client::ump_endpoint`; `Client::publish_ump_endpoint` records them for the client itself |
//! | `QueueClock` | the queue a stamped port reads its timestamps from, and the clock that says what time it is now |
//! | `Poller`, `ClientHandle` | a client and a stop descriptor it owns, waited on with `poll(2)`: `Client::into_poller`, then `wait` and `wait_for`; a `ClientHandle` sends on the client from another thread without keeping it open |
//! | `Event`, `Announced` | an event copied out of alsa-lib's buffer (a UMP message, an announcement or a notice), and what an announce-port event says |
//! | `Address`, `ClientId`, `PortId`, `PortCaps`, `PortKind` | a port's client and number, what it allows, and what it is |
//! | `Error`, `Operation` | the alsa-lib call that failed and the OS error it returned |
//!
//! The raw declarations are a private module (`sys`), with alsa-lib's C names so a
//! reader can grep its headers. `snd_seq_ump_event_t` mirrors a C struct whose last
//! member is a union, so its size and field offsets are measured against the real
//! header and re-checked by compile-time asserts.
//!
//! Each `unsafe` block carries one `// SAFETY:` argument where it is introduced;
//! `undocumented_unsafe_blocks` and `missing_safety_doc` are denied.
//!
//! ## Linking and `links`
//!
//! `alsa-sys` (through `cpal`) declares `links = "alsa"`, so this crate declares
//! `links = "asound"` and the two coexist. Both link `libasound`, which is
//! harmless. The key also carries the build script's probe result to dependents as
//! `DEP_ASOUND_ALSA_UMP` (alsa-lib >= 1.2.10) and `DEP_ASOUND_ALSA_UMP_CREATE`
//! (>= 1.2.13). With an older or missing alsa-lib the build warns and sets neither;
//! nothing is linked.
//!
//! ## Features
//!
//! `testing` adds hidden hooks that let a test stand in for a peer on the
//! sequencer: a legacy (non-UMP) client with its byte events (`Client::open_legacy`,
//! `Client::send_legacy_to_subscribers`, `LegacyEvent`), a rename
//! (`Client::rename`), a change of a port's capabilities (`Client::set_port_caps`)
//! the client's own id (`Client::id`), a hook run before each read
//! (`Client::set_read_hook`: a failing or a held sequencer) and the count of clients
//! opened (`clients_opened`). Off in a production build;
//! `tutti-midi-hardware` turns it on only through its `sequencer-tests` feature.
// The whole crate is the ALSA UMP backend's FFI: `build.rs` sets `alsa_ump` on
// Linux when alsa-lib is 1.2.10 or newer, and otherwise there is nothing to link.
#![cfg(all(target_os = "linux", alsa_ump))]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` block says why it is sound, and every `unsafe fn` what it
// asks of its caller.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![warn(unsafe_op_in_unsafe_fn)]

mod address;
mod client;
mod error;
mod event;
mod poll;
mod ports;
mod records;
mod sys;
mod ump;

pub use address::{Address, ClientId, PortCaps, PortId, PortKind};
pub use client::{Blocking, Client, QueueClock, Read, UmpVersion};
#[cfg(feature = "testing")]
pub use client::{LegacyEvent, clients_opened};
pub use error::{Error, Operation, Result};
pub use event::{Announced, Event};
pub use poll::{ClientHandle, Poller, Wait};
pub use ports::{PortEntry, Ports};
pub use ump::{UmpBlock, UmpEndpoint};
