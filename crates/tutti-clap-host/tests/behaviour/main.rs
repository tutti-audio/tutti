//! What the CLAP host does with a plugin: audio, events, parameters, bundles and event translation.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

#[path = "../support/mod.rs"]
mod support;

// `assert_no_alloc` is inert unless `RtAllocCheck` is the global allocator. It
// does nothing outside an `assert_no_alloc` closure, so the other tests here are
// unaffected.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod clap_audio_correctness;
mod clap_editor_instance;
mod clap_event_lists;
mod clap_midi_groups;
mod clap_multi_plugin_bundle;
mod clap_param_gain;
mod clap_ports_rescan;
mod clap_process_no_alloc;
mod clap_thread_pool;
mod clap_traits;
mod unit_tests;
