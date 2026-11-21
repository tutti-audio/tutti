//! A hosted CLAP plugin as a graph node (feature `clap`).
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

#[cfg(feature = "clap")]
#[path = "../support/clap_probe.rs"]
mod clap_probe;
#[cfg(feature = "clap")]
#[path = "../support/param_probe.rs"]
mod param_probe;
#[cfg(feature = "clap")]
#[path = "../support/param_reads.rs"]
mod param_reads;

// `assert_no_alloc` is inert unless `AllocDisabler` is the global allocator. It
// does nothing outside an `assert_no_alloc` closure, so the other tests here are
// unaffected.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod clap_automation;
mod clap_crash_recovery;
mod clap_editor_transport;
mod clap_event_bound;
mod clap_fork;
mod clap_live;
mod clap_node_no_alloc;
mod clap_pdc_alignment;
mod clap_ragged;
mod clap_rate_refused;
mod clap_transport;
mod clap_wake;
mod plugin_param_delivery;
mod plugin_param_delivery_no_alloc;
