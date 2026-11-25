//! The translation between MIDI 1.0 and MIDI 2.0: the stateless conversions,
//! `Midi1Translator` and the stateful `Midi2Translator`.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod channel_voice;
mod midi1;
mod midi1_translator;
mod midi2_translator;
mod to_midi1;
mod to_midi2;
