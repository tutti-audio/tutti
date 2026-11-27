//! The hardware MIDI backends: the MIDI-1 wire into MIDI-CI, the input port polled on the audio thread, the loopback of the backends that have one (ALSA, Windows MIDI Services), and the output suite every backend with a target output passes.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod ci_wire;
mod hardware_poll_no_alloc;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(midi_backend = "midi_services", feature = "services-tests"),
    all(target_os = "macos", feature = "coremidi")
))]
mod listener;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(midi_backend = "midi_services", feature = "services-tests"),
    all(target_os = "macos", feature = "coremidi")
))]
mod listing;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(midi_backend = "midi_services", feature = "services-tests")
))]
mod loopback;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(midi_backend = "midi_services", feature = "services-tests"),
    all(target_os = "macos", feature = "coremidi")
))]
mod opening;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(midi_backend = "winmm", feature = "synth-tests"),
    all(midi_backend = "midi_services", feature = "services-tests")
))]
mod output;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(midi_backend = "winmm", feature = "synth-tests"),
    all(midi_backend = "midi_services", feature = "services-tests")
))]
#[cfg_attr(target_os = "linux", path = "target/alsa.rs")]
#[cfg_attr(midi_backend = "winmm", path = "target/winmm.rs")]
#[cfg_attr(midi_backend = "midi_services", path = "target/midi_services.rs")]
mod target;
#[cfg(all(target_os = "linux", feature = "sequencer-tests"))]
mod virtual_endpoint;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(midi_backend = "midi_services", feature = "services-tests"),
    all(target_os = "macos", feature = "coremidi")
))]
mod virtual_loopback;
#[cfg(any(
    all(midi_backend = "winmm", feature = "synth-tests"),
    all(midi_backend = "midi_services", feature = "services-tests")
))]
mod virtual_unsupported;
