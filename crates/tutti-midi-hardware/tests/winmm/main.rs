//! The Windows backend against the Windows this runs on: what a CI runner has
//! (no MIDI input, the built-in GS Wavetable Synth output). Windows-only; a
//! missing synth is a failure with its name, never a skip. Opening the synth
//! needs an audio endpoint, which a hosted runner lacks: those tests are
//! `synth-tests`' and run on a Windows machine with sound.

#[cfg(windows)]
mod gs_synth;
#[cfg(all(windows, feature = "synth-tests"))]
mod synth_open;
