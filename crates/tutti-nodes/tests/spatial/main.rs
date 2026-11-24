//! The panner nodes: HRTF and VBAP.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod hrir_sphere;
mod hrtf_panner;
mod hrtf_render;
mod spatial_contract;
mod spatial_cost_classes;
mod spatial_fan_in;
mod spatial_param_fork;
mod vbap_bed;
mod vbap_energy_sweep;
mod vbap_grid;
