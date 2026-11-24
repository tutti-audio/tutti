//! The in-process CLAP node forked, on the process's main thread.
//!
//! A fork of an in-process plugin is made on the main thread only
//! (`MainThreadMarker::new` asks the OS), and the default harness runs every
//! test on a thread it spawns: this target sets `harness = false` and owns
//! `main()` (`tutti_fixtures::main_thread_cases`).
//!
//! **A fork starts from the host's writes no block has taken.** CLAP's
//! controller only queues a write for the Processor, so a state saved right
//! after the write does not hold it: the fork gets it from the host's own record
//! of what the plugin was not handed. The probe applies its `Gain` parameter to
//! every output sample (`(input + 1.0) · gain`, silence in), so the fork's
//! samples show the gain it started from.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

use std::sync::Arc;
use tutti_buffers::{ChannelLayout, MaxBlock, Planes, PlanesMut};
use tutti_clap_host::ClapLoaded;
use tutti_fixtures::main_thread;
use tutti_graph::{ForkTarget, GraphBuilder, Prepare, RenderMode};
use tutti_plugin_host::load_client;
use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig, render_graph};
use tutti_units::{Normalized, ParamId, SampleRate, Samples};

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// The probe's applied gain (`GAIN_PARAM_ID` in the plugin), and its range in
/// dB. Mirrored rather than imported: this crate links no probe rlib.
const GAIN: ParamId = ParamId::new(77);
const GAIN_DB_MIN: f64 = -60.0;
const GAIN_DB_MAX: f64 = 12.0;
const GAIN_DB: f64 = -6.0;

/// The linear amplitude the probe multiplies by at `db`, as the probe computes it.
fn amplitude(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// **A parameter written with no block rendered is the fork's starting value.**
fn a_fork_starts_from_the_hosts_undrained_write() {
    // The probe adds its channel tag to its input and applies its gain. Set
    // before the plugin loads, on the one thread this target runs.
    for (key, value) in [
        ("TUTTI_CLAP_PROBE_RENDER_MODE", "1"),
        ("TUTTI_CLAP_PROBE_APPLY_GAIN", "1"),
    ] {
        // SAFETY: nothing else in this process reads the environment yet.
        unsafe { std::env::set_var(key, value) };
    }
    let (client, handle) = load_client::<ClapLoaded, f32>(
        main_thread(),
        &tutti_fixtures::detached_work(),
        &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
        SampleRate::of(SAMPLE_RATE),
    )
    .expect("the reference plugin loads in process");
    let stereo = ChannelLayout::from_count(2);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, stereo);
    let (plugin, _controls) = g.add_with_controls(client.bind(tutti_fixtures::tempo()));
    g.pipe_output(plugin);
    let prepare = Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(BLOCK)),
    );
    let live = g.renderer(prepare).expect("the graph compiles");
    handle
        .send_parameter(
            GAIN,
            Normalized::clamped((GAIN_DB - GAIN_DB_MIN) / (GAIN_DB_MAX - GAIN_DB_MIN)),
        )
        .expect("a declared parameter");

    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate::of(SAMPLE_RATE),
        ..Default::default()
    }));
    let (_fork_ed, mut fork_exec) = live
        .editor()
        .fork(
            ForkTarget::MainOutput,
            prepare.with_render_mode(RenderMode::Offline),
        )
        .expect("the plugin forks");
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    for _ in 0..8 {
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        render_graph(
            &*timeline,
            &mut fork_exec,
            Planes::default(),
            PlanesMut::new(&mut outs).unwrap(),
        );
    }
    assert_eq!(
        out[0].last().copied(),
        Some(amplitude(GAIN_DB)),
        "the fork renders at the gain written before it, not the default"
    );
    drop(handle);
}

fn main() {
    tutti_fixtures::main_thread_cases(tutti_fixtures::cases![
        a_fork_starts_from_the_hosts_undrained_write,
    ]);
}
