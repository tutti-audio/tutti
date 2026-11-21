//! The VST3 host against real and reference plugins: loading, event lists, MIDI groups and the FFI plugin.
//!
//! The binary's global allocator is armed (`RtAllocCheck`), so its no-alloc gates bite.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

#[path = "../support/active.rs"]
mod active;
#[path = "../support/probe.rs"]
mod probe;

// `assert_no_alloc` does nothing outside an `assert_no_alloc` closure, so the
// other tests here are unaffected.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

// Every test target is declared because `autotests = false` (see above);
// a new file under tests/ needs an entry here or it will not run.
mod class_choice;
mod integration_tests;
mod param_mapping;
mod vst3_event_lists;
mod vst3_ffi_plugin;
mod vst3_midi_groups;
