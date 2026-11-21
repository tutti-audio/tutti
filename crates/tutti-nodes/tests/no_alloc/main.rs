//! The nodes' allocation gates: each node's audio-thread path, alone and through
//! an engine. The binary installs `AllocDisabler`, so an allocation inside an
//! `assert_no_alloc` closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod rt_no_alloc;
mod rt_no_alloc_bit_crusher;
mod rt_no_alloc_comb_shifter;
mod rt_no_alloc_convolver;
mod rt_no_alloc_crossover;
mod rt_no_alloc_cut_filter;
mod rt_no_alloc_dc_blocker;
mod rt_no_alloc_dynamic_eq;
mod rt_no_alloc_dynamics;
mod rt_no_alloc_engine;
mod rt_no_alloc_envelope_filter;
mod rt_no_alloc_eq_filters;
mod rt_no_alloc_filter;
mod rt_no_alloc_filter_corners;
mod rt_no_alloc_leveller;
mod rt_no_alloc_modulate;
mod rt_no_alloc_modulated_reverb;
mod rt_no_alloc_modulation;
mod rt_no_alloc_pitch_shifter;
mod rt_no_alloc_reflections;
mod rt_no_alloc_reverb;
mod rt_no_alloc_shimmer;
mod rt_no_alloc_transient;
mod rt_no_alloc_utility;
mod spatial_rt_no_alloc;
