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

// `RtAllocCheck` aborts inside an `RtScope` (a render prepared `Realtime`) or
// an `assert_no_alloc` closure and does nothing elsewhere, so the other tests
// here are unaffected.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

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
mod clap_read_after_write;
mod clap_transport;
mod clap_wake;
mod plugin_param_delivery;
mod plugin_param_delivery_no_alloc;
mod ramp_source;
