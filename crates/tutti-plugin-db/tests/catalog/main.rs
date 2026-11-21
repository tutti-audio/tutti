//! `Catalog` against real files and the reference plugin: scans, registrations,
//! measurements, links and the probe's failures.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

mod scan;
