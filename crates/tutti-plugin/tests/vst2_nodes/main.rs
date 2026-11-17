//! VST2: the in-process node (feature `vst2`), and the client refusing a VST2
//! file for out-of-process hosting (no feature).

#[cfg(feature = "vst2")]
#[path = "../support/param_list.rs"]
mod param_list;
#[cfg(feature = "vst2")]
#[path = "../support/param_reads.rs"]
mod param_reads;
mod vst2_in_process;
mod vst2_in_process_transport;
mod vst2_is_not_served;
