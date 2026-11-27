//! The hardware MIDI backends: the MIDI-1 wire into MIDI-CI, the input port polled on the audio thread, the ALSA loopback, and the output suite every backend with a target output passes.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[cfg(all(target_os = "linux", feature = "sequencer-tests"))]
mod alsa_loopback;
mod ci_wire;
mod hardware_poll_no_alloc;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(target_os = "macos", feature = "coremidi")
))]
mod listener;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(target_os = "macos", feature = "coremidi")
))]
mod listing;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(target_os = "macos", feature = "coremidi")
))]
mod opening;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(windows, feature = "synth-tests")
))]
mod output;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(windows, feature = "synth-tests")
))]
#[cfg_attr(target_os = "linux", path = "target/alsa.rs")]
#[cfg_attr(windows, path = "target/winmm.rs")]
mod target;
#[cfg(all(target_os = "linux", feature = "sequencer-tests"))]
mod virtual_endpoint;
#[cfg(any(
    all(target_os = "linux", feature = "sequencer-tests"),
    all(target_os = "macos", feature = "coremidi")
))]
mod virtual_loopback;
#[cfg(all(windows, feature = "synth-tests"))]
mod virtual_unsupported;
