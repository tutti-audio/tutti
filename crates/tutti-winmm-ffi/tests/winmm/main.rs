//! The winmm calls against the Windows this runs on: what a CI runner has
//! (no MIDI input, the built-in GS Wavetable Synth output). Every test here is
//! Windows-only, and a missing synth is a failure with its name, never a skip.
//! Opening the synth needs an audio endpoint: those tests are `synth-tests`', and
//! CI installs a virtual audio driver before it runs them.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

#[cfg(windows)]
mod listing;
#[cfg(windows)]
mod sleeper;
#[cfg(all(windows, feature = "synth-tests"))]
mod synth_open;
