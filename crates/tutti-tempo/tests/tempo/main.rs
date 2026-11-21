//! The tempo map's analytic figures, its cell grid and its range contract, and
//! the audio-thread calls' allocation gate. The binary installs `RtAllocCheck`,
//! so an allocation inside an `assert_no_alloc` closure aborts.

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../common/mod.rs"]
mod common;

mod analytic;
mod cells;
mod from_beats;
mod no_alloc;
mod range;
mod stepped;
