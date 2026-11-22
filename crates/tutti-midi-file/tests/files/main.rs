//! The file codecs: a Standard MIDI File and a MIDI 2.0 Clip File, read, written and
//! converted to a playable timeline, each against its specification.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod authoring;
mod clip_model;
mod common;
mod division;
mod meter;
mod rules;
mod smf_model;
mod smf_wire;
mod support;
mod tick_at;
mod to_clip;
#[cfg(unix)]
mod write_mode;
mod writing;
