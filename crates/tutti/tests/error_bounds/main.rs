//! Every error type the umbrella reaches is an `Error` a caller can box and move.
//!
//! `?` into `Box<dyn Error + Send + Sync>` or anyhow's error type needs
//! `Error + Send + Sync + 'static`; the check is the bound, so a type that gains
//! a non-`Send` field stops compiling here. A feature-gated type is held where
//! the umbrella exposes it.
//!
//! The functions are never called: the compiler checks each bound where the
//! function is written, so the failure is a build error, not a test result.
#![allow(dead_code)]

use std::error::Error;

fn err<T: Error + Send + Sync + 'static>() {}

fn types_errors_are_boxable() {
    use tutti::types::{
        BeatsPerCycleOutOfRange, BipolarOutOfRange, BpmOutOfRange, InvalidBeatsPerBar,
        InvalidMaxBlock, InvalidNoteValue, LayoutError, LoopRangeError, MetresOutOfRange,
        SampleRateOutOfRange, SpeedOutOfRange, TopologyError, UnitParamOutOfRange,
    };
    err::<BeatsPerCycleOutOfRange>();
    err::<BpmOutOfRange>();
    err::<TopologyError>();
    err::<BipolarOutOfRange>();
    err::<InvalidBeatsPerBar>();
    err::<InvalidMaxBlock>();
    err::<InvalidNoteValue>();
    err::<LayoutError>();
    err::<LoopRangeError>();
    err::<MetresOutOfRange>();
    err::<SampleRateOutOfRange>();
    err::<SpeedOutOfRange>();
    err::<UnitParamOutOfRange>();
}

#[cfg(feature = "midi")]
fn midi_errors_are_boxable() {
    use tutti::midi::{NotOnMidiScale, NoteNumberOutOfRange};
    err::<NotOnMidiScale>();
    err::<NoteNumberOutOfRange>();
}

fn buffers_errors_are_boxable() {
    use tutti::types::{BufferError, FoldError, SinkFailed};
    err::<BufferError>();
    err::<FoldError>();
    err::<SinkFailed>();
}

#[cfg(feature = "session")]
fn session_graph_errors_are_boxable() {
    use tutti::session::{
        BusError, CloseError, CommitError, MissingSource, MixError, ModeError, PassError,
    };
    err::<BusError>();
    err::<CloseError>();
    err::<CommitError>();
    err::<MissingSource>();
    err::<MixError>();
    err::<ModeError>();
    err::<PassError>();
}

#[cfg(feature = "session")]
fn session_errors_are_boxable() {
    err::<tutti::Error>();
}

#[cfg(all(feature = "session", feature = "device"))]
fn device_errors_are_boxable() {
    err::<tutti::session::RestartError>();
}

fn modulation_errors_are_boxable() {
    err::<tutti::modulation::ProgramCompileError>();
    err::<tutti::modulation::ProgramError>();
}

#[cfg(feature = "session")]
fn session_modulation_errors_are_boxable() {
    err::<tutti::session::Problem>();
}

#[cfg(feature = "sampler")]
fn sampler_errors_are_boxable() {
    err::<tutti::sampler::VoiceNodeError>();
}

#[cfg(all(feature = "session", feature = "plugin"))]
fn recovery_errors_are_boxable() {
    err::<tutti::session::RecoveryFailure>();
    err::<tutti::session::ReloadCause>();
}
