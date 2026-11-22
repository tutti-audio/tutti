//! What the engine publishes beside its output: the heard tap and the main output meter.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

mod heard_tap;
mod main_output_meter;
