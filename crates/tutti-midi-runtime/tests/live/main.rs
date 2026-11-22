//! The live border: input inlets, the input node, tap and clock nodes, and their cost classes.

/// Aborts inside an `RtScope` or an `assert_no_alloc` region; inert elsewhere, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../common/mod.rs"]
mod common;

mod border_no_alloc;
mod cost_classes;
mod edge_nodes;
mod input_inlets;
