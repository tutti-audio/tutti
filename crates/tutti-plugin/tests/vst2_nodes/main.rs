//! VST2: the in-process node (feature `vst2`).

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[cfg(feature = "vst2")]
#[path = "../support/param_list.rs"]
mod param_list;
#[cfg(feature = "vst2")]
#[path = "../support/param_reads.rs"]
mod param_reads;
mod vst2_in_process;
mod vst2_in_process_transport;
