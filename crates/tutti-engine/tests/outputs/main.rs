//! What the engine publishes beside its output: the heard tap and the master meter.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

mod heard_tap;
mod master_meter;
