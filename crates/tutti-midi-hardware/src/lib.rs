#![doc = include_str!("../README.md")]
//! OS MIDI I/O for the Tutti audio engine: enumerate endpoints, connect
//! them, send MIDI out and receive it back, through CoreMIDI (macOS), the
//! ALSA UMP sequencer (Linux) and Windows MIDI Services or Windows Multimedia (Windows). `tutti::midi` names its
//! device and profile types (feature `midi-hardware`).
//!
//! Events reach the wire as UMP words, not MIDI 1.0 bytes, so MIDI-2-only messages
//! (per-note controllers, per-note pitch bend, JR Timestamps) survive where the
//! device's Protocol carries them: an output is put in the device's wire form (the
//! OS's description, then what the UMP Stream negotiation settled), so a MIDI 1.0
//! endpoint is sent MIDI 1.0 and never type `0x4`, and JR Timestamps go only to a
//! device that receives them.
//!
//! ## Features
//!
//! A backend is a cargo feature, each a no-op off its OS: `alsa` (Linux, needing
//! alsa-lib 1.2.10 or later), `coremidi` (macOS), `midi-services` (Windows MIDI
//! Services) and `winmm` (Windows Multimedia). All but `winmm` are on by default, so
//! every platform keeps its native backend; `winmm` replaces `midi-services` on Windows
//! (with both on the backend is winmm), so only the app turns it on, for a PC with no MIDI
//! Services. With `default-features = false` the backend is a stub that lists no device
//! and fails to open one with [`MidiHardwareError::Unsupported`]. `build.rs` picks the
//! backend from the target OS and these features: there is no runtime fallback, and a
//! PC without the service fails to open with
//! [`MidiHardwareError::MidiServicesNotRunning`] or
//! [`MidiHardwareError::MidiServicesApiMissing`].
//! `serde` adds `Serialize` and `Deserialize` on `MidiHardwareConfig` and
//! `InputTiming`; `testing` exports `test_support::FakeBackend`; `sequencer-tests` and
//! `synth-tests` and `services-tests` gate the suites that need a real ALSA sequencer,
//! Windows' GS Wavetable Synth or the Windows MIDI service, and turn their backend on.
//!
//! ## Devices
//!
//! [`Devices`] lists the MIDI devices and keeps the list current while it lives:
//! `open()` starts a watch, which stops when the last `Devices` and [`DeviceEvents`]
//! over it are dropped. `subscribe()` gives a [`DeviceEvents`] of its own, so one
//! reader never takes another's events. Each device has a [`MidiDeviceId`](tutti_midi_device::MidiDeviceId), and
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
//! use futures_lite::StreamExt;
//! use tutti_midi_device::DeviceEvent;
//! use tutti_midi_hardware::Devices;
//!
//! # async fn watch() -> Result<(), Box<dyn std::error::Error>> {
//! let executor = tutti_platform::Executor::new();
//! let devices = Devices::open(&executor)?;
//! // The backend may need a moment to start (macOS makes CoreMIDI's client on a thread
//! // of its own): `start()` is a future the app awaits, on whatever executor it has,
//! // and the list is seeded when it resolves. Every other backend is ready at once.
//! devices.start().await?;
//! let mut events = devices.subscribe();
//! for device in events.initial() {
//!     println!("{}: {}", device.id, device.name);
//! }
//! // The events are a `Stream` the watch wakes when a device comes or goes; it ends once
//! // the watch has ended.
//! while let Some(event) = events.next().await {
//!     match event {
//!         DeviceEvent::Added(device) => println!("plugged in: {}", device.name),
//!         DeviceEvent::Removed(device) => println!("gone: {}", device.name),
//!         DeviceEvent::Changed(device) => println!("changed: {}", device.name),
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! - **Linux:** a thread subscribes to the sequencer's `System:Announce` port, so a
//!   device that vanishes is noticed when the kernel says so. A [`MidiDeviceId`](tutti_midi_device::MidiDeviceId) is the
//!   client and port name plus an ordinal for identical devices; the client number
//!   is a handle to the current connection and not part of it. A client number the
//!   kernel hands to a new device straight after an old one exits is a new device,
//!   told by the exit announcement. On ALSA the name is the identity, so a renamed
//!   device is `Removed` and then `Added` under the new name, which closes a port
//!   held on it; `Changed` is for backends whose identity survives a rename.
//! - **macOS:** `tutti-coremidi-ffi`'s `SharedClient` runs its run loop on one thread
//!   for setup notifications (the ports still use a second client, until they
//!   move to that crate), and each [`Devices`] registers with it. A
//!   [`MidiDeviceId`](tutti_midi_device::MidiDeviceId) is the endpoint's `MIDIUniqueID`. Untested by the author.
//! - If the watch ends on its own, the [`DeviceEvents`] stream ends once the events the watch reported are taken,
//!   [`DeviceEvents::failure`] says why, and the list stays as it was.
//! - **Windows (MIDI Services):** the service's endpoint watcher reports each endpoint
//!   that appears, leaves or changes, with no re-list. A
//!   [`MidiDeviceId`](tutti_midi_device::MidiDeviceId) is the endpoint's device interface
//!   path, and each endpoint is one duplex device. Each listing reads the endpoint's
//!   declared info and Function Blocks, so a change to either shows in the list.
//! - **Windows (winmm):** Windows Multimedia has no notification, so the watch lists the
//!   ports once a second and reports the difference; a device shows within about
//!   that long. A [`MidiDeviceId`](tutti_midi_device::MidiDeviceId) is the device interface path the driver reports,
//!   or the product name and ids when it reports none, with the direction.
//! - **Linux without alsa-lib 1.2.10:** the list is empty and stays so, as
//!   enumeration is; opening reports [`MidiHardwareError::Unsupported`].
//!
//! A [`MidiDeviceId`](tutti_midi_device::MidiDeviceId) built on the UMP Product Instance Id is in the types, but no
//! backend reads one yet: alsa-lib exposes none, and CoreMIDI's unique id stands in.
//!
//! ## Virtual endpoints
//!
//! A [`VirtualEndpoint`] is the one UMP Endpoint this process publishes for other
//! applications to connect to: they read what it sends and write what it
//! receives. A [`VirtualSpec`] gives the name, the Device Identity and Product
//! Instance Id, the [`VirtualProtocol`] and the Function Blocks. Opening one takes
//! the [`Started`] a [`Start`] resolved to and returns an
//! [`OpeningVirtualEndpoint`], which is polled with the current time and a
//! `MainThreadMarker` until it yields the endpoint (macOS retries a registration
//! CoreMIDI refuses, and `deadline()` is the next attempt's instant; the other
//! platforms are ready on the first poll). The endpoint is `!Send` and `!Sync` on every platform
//! (macOS's UMP Endpoint is a main-thread object); a [`VirtualSender`] sends from
//! another thread. What peers write is delivered into an [`InputProducerHandle`],
//! so its events take the path a device input's do. Where the platform cannot publish what the
//! spec asks, opening says so: [`MidiHardwareError::Unsupported`] where there is no backend
//! or the backend publishes no virtual ports (Windows Multimedia),
//! [`MidiHardwareError::VirtualBlocksUnsupported`] for Function Blocks on macOS before 15,
//! where an endpoint carries a name and a protocol only (from macOS 15 the endpoint is
//! a UMP Endpoint that declares its blocks and its identity; CoreMIDI numbers the blocks
//! itself in registration order and has no MIDI-CI version, so a block that needs either is [`MidiHardwareError::Os`] with the
//! refusal named). On Linux the sequencer is
//! opened as a MIDI 2.0 client when the kernel has that mode and as a MIDI 1.0
//! client otherwise; the endpoint reports the protocol it got. On Windows MIDI Services
//! the endpoint is a virtual device the service lists as one UMP Endpoint with the
//! declared Function Blocks, published on a session of its own and removed when the
//! endpoint is dropped.
//!
//! ## Starting
//!
//! A backend may have something to start before it can open an endpoint: on macOS
//! the process's one CoreMIDI client is made on a thread of the backend's own, and
//! CoreMIDI's server may be restarting for several seconds. That wait ends when the
//! backend's thread says so, so [`Devices::start`] and [`MidiSession::start`]
//! return a [`Start`], a `Send` [`Future`] that thread wakes, with no executor of
//! this crate behind it. It resolves to the [`Started`] that opening a port or a
//! virtual endpoint takes, so a port is never opened before the backend can open it.
//! A start that failed is [`StartFailed`] for every future
//! waiting on it; the device watches stay registered, and the next [`Start`] asks the
//! backend again and, when it succeeds, seeds them. Every other backend is ready on
//! the first poll.
//!
//! ## Background work
//!
//! The work that waits on the OS (an ALSA input's pump, the device watch, Active
//! Sensing, the legacy ports' poll) runs as futures on the [`Executor`](tutti_platform::Executor)
//! the session is opened with: the ALSA input and watch and each output's Active
//! Sensing each on a dedicated thread of it, the poll as a timer loop. Each
//! component holds its task and drops it to stop. A dedicated thread runs whether
//! or not the app drives the executor; the poll runs only while the app drives
//! it: in `tutti-session`, inside `update`.
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
//! use tutti_midi_hardware::{HardwareMidiInputs, MidiHardwareConfig, MidiSession};
//! use tutti_graph::Offset;
//! use tutti_fixtures::midi;
//! use tutti_ump::{MidiChannel, NoteVelocity};
//!
//! # async fn play() -> Result<(), Box<dyn std::error::Error>> {
//! // The inlets inbound events land in (1024 events each, placed 10 ms after
//! // they arrive), then a session over this platform's backend.
//! let inlets = InputInlets::new(tutti_graph::EventCapacity::of(1024), std::time::Duration::from_millis(10));
//! let executor = tutti_platform::Executor::new();
//! let mut session = MidiSession::open(&executor, HardwareMidiInputs::new(Arc::clone(&inlets)), MidiHardwareConfig::new())?;
//! // Awaited before any port opens: the proof it resolves to is what opening takes.
//! let started = session.start().await?;
//!
//! // The devices present now; capability (protocol, function blocks) is read from
//! // the OS.
//! for device in session.inputs() {
//!     println!("{}: {:?}", device.name, session.devices().capability(&device.id));
//! }
//!
//! // A device is connected by its id. Names are not unique, so finding one by
//! // name is the app's own search over the listing.
//! let keystep = session.inputs().into_iter().find(|d| d.name == "Keystep Pro");
//! if let Some(keystep) = keystep {
//!     // The inlet node of a newly opened input is the graph's to add and wire.
//!     let _inlet = session.connect_input(started, &keystep.id)?;
//!     session.disconnect_input(&keystep.id)?;
//! }
//!
//! // Outbound: the output is a value the caller owns; `queue` returns what the
//! // device took of them, always whole messages.
//! let iac = session.outputs().into_iter().find(|d| d.name == "IAC Driver Bus 1");
//! let mut output = session.connect_output(started, &iac.expect("an IAC bus").id)?;
//! let note = midi::note_on(MidiChannel::FIRST, 60, 0x8000);
//! assert_eq!(output.queue(&[(Offset::ZERO, note)]).packets(), 1);
//! # Ok(())
//! # }
//! ```
//!
//! ## Main types
//!
//! - [`Devices`]: the devices present, and the changes to them.
//! - [`MidiSession`]: connect and disconnect inputs; owns a [`Devices`] and the
//!   [`HardwareMidiInputs`] over the inlets it was opened with. `connect_input`,
//!   `disconnect_input` and `connect_output` take a `&MidiDeviceId`; `connect_output` returns the [`OpenOutput`], which
//!   the caller owns and sends through.
//! - [`HardwareMidiInputs`]: the open input ports, each named by its
//!   [`InputId`](tutti_midi_runtime::InputId) and the [`InputProducerHandle`] its driver thread pushes through,
//!   whose `push` answers a `Result` of [`Delivered`].
//! - [`DeviceInfo`](tutti_midi_device::DeviceInfo), [`MidiDeviceId`](tutti_midi_device::MidiDeviceId), [`DeviceEvent`](tutti_midi_device::DeviceEvent): what the list holds and reports;
//!   they live in `tutti-midi-device`, with the pure `DeviceTable` behind them.
//! - [`OsEndpoint`], [`EndpointId`](tutti_midi_device::EndpointId), what a backend enumerates.
//! - [`DeviceStream`], [`Basis`]: what a device's UMP Stream conversation settled
//!   (`MidiSession::poll` steps it, `MidiSession::stream` reads it).
//! - [`MidiEndpoints`]: the backend trait; [`MidiSession::open`] picks this
//!   platform's implementation.
//! - [`MidiHardwareError`]: what opening, connecting and sending can fail with.
//!
//! ## Backends
//!
//! | Platform | Backend | Notes |
//! |---|---|---|
//! | macOS | CoreMIDI | `MIDIInputPortCreateWithProtocol` / `MIDISendEventList`. |
//! | Linux | ALSA UMP sequencer | Needs alsa-lib ≥ 1.2.10. Kernel ≥ 6.5 converts UMP↔legacy at the port boundary, so it works against MIDI 1.0 hardware. |
//! | Windows | Windows MIDI Services | UMP words end to end, each endpoint one duplex device. The service's endpoint watcher feeds the list, each endpoint is described by the info, Function Blocks and protocol the service declares, and virtual endpoints are the service's virtual devices. An output is a send-only connection that hands each packet to the service at its send time; a send more than 20 s ahead is refused and counted once as a loss. A PC without the service fails to open with a named error. |
//! | Windows (`winmm` feature) | Windows Multimedia (winmm) | MIDI 1.0 bytes, carried as MIDI 1.0 UMP and translated as a legacy ALSA port is. Polled device watch, no virtual ports. An output holds each event until its send time on a thread of its own. Replaces Windows MIDI Services, for a PC that has none. |
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
//! - `serde`: `Serialize` and `Deserialize` for [`MidiHardwareConfig`] and
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
//! - `services-tests`: the suites that open the diagnostic loopback endpoints of
//!   Windows MIDI Services. Windows only; needs the MIDI service and
//!   `Windows.Devices.Midi2.dll` beside the test binary. Off by default.
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
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

// --- Framework-free hardware I/O core ---

// No `///` on these: a doc comment on a `pub mod` shadows the module's own
// `//!` and re-resolves its intra-doc links in this scope.
mod answer;
mod backend;
// The decision `build.rs` makes, compiled here so its table is a unit test.
#[cfg(test)]
#[path = "../backend_choice.rs"]
mod backend_choice;
// Compiled everywhere so its tests run on every platform; only the UMP backends
// (ALSA, CoreMIDI, Windows MIDI Services) convert an OS block, so it is dead where
// the stub or winmm is the backend.
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
// Only the ALSA, Windows Multimedia and Windows MIDI Services callbacks run through it: the
// stub has none, and CoreMIDI's port catches a panic in its own.
#[cfg_attr(
    any(midi_backend = "none", midi_backend = "coremidi"),
    allow(
        dead_code,
        reason = "only the ALSA, Windows Multimedia and Windows MIDI Services backends use it"
    )
)]
mod guard;
mod peer;
mod port;
mod process;
mod profiles;
mod properties;
mod sensing;
mod session;
mod start;
// Only the backends with a clock of their own to read convert its timestamp.
#[cfg_attr(
    any(
        midi_backend = "none",
        midi_backend = "winmm",
        midi_backend = "midi_services"
    ),
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
    ChangeSink, DeviceEvents, Devices, OpenInput, OpenOutput, Pending, SeedError, WatchFailure,
};
pub use endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
pub use error::{MidiHardwareError, MidiThread, NotFound, Result};
pub use fault::{Errno, ErrnoOutOfRange, InputFault};
pub use peer::{PeerRef, PeerRequestError};
pub use port::{
    Delivered, HardwareMidiInputs, InputProducerHandle, PortInfo, StaleInput, StreamDrain,
};
pub use process::{ProcessChange, ReportAsk, ReportInquiry, Reporter};
pub use profiles::{
    ChannelCount, ChannelSwitcher, NotAChannelCount, PeerProfiles, ProfileError, ProfileSwitch,
    ProfilesChange, Switcher, WholeSwitcher,
};
pub use properties::{PropertyAsk, PropertyCall, PropertyChange, PropertyPeer};
pub use session::{Connected, MidiHardwareConfig, MidiSession, Polled, Presented};
pub use start::{Start, StartFailed, Started};
pub use stream::{Basis, DeviceStream, InputTiming};
pub use virtual_endpoint::{
    OpeningVirtualEndpoint, VirtualEndpoint, VirtualProtocol, VirtualSender, VirtualSpec,
};

// --- Standard MIDI File codec: NOT here ---
//
// The file codecs live in `tutti-midi-file` and are deliberately *not*
// re-exported. Reading a `.mid` and talking to a MIDI port are different jobs,
// and a consumer that wants only the former should not link CoreMIDI. Depend
// on `tutti-midi-file` directly.

// This crate is OS MIDI I/O plus the value types a host drives.
