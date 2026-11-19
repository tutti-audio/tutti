#![doc = include_str!("../README.md")]
//! OS MIDI I/O for the Tutti audio engine: enumerate endpoints, connect
//! them, send MIDI out and receive it back, through CoreMIDI (macOS), the
//! ALSA UMP sequencer (Linux) and Windows Multimedia (Windows). `tutti::midi` names its
//! device and profile types (feature `midi-hardware`).
//!
//! Events reach the wire as UMP words, not MIDI 1.0 bytes, so MIDI-2-only messages
//! (per-note controllers, per-note pitch bend, JR Timestamps) survive where the
//! device's Protocol carries them: an output is put in the device's wire form (the
//! OS's description, then what the UMP Stream negotiation settled), so a MIDI 1.0
//! endpoint is sent MIDI 1.0 and never type `0x4`, and JR Timestamps go only to a
//! device that receives them.
//!
//! ## Devices
//!
//! [`Devices`] lists the MIDI devices and keeps the list current while it lives:
//! `open()` starts a watch, which stops when the last `Devices` and [`DeviceEvents`]
//! over it are dropped. `subscribe()` gives a [`DeviceEvents`] of its own, so one
//! reader never takes another's events. Each device has a [`DeviceId`](tutti_midi_device::DeviceId), and
//! `open_input(&id, &ports)` returns an [`OpenInput`] that closes the connection
//! and retires its ring when dropped.
//! `resolve(&saved)` restores a remembered [`DeviceInfo`](tutti_midi_device::DeviceInfo) (see `tutti-midi-device`'s
//! `serde` feature): `Exact` is safe to bind, `SameName` is a lone device that
//! matches only by name (ALSA), `Ambiguous` lists candidates. An id names a device within a run:
//! its identity is what the OS lets survive a restart, and the ordinal after it only
//! tells alike devices apart (unplug two identical keyboards and plug them back in
//! the other order, and their ordinals swap).
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use tutti_midi_device::DeviceEvent;
//! use tutti_midi_hardware::Devices;
//!
//! let devices = Devices::open()?;
//! let events = devices.subscribe();
//! for device in events.initial() {
//!     println!("{}: {}", device.id, device.name);
//! }
//! // Blocks until a device comes or goes, or the timeout passes.
//! while let Ok(event) = events.recv_timeout(Duration::from_secs(5)) {
//!     match event {
//!         DeviceEvent::Added(device) => println!("plugged in: {}", device.name),
//!         DeviceEvent::Removed(device) => println!("gone: {}", device.name),
//!         DeviceEvent::Changed(device) => println!("changed: {}", device.name),
//!     }
//! }
//! # Ok::<(), tutti_midi_hardware::MidiHardwareError>(())
//! ```
//!
//! - **Linux:** a thread subscribes to the sequencer's `System:Announce` port, so a
//!   device that vanishes is noticed when the kernel says so. A [`DeviceId`](tutti_midi_device::DeviceId) is the
//!   client and port name plus an ordinal for identical devices; the client number
//!   is a handle to the current connection and not part of it. A client number the
//!   kernel hands to a new device straight after an old one exits is a new device,
//!   told by the exit announcement. On ALSA the name is the identity, so a renamed
//!   device is `Removed` and then `Added` under the new name, which closes a port
//!   held on it; `Changed` is for backends whose identity survives a rename.
//! - **macOS:** `tutti-coremidi-ffi`'s `SharedClient` runs its run loop on one thread
//!   for setup notifications (the ports still use a second client, until they
//!   move to that crate), and each [`Devices`] registers with it. A
//!   [`DeviceId`](tutti_midi_device::DeviceId) is the endpoint's `MIDIUniqueID`. Untested by the author.
//! - If the watch ends on its own, [`DeviceEvents::recv_timeout`] says why, once the events the watch reported are taken, and the list
//!   stays as it was.
//! - **Windows:** Windows Multimedia has no notification, so the watch lists the
//!   ports once a second and reports the difference; a device shows within about
//!   that long. A [`DeviceId`](tutti_midi_device::DeviceId) is the device interface path the driver reports,
//!   or the product name and ids when it reports none, with the direction.
//! - **Linux without alsa-lib 1.2.10:** the list is empty and stays so, as
//!   enumeration is; opening reports [`MidiHardwareError::Unsupported`].
//!
//! A [`DeviceId`](tutti_midi_device::DeviceId) built on the UMP Product Instance Id is in the types, but no
//! backend reads one yet: alsa-lib exposes none, and CoreMIDI's unique id stands in.
//!
//! ## Virtual endpoints
//!
//! A [`VirtualSource`] is an endpoint this process publishes for other
//! applications to read from, and a [`VirtualDestination`] one they write to; a
//! [`VirtualSpec`] gives the name, the [`VirtualProtocol`] and the Function
//! Blocks. A destination delivers into an [`InputProducerHandle`], so its events
//! take the path a device input's do. Where the platform cannot publish what the
//! spec asks, opening says so: [`MidiHardwareError::Unsupported`] where there is no backend
//! or the API has no virtual ports (Windows Multimedia),
//! [`MidiHardwareError::VirtualBlocksUnsupported`] for Function Blocks on macOS, where an
//! endpoint carries a name and a protocol only. On Linux the sequencer is
//! opened as a MIDI 2.0 client when the kernel has that mode and as a MIDI 1.0
//! client otherwise; the endpoint reports the protocol it got.
//!
//! ## Quick start
//!
//! [`MidiSession`] is the whole OS edge. Inbound events never pass *through* it:
//! each opened input is a port of the session's [`HardwareMidiInputs`]
//! (`session.ports()`), an inlet made by the [`InputInlets`](tutti_midi_runtime::InputInlets) the session was
//! opened over, and the device's own thread stamps each arrival with the frame
//! the engine's block clock puts it on and pushes it into that device's event
//! inlet. The graph reads the inlets on their frames (the node
//! `connect_input` returns is the inlet's graph side, to be wired to a
//! [`MidiInputNode`](tutti_midi_runtime::MidiInputNode)), so the audio thread converts no time. The session only
//! owns the connection. Disconnecting drops the connection's
//! [`InputProducerHandle`] and retires its port.
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use tutti_midi_runtime::InputInlets;
//! use tutti_midi_hardware::{MidiHardwareOptions, MidiSession};
//! use tutti_graph::Offset;
//! use tutti_fixtures::midi;
//! use tutti_ump::{MidiChannel, NoteVelocity};
//!
//! // The inlets inbound events land in (1024 events each, placed 10 ms after
//! // they arrive), then a session over this platform's backend.
//! let inlets = InputInlets::new(tutti_graph::EventCapacity::of(1024), std::time::Duration::from_millis(10));
//! let mut session = MidiSession::open(Arc::clone(&inlets), MidiHardwareOptions::new())?;
//!
//! // The devices present now; capability (protocol, function blocks) is read from
//! // the OS.
//! for device in session.inputs() {
//!     println!("{}: {:?}", device.name, session.devices().capability(&device.id));
//! }
//!
//! // Connect by id when the choice matters (see "Matching by name" below).
//! if let Some(first) = session.inputs().first() {
//!     // The inlet node of a newly opened input is the graph's to add and wire.
//!     let _inlet = session.connect_input(&first.id)?;
//! }
//! // Or by name: one `DeviceQuery` form for connect and disconnect.
//! session.disconnect_input("keystep");
//!
//! // Outbound: the output is a value the caller owns; `queue` returns what the
//! // device took of them, always whole messages.
//! let mut output = session.connect_output("iac")?;
//! let note = midi::note_on(MidiChannel::FIRST, 60, 0x8000);
//! assert_eq!(output.queue(&[(Offset::ZERO, note)]).packets(), 1);
//! # Ok::<(), tutti_midi_hardware::MidiHardwareError>(())
//! ```
//!
//! ## Main types
//!
//! - [`Devices`]: the devices present, and the changes to them.
//! - [`MidiSession`]: connect and disconnect inputs; owns a [`Devices`] and the
//!   [`HardwareMidiInputs`] over the inlets it was opened with. `connect_input`,
//!   `disconnect_input` and `connect_output` take a [`DeviceQuery`] (a
//!   `&DeviceId` or a `&str`); `connect_output` returns the [`OpenOutput`], which
//!   the caller owns and sends through.
//! - [`HardwareMidiInputs`]: the open input ports, each named by its
//!   [`InputId`](tutti_midi_runtime::InputId) and the [`InputProducerHandle`] its driver thread pushes through,
//!   whose `push` answers a `Result` of [`Delivered`].
//! - [`DeviceInfo`](tutti_midi_device::DeviceInfo), [`DeviceId`](tutti_midi_device::DeviceId), [`DeviceEvent`](tutti_midi_device::DeviceEvent): what the list holds and reports;
//!   they live in `tutti-midi-device`, with the pure `DeviceTable` behind them.
//! - [`OsEndpoint`], [`EndpointId`](tutti_midi_device::EndpointId), what a backend enumerates.
//! - [`DeviceStream`], [`Basis`]: what a device's UMP Stream conversation settled
//!   (`MidiSession::poll` steps it, `MidiSession::stream` reads it).
//! - [`MidiEndpoints`]: the backend trait; [`MidiSession::open`] picks this
//!   platform's implementation.
//! - [`MidiHardwareError`]: what opening, connecting and sending can fail with.
//!
//! ## Matching by name picks a device you did not choose
//!
//! A name query ([`DeviceQuery`] from a `&str`) matches **case-insensitive
//! substring, first hit wins**, in id order. So `"iac"` matches `"IAC Driver
//! Bus 1"`, and a name matching two devices takes the one with the lowest id; for
//! a disconnect the match is over the open inputs, and the lowest id among them is
//! the one closed. Connect by [`DeviceId`](tutti_midi_device::DeviceId) when the choice matters, and
//! [`MidiSession::disconnect_all_inputs`] closes every one. A name that matches
//! nothing is [`MidiHardwareError::NoDeviceNamed`], listing the names it searched.
//!
//! ## Backends
//!
//! | Platform | Backend | Notes |
//! |---|---|---|
//! | macOS | CoreMIDI | `MIDIInputPortCreateWithProtocol` / `MIDISendEventList`. |
//! | Linux | ALSA UMP sequencer | Needs alsa-lib ≥ 1.2.10. Kernel ≥ 6.5 converts UMP↔legacy at the port boundary, so it works against MIDI 1.0 hardware. |
//! | Windows | Windows Multimedia (winmm) | MIDI 1.0 bytes, carried as MIDI 1.0 UMP and translated as a legacy ALSA port is. Polled device watch, no virtual ports. An output holds each event until its send time on a thread of its own. Windows MIDI Services (UMP-native) is not used. |
//! | Elsewhere | stub | Enumerates nothing, [`MidiHardwareError::Unsupported`] on open. |
//!
//! [`MidiSession::open`] picks one, once, at construction; nothing downstream learns
//! which OS it is talking to.
//!
//! **A build may have no backend, and it is not an error.** On Linux, the
//! `tutti-alsa-ffi` build script probes alsa-lib via `pkg-config`; below 1.2.10 (or with no alsa-lib) it emits a
//! `cargo:warning` naming the version and compiles the stub:
//! zero endpoints, and [`MidiHardwareError::Unsupported`] naming the reason. So code using this
//! crate compiles everywhere and simply enumerates nothing where there is no
//! backend.
//!
//! ## Features
//!
//! - `serde`: `Serialize` and `Deserialize` for [`MidiHardwareOptions`] and
//!   [`InputTiming`], so an app stores the options it opens with. Off by default.
//! - `testing`: exports `test_support::FakeBackend`, a device list with no OS
//!   behind it, for [`MidiSession::open_over`] in headless tests; its `plug` and
//!   `unplug` drive hot-plug. Off by default.
//! - `sequencer-tests`: the suites that run `Devices` against the real ALSA
//!   sequencer, with user-space clients as the devices. Linux only; needs the
//!   `snd-seq` module and a readable `/dev/snd/seq`. Off by default.
//! - `synth-tests`: the suites that open Windows' GS Wavetable Synth through
//!   the winmm backend. Windows only; needs an audio endpoint, which a machine
//!   with no sound hardware gets from a virtual audio driver. Off by default.
//!
//! ## Where it sits
//!
//! Builds on `tutti-ump` (the wire vocabulary and its decoded `Message`),
//! `tutti-midi-device` (devices, endpoints and their identity) and
//! `tutti-midi-runtime` (the graph's MIDI nodes); their items are named from
//! those crates. It does **not** include the file codecs: SMF and the
//! path-level Clip File codec are in `tutti-midi-file`, so a consumer that only
//! reads `.mid` files does not link CoreMIDI or ALSA. Re-exported by `tutti`.
//!
//! ## Examples
//!
//! `cargo run --example list_devices` lists the devices, then watches hot-plug
//! for ten seconds. The ALSA loopback suite (`sequencer-tests`) sends UMP out
//! through the kernel's `Midi Through` client and reads it back. **VirMIDI does
//! not echo** (writing to it feeds the raw-MIDI device, not that port's own
//! subscribers, so a send/receive pair on one virmidi port receives nothing),
//! and `Midi Through` is the kernel's actual loopback.
#![forbid(unsafe_code)]

// --- Framework-free hardware I/O core ---

// No `///` on these: a doc comment on a `pub mod` shadows the module's own
// `//!` and re-resolves its intra-doc links in this scope.
mod answer;
mod backend;
// Compiled everywhere so its tests run on every platform; only the native
// backends (ALSA, CoreMIDI) convert an OS block, so it is dead where the stub
// or winmm is the backend.
#[cfg_attr(
    any(midi_backend = "none", midi_backend = "winmm"),
    allow(dead_code, reason = "only a UMP backend converts OS blocks")
)]
mod capability;
mod ci;
mod devices;
mod endpoints;
mod error;
mod fault;
// Only a native backend's callback runs through it; the stub has none.
#[cfg_attr(
    midi_backend = "none",
    allow(dead_code, reason = "only a native backend has a driver callback")
)]
mod guard;
mod peer;
mod port;
mod process;
mod profiles;
mod properties;
mod sensing;
mod session;
// Only the backends with a clock of their own to read convert its timestamp.
#[cfg_attr(
    any(midi_backend = "none", midi_backend = "winmm"),
    allow(
        dead_code,
        reason = "only the ALSA and CoreMIDI backends convert a clock"
    )
)]
mod stamp;
mod stream;
#[cfg(test)]
mod testing;
mod virtual_endpoint;
mod wire;

// A backend with no OS behind it, for testing the session layer. See the
// module's own docs for why it is exported rather than `#[cfg(test)]`.
#[cfg(any(test, feature = "testing"))]
pub mod test_support;

pub use answer::{Answer, ReportEnd};
pub use capability::OsEndpoint;
pub use ci::{CiChange, CiPeerInfo, PeerId};
pub use devices::{
    ChangeSink, DeviceEvents, Devices, OpenInput, OpenOutput, Pending, RecvTimeoutError, SeedError,
    WatchFailure,
};
pub use endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
pub use error::{MidiHardwareError, MidiThread, NotFound, Result};
pub use fault::{Errno, ErrnoOutOfRange, InputFault};
pub use peer::{PeerError, PeerRef};
pub use port::{Delivered, HardwareMidiInputs, InputProducerHandle, PortInfo, StreamDrain};
pub use process::{ProcessChange, ReportAsk, Reporter};
pub use profiles::{
    ChannelCount, ChannelSwitcher, NotAChannelCount, PeerProfiles, ProfileError, ProfileSwitch,
    ProfilesChange, Switcher, WholeSwitcher,
};
pub use properties::{PropertyAsk, PropertyCall, PropertyChange, PropertyPeer};
pub use session::{Connected, DeviceQuery, MidiHardwareOptions, MidiSession, Polled, Presented};
pub use stream::{Basis, DeviceStream, InputTiming};
pub use virtual_endpoint::{VirtualDestination, VirtualProtocol, VirtualSource, VirtualSpec};

// --- Standard MIDI File codec: NOT here ---
//
// The file codecs live in `tutti-midi-file` and are deliberately *not*
// re-exported. Reading a `.mid` and talking to a MIDI port are different jobs,
// and a consumer that wants only the former should not link CoreMIDI. Depend
// on `tutti-midi-file` directly.

// This crate is OS MIDI I/O plus the value types a host drives.
