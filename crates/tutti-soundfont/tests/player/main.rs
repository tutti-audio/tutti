//! The SoundFont unit's audio: messages, controllers, sends, universal SysEx and the reference font.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod message;
mod player;
mod sends;
mod soundfont_unit_audio;
#[path = "../support/mod.rs"]
mod support;
mod universal;
