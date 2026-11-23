//! The instrument contract against `PolySynth`, the SoundFont unit and a hosted
//! plugin, and the instruments' main pair and refusals. The binary installs
//! `RtAllocCheck`, so an allocation inside an `assert_no_alloc` closure aborts.
//!
//! The plugin subject needs a built `plugin-server`, and fails without it.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a test binary reports on its own output"
)]

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[macro_use]
mod instrument_support;

mod f64_session;
mod main_pair;
mod plugin;
mod polysynth;
// Loading a plugin file, as a test does where an app runs the load.
#[path = "../support/loading.rs"]
mod loading;
mod refusal;
mod soundfont;
mod soundfont_rates;
