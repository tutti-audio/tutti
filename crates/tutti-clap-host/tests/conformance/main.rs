//! CLAP host conformance: an in-process pluginval for our host.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../support/mod.rs"]
mod support;

mod clap_conformance;
mod clap_enumeration_holes;
mod clap_gui_lifecycle;
mod clap_host_extensions_conformance;
mod clap_params_state_conformance;
mod clap_refusal_conformance;
mod clap_threading_conformance;
mod clap_version_conformance;
