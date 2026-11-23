//! What a step that drives a plugin which answers in time costs the audio
//! thread: percentiles of the working steps' wall time over real
//! out-of-process probes.
//!
//! The `real_plugin_pressure` test asserts that no step waits for a stalled
//! plugin; this reports what a working step costs, which a test never asserts.
//! Each answering plugin follows a stalled one: an answering plugin wakes its
//! server, and a server that shares the host's core runs during the host's
//! next step, so every step after an answering plugin pays for it in cache and
//! switches. Following a stalled probe, whose server sleeps through the block,
//! keeps that out of the figure.
//!
//! A percentile harness rather than a criterion group: the cost is dominated by
//! the OS scheduler, so the spread matters more than a mean with outliers
//! rejected. Each block waits for the answering plugins' answers before the
//! next starts, and only the answering plugins' `run` is timed.
//!
//! Needs `cargo build -p tutti-plugin-server` first, and the `clap` feature:
//!
//! ```text
//! cargo build -p tutti-plugin-server
//! cargo bench -p tutti-plugin --features clap --bench pressure
//! ```

#![cfg(feature = "clap")]
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a benchmark reports its figures on its own output"
)]

#[path = "../tests/support/clap_probe.rs"]
mod clap_probe;
#[path = "../tests/support/pressure_rig.rs"]
mod pressure_rig;

use pressure_rig::{BLOCK, Planes, fill_sine, load_half_stalled};
use std::time::Duration;
use tutti_platform::Instant;

/// Blocks run before timing, so the pipeline is carrying audio.
const WARM_UP: usize = 64;
/// Timed blocks per count.
const BLOCKS: usize = 500;

fn main() {
    for pairs in [1usize, 2, 4] {
        let (mut units, _handles) = load_half_stalled(pairs);
        let channels = units
            .iter()
            .map(|u| u.inputs().max(u.outputs()).max(1))
            .max()
            .unwrap_or(2);
        let mut input = Planes::new(channels);
        let mut output = Planes::new(channels);
        let mut took: Vec<Duration> = Vec::with_capacity(BLOCKS * pairs);
        for block in 0..WARM_UP + BLOCKS {
            fill_sine(&mut input, channels, block);
            for unit in units.iter_mut() {
                unit.stage(BLOCK, &input);
            }
            for unit in units.iter_mut() {
                let start = Instant::now();
                unit.run(BLOCK);
                if !unit.stalled && block >= WARM_UP {
                    took.push(start.elapsed());
                }
            }
            for unit in units.iter_mut() {
                unit.read(BLOCK, &mut output);
            }
            for unit in units.iter().filter(|u| !u.stalled) {
                clap_probe::await_answer(&unit.controls);
            }
        }
        took.sort_unstable();
        let at = |per_mille: usize| took[(took.len() * per_mille / 1000).min(took.len() - 1)];
        println!(
            "{pairs} answering after {pairs} stalled: working step p10 {:?}  p25 {:?}  \
             p50 {:?}  p75 {:?}  p99 {:?}  ({} steps)",
            at(100),
            at(250),
            at(500),
            at(750),
            at(990),
            took.len()
        );
    }
}
