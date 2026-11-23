//! The nodes' allocation gates: each node's audio-thread path, alone and through
//! an engine. The binary installs `RtAllocCheck`, so an allocation inside a node's
//! `process` run by `Direct` (a realtime `RtScope`) or inside an `assert_no_alloc` closure aborts.

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod harness;
mod rows;
mod rt_no_alloc;
mod rt_no_alloc_convolver;
mod rt_no_alloc_crossover;
mod rt_no_alloc_dynamics;
mod rt_no_alloc_engine;
mod rt_no_alloc_eq_filters;
mod rt_no_alloc_filter;
mod rt_no_alloc_filter_corners;
mod rt_no_alloc_modulate;
mod rt_no_alloc_modulated_reverb;
mod rt_no_alloc_modulation;
mod rt_no_alloc_physical;
mod rt_no_alloc_reflections;
mod rt_no_alloc_reverb;
mod rt_no_alloc_synthesis;
mod rt_no_alloc_utility;
mod spatial_rt_no_alloc;

#[test]
fn every_row_of_the_table_is_allocation_free() {
    for row in rows::ROWS {
        row();
        assert!(
            harness::take_peak() > 0.0,
            "a row's measured blocks rendered silence"
        );
    }
}
