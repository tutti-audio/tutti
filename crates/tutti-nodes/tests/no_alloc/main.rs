//! The nodes' allocation gates: each node's audio-thread path, alone and through
//! an engine. The binary installs `RtAllocCheck`, so an allocation inside an
//! `assert_no_alloc` closure aborts.

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod rt_no_alloc;
mod rt_no_alloc_bit_crusher;
mod rt_no_alloc_comb_shifter;
mod rt_no_alloc_convolver;
mod rt_no_alloc_crossover;
mod rt_no_alloc_cut_filter;
mod rt_no_alloc_dc_blocker;
mod rt_no_alloc_diode_clipper;
mod rt_no_alloc_distance;
mod rt_no_alloc_dynamic_eq;
mod rt_no_alloc_dynamics;
mod rt_no_alloc_engine;
mod rt_no_alloc_envelope_filter;
mod rt_no_alloc_eq_filters;
mod rt_no_alloc_filter;
mod rt_no_alloc_filter_corners;
mod rt_no_alloc_granular;
mod rt_no_alloc_hysteresis;
mod rt_no_alloc_leveller;
mod rt_no_alloc_modal;
mod rt_no_alloc_modulate;
mod rt_no_alloc_modulated_reverb;
mod rt_no_alloc_modulation;
mod rt_no_alloc_physical;
mod rt_no_alloc_pitch_shifter;
mod rt_no_alloc_reflections;
mod rt_no_alloc_reverb;
mod rt_no_alloc_shimmer;
mod rt_no_alloc_synthesis;
mod rt_no_alloc_transient;
mod rt_no_alloc_tube_stage;
mod rt_no_alloc_utility;
mod rt_no_alloc_wavetable;
mod spatial_rt_no_alloc;
