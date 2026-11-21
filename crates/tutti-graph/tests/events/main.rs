//! Events as ports, timestamped commands and the clocks a node reads.
//!
//! The binary installs `AllocDisabler`, so an allocation inside an `assert_no_alloc`
//! closure aborts.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod arrival_transport;
mod commands;
mod event_inlet;
mod event_loss;
mod event_ports;
mod event_tap;
mod events;
mod held_keys;
mod play_clock;
