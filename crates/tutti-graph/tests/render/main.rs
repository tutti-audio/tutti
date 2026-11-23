//! The render oracles: graphs built from a small grammar beside the figure
//! they must render (`lti`), and random graphs through the compiler and its
//! verifier (`compile_random`).
//!
//! The binary installs `RtAllocCheck`, so an allocation inside a graph block
//! aborts.

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../support/generator.rs"]
mod generator;

mod compile_random;
mod lti;
