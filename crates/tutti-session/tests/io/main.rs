//! The session's I/O edges: devices and their restart, the recorder, the inlets and the taps.

// The device-free device tests (`ManualStreamDriver`): `device` is a feature,
// so this suite runs under it (CI's dark-features job runs the crate with every
// feature on; a workspace run does not turn it on).
// A session changes clock between the system clock and a sound card.
#[cfg(feature = "device")]
mod clock_switch;
#[cfg(feature = "device")]
mod device;
#[cfg(feature = "capture")]
mod device_input;
#[cfg(feature = "device")]
mod device_listing;
#[cfg(feature = "device")]
mod f64_device;
// An `f64` session's take.
#[cfg(all(feature = "capture", feature = "sampler"))]
mod f64_takes;
// A restart onto another rate, width or block, and the MIDI clock's re-rate.
#[cfg(all(feature = "device", feature = "midi"))]
mod device_midi_clock;
// A sink is sent at the time of each event's frame.
#[cfg(feature = "analyze")]
mod analyzer;
#[cfg(all(feature = "device", feature = "midi"))]
mod device_midi_out_timing;
#[cfg(feature = "device")]
mod device_restart;
// The recorder: its thread, and the pump over a tap and a decoded file.
mod event_inlet;
mod event_tap;
#[cfg(feature = "record")]
mod file_in_pump;
mod inlet;
#[cfg(feature = "record")]
mod recorder_thread;
// A node's audio-thread log, emitted as `tracing` events.
#[cfg(feature = "device")]
mod rt_log;
mod tap_close;
#[cfg(feature = "record")]
mod tap_to_wav_roundtrip;
// Takes: a recording placed on the graph's clock, punched, and reported.
#[cfg(feature = "capture")]
mod takes;
