//! Events as ports, timestamped commands and the clocks a node reads.
//!
//! The binary installs `RtAllocCheck`, so an allocation inside an `assert_no_alloc`
//! closure aborts.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod arrival_transport;
mod commands;
mod event_inlet;
mod event_loss;
mod event_ports;
mod event_tap;
mod events;
mod held_keys;
mod play_clock;
