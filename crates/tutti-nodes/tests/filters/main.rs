//! Filters and dynamics: responses, block independence and non-finite input.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a test binary reports on its own output"
)]

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod delay_echoes;
mod envelope_filter;
mod filter_block_independence;
mod filter_responses;
mod ladder_crossover_node;
mod non_finite_constructors;
mod non_finite_controls;
mod non_finite_inputs;
mod phaser_allpass;
mod rbj_cookbook;
mod width_generic;
