//! Forking a hosted plugin **by state transfer**, against the reference CLAP
//! plugin in a real `plugin-server`.
//!
//! What a fork promises (`host::node::fork`'s module docs): a fresh instance of
//! the same plugin, carrying the live instance's saved state, in a process of
//! its own, rendering deterministically offline — and the live instance is only
//! asked for its state. Each test below pins one of those against the probe's
//! applied `Gain` parameter, which is the observable that separates a parameter
//! the plugin *holds* from one a host merely remembers: it is only visible in
//! the samples.
//!
//! The probe runs in `TagPassthrough` with gain applied, so output channel 0 of
//! block `k` is `(input_{k-1} + 1.0) · gain` — one block late, the declared
//! pipeline latency. Inputs vary per block and per sample so that a block
//! delivered to the wrong place, or twice, cannot pass.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

use crate::clap_probe;
use crate::ramp_source::{RampSource, ramp};
use tutti_buffers::{Planes, PlanesMut};
use tutti_graph::EdgeId;
// The live side waits for the server's answer to each block; the fork is *not*: it waits for each
// block, which is the property under test.
use clap_probe::{BLOCK, ProbeEnv, Rig, SAMPLE_RATE, load_probe, load_probe_with, render};
use tutti_buffers::MaxBlock;
use tutti_fixtures::machine_lock;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::{
    Editor, ForkError, ForkFaultKind, ForkTarget, IntoNode, Prepare, RenderMode, Renderer,
};
use tutti_graph::{OutputPort, PortSource};
use tutti_plugin::{Bound, PluginClient};
use tutti_plugin_host::PluginHandle;
use tutti_plugin_ipc::{IpcConfig, PluginRenderFault};
use tutti_plugin_types::PluginForkError;
use tutti_plugin_types::Timeouts;
use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig, render_graph};
use tutti_ump::{Address, Message, NoteNumber, NoteVelocity, Voice2};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{SampleRate, Samples};

/// The probe's applied gain (`GAIN_PARAM_ID` in the plugin), and its range in
/// dB. Mirrored rather than imported: this crate links no probe rlib.
const GAIN: ParamId = ParamId::new(77);
const GAIN_DB_MIN: f64 = -60.0;
const GAIN_DB_MAX: f64 = 12.0;

/// `probe_tag(0, 0)`, added to channel 0 in `TagPassthrough`.
const TAG_P0C0: f32 = 1.0;

/// The normalized value that lands on `db`.
fn gain_at(db: f64) -> Normalized {
    Normalized::clamped((db - GAIN_DB_MIN) / (GAIN_DB_MAX - GAIN_DB_MIN))
}

/// The linear amplitude the probe multiplies by at `db`, computed as the probe
/// does (`10^(dB/20)` in f64, then to f32): the tests compare with it exactly.
fn amplitude(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// Input sample `i` of block `b`: distinct everywhere, small, exact in f32.
fn input(b: usize, i: usize) -> f32 {
    ((b * BLOCK + i) % 997) as f32 / 1024.0
}

/// What block `b` of channel 0 must read at `db`: block `b - 1`'s input,
/// tagged and scaled.
fn expected(b: usize, db: f64) -> Vec<f32> {
    (0..BLOCK)
        .map(|i| (input(b - 1, i) + TAG_P0C0) * amplitude(db))
        .collect()
}

/// Drive block `b` through `unit` and return output channel 0.
fn drive(unit: &mut Rig, b: usize) -> Vec<f32> {
    let out = unit.run(BLOCK, |_, i| input(b, i));
    out.into_iter().next().expect("the probe has outputs")
}

/// Drive `blocks` blocks through the live unit, each answered before the next; return each block's
/// channel 0, silent (`None`) where the pipeline had nothing to collect.
fn drive_live(unit: &mut Rig, blocks: usize) -> Vec<Option<Vec<f32>>> {
    (0..blocks)
        .map(|b| {
            let out = unit
                .run_answered(BLOCK, |_, i| input(b, i))
                .into_iter()
                .next()
                .expect("the probe has outputs");
            out.iter().any(|&s| s != 0.0).then_some(out)
        })
        .collect()
}

/// Drive `blocks` unpaced blocks through a fork.
fn drive_fork(unit: &mut Rig, blocks: usize) -> Vec<Vec<f32>> {
    (0..blocks).map(|b| drive(unit, b)).collect()
}

fn env() -> ProbeEnv {
    ProbeEnv::new()
        .render_mode(render::TAG_PASSTHROUGH)
        .gain_enabled(true)
}

/// **A fork renders what the live instance renders, from the live instance's
/// state** — a gain set on the live plugin before the fork is in the fork's
/// samples — and it renders every block, unpaced, because an offline fork
/// waits for its subprocess.
///
/// Every block the live pipeline collected is compared bit for bit with the
/// fork's block at the same index (both are one block late), and the fork's are
/// checked against the arithmetic too.
#[test]
fn a_fork_renders_like_the_live_instance_from_its_state() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    probe
        .client
        .handle()
        .send_parameter(GAIN, gain_at(-6.0))
        .expect("a declared parameter");

    let fork = probe
        .client
        .fork_instance(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect("the probe forks");
    assert!(
        probe.client.plugin().id().is_some(),
        "the live plugin names itself"
    );
    assert_eq!(
        fork.plugin().id(),
        probe.client.plugin().id(),
        "the fork is the live plugin"
    );

    const BLOCKS: usize = 40;
    let mut fork = Rig::offline(fork, SAMPLE_RATE, BLOCK);
    let forked = drive_fork(&mut fork, BLOCKS);
    assert!(forked[0].iter().all(|&s| s == 0.0), "one block of latency");
    for (b, block) in forked.iter().enumerate().skip(1) {
        assert_eq!(block, &expected(b, -6.0), "fork block {b}");
    }

    let mut live = Rig::new(
        probe.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
        SAMPLE_RATE,
        BLOCK,
    );
    let lived = drive_live(&mut live, BLOCKS);
    let collected: Vec<_> = lived
        .iter()
        .enumerate()
        .filter_map(|(b, o)| o.as_ref().map(|o| (b, o)))
        .collect();
    assert!(
        collected.len() >= 5,
        "the live pipeline collected {} of {BLOCKS} blocks; too few to compare",
        collected.len()
    );
    for (b, block) in collected {
        assert_eq!(block, &forked[b], "live block {b} differs from the fork's");
    }
}

/// **Rendering a fork does not touch the live instance**, with both rendering
/// on two threads over the same span of time: the live output stays exactly
/// what its own input and gain make, and its saved state is what it was
/// before the fork. And **a live parameter change after the fork does not
/// reach the fork**: the live gain moves to -15 dB, the fork stays at -6.
///
/// The overlap is enforced, not hoped for: both threads start from a barrier,
/// the fork keeps rendering until the live run is done, and the test asserts
/// the fork rendered blocks while the live run was still going.
#[test]
fn a_fork_and_the_live_instance_do_not_reach_each_other() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let handle: PluginHandle = probe.handle.clone();
    probe
        .client
        .handle()
        .send_parameter(GAIN, gain_at(-6.0))
        .expect("a declared parameter");

    let fork = probe
        .client
        .fork_instance(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect("the probe forks");
    let state_at_fork = handle
        .save_state(tutti_plugin_types::StateUse::Project)
        .expect("the probe saves");

    // After the fork: the live instance moves to -15 dB.
    probe
        .client
        .handle()
        .send_parameter(GAIN, gain_at(-15.0))
        .expect("a declared parameter");

    const MIN_FORK_BLOCKS: usize = 40;
    let start = Arc::new(Barrier::new(2));
    let live_done = Arc::new(AtomicBool::new(false));
    let fork_thread = {
        let (start, live_done) = (Arc::clone(&start), Arc::clone(&live_done));
        std::thread::spawn(move || {
            let mut fork = Rig::offline(fork, SAMPLE_RATE, BLOCK);
            start.wait();
            let (mut blocks, mut during_live) = (Vec::new(), 0usize);
            while blocks.len() < MIN_FORK_BLOCKS || !live_done.load(Ordering::SeqCst) {
                if !live_done.load(Ordering::SeqCst) {
                    during_live += 1;
                }
                blocks.push(drive(&mut fork, blocks.len()));
            }
            (blocks, during_live)
        })
    };

    let mut live = Rig::new(
        probe.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
        SAMPLE_RATE,
        BLOCK,
    );
    start.wait();
    let lived = drive_live(&mut live, 40);
    live_done.store(true, Ordering::SeqCst);
    let (forked, during_live) = fork_thread.join().expect("the fork thread renders");
    assert!(
        during_live >= 10,
        "the fork rendered only {during_live} blocks while the live run was going; \
         the two did not overlap"
    );

    let mut collected = 0;
    for (b, block) in lived.iter().enumerate() {
        if let Some(block) = block {
            assert_eq!(block, &expected(b, -15.0), "live block {b}");
            collected += 1;
        }
    }
    assert!(
        collected >= 5,
        "the live pipeline collected {collected} blocks"
    );
    for (b, block) in forked.iter().enumerate().skip(1) {
        assert_eq!(block, &expected(b, -6.0), "fork block {b}");
    }

    // The live state moved only by its own parameter change: put the gain
    // back and it is byte for byte what it was at the fork. Through the
    // handle, the path a host has once the node is in a graph.
    handle
        .send_parameter(GAIN, gain_at(-6.0))
        .expect("a declared parameter");
    assert_eq!(
        handle
            .save_state(tutti_plugin_types::StateUse::Project)
            .expect("the probe saves"),
        state_at_fork,
        "rendering the fork changed the live instance's state"
    );
}

/// **A fresh instance that refuses the live state is a named error**, and
/// through the graph a `ForkError::Source` naming the node, whose cause is
/// that same `PluginForkError::LoadState` — never a fork at the plugin's
/// defaults.
///
/// The refusal is armed after the live instance loaded, so only the fork's
/// server (spawned later, inheriting the environment) refuses.
#[test]
fn a_state_the_fresh_instance_refuses_is_a_named_fork_error() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let _refuse = ProbeEnv::new().refuse_state_load(true);

    let err = probe
        .client
        .fork_instance(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect_err("the fresh instance refuses the state");
    assert!(
        matches!(err, PluginForkError::LoadState(_)),
        "expected LoadState, got {err:?}"
    );

    let prepare = Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(BLOCK)),
    );
    let (mut editor, _exec) = Editor::new(prepare);
    let key = NodeKey(7);
    let _controls = editor.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    editor
        .wire_outputs(vec![PortSource::Node(OutputPort::new(key, 0))])
        .unwrap();
    match editor.fork(
        ForkTarget::Node(key),
        prepare.with_render_mode(RenderMode::Offline),
    ) {
        Err(ForkError::Source { key: at, cause }) => {
            assert_eq!(at, key);
            assert!(
                matches!(
                    cause.downcast_ref::<PluginForkError>(),
                    Some(PluginForkError::LoadState(_))
                ),
                "the cause is the plugin's: {cause:?}"
            );
        }
        other => panic!("expected ForkError::Source, got {:?}", other.err()),
    }
}

/// **A fork is a duplicate, and a project save is a project's.** The plugin
/// takes a state only saved for and loaded in `CLAP_STATE_CONTEXT_FOR_DUPLICATE`
/// (2), so the fork succeeds only if the live instance saved it for a
/// duplicate and the fresh one restored it as one, through the server's wire;
/// the project's own save, from the same live instance, is tagged 3.
#[test]
fn a_fork_is_saved_and_restored_as_a_duplicate_and_a_project_save_is_not() {
    let _lock = machine_lock();
    let _env = env().load_only_context(2);
    let probe = load_probe(SAMPLE_RATE);

    // The probe stamps the context after its 4-byte magic.
    const CONTEXT_BYTE: usize = 4;
    let project = probe
        .handle
        .save_state(tutti_plugin_types::StateUse::Project)
        .expect("the project save");
    assert_eq!(project[CONTEXT_BYTE], 3, "a project save is FOR_PROJECT");

    probe
        .client
        .fork_instance(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect("the fork saved for a duplicate and restored as one");
}

/// **A plugin that cannot save its state cannot be forked**, and says so
/// before any fresh instance is started.
#[test]
fn a_plugin_that_cannot_save_its_state_is_a_named_fork_error() {
    let _lock = machine_lock();
    let _env = env().refuse_state_save(true);
    let probe = load_probe(SAMPLE_RATE);
    let err = probe
        .client
        .fork_instance(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect_err("the live instance refuses to save");
    assert!(
        matches!(err, PluginForkError::SaveState(_)),
        "expected SaveState, got {err:?}"
    );
}

/// **Through the graph**: a bound `PluginClient` inserted into an editor is
/// forkable, and the forked graph renders the plugin at the live instance's
/// gain on the offline timeline — the path an export takes.
/// With its inputs unwired the probe renders its tag, scaled.
#[test]
fn a_graph_holding_a_plugin_forks_and_renders_offline() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    probe
        .client
        .handle()
        .send_parameter(GAIN, gain_at(-6.0))
        .expect("a declared parameter");

    let prepare = Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(BLOCK)),
    );
    let (mut editor, _exec) = Editor::new(prepare);
    let key = NodeKey(1);
    let _controls = editor.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    editor
        .wire_outputs(vec![PortSource::Node(OutputPort::new(key, 0))])
        .unwrap();

    let (fork_ed, fork_exec) = editor
        .fork(
            ForkTarget::Node(key),
            prepare.with_render_mode(RenderMode::Offline),
        )
        .expect("a graph holding a plugin forks");
    let out = Renderer::new(fork_ed, fork_exec).render(BLOCK * 8);
    let want = TAG_P0C0 * amplitude(-6.0);
    assert!(
        out[0][..BLOCK].iter().all(|&s| s == 0.0),
        "pipeline latency"
    );
    assert!(
        out[0][BLOCK..].iter().all(|&s| s == want),
        "every later frame is the tag at -6 dB ({want}); got {:?}",
        &out[0][BLOCK..BLOCK + 4]
    );
    drop(probe.handle);
}

/// Render `blocks` blocks of the offline fork of the plugin through the
/// graph; return the forked editor, to ask its health.
fn render_fork_of(client: PluginClient, blocks: usize) -> Editor {
    let prepare = Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(BLOCK)),
    );
    let (mut editor, _exec) = Editor::new(prepare);
    let key = NodeKey(3);
    let _controls = editor.insert(
        key,
        "plugin",
        client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    editor
        .wire_outputs(vec![PortSource::Node(OutputPort::new(key, 0))])
        .unwrap();
    let (fork_ed, fork_exec) = editor
        .fork(
            ForkTarget::Node(key),
            prepare.with_render_mode(RenderMode::Offline),
        )
        .expect("forks");
    assert_eq!(fork_ed.fork_health(), Ok(()), "healthy before rendering");
    let mut renderer = Renderer::new(fork_ed, fork_exec);
    renderer.render(BLOCK * blocks);
    let (fork_ed, _exec) = renderer.into_parts();
    fork_ed
}

/// **A fork whose server dies mid-render is a named fault** — not a silent
/// render that reports success. The fork's server (only: the switch is armed
/// after the live one loaded) aborts on its 8th block; the forked editor
/// reports `Crashed` for the plugin's key with a `PluginRenderFault::Crashed`
/// cause. (That the render does not wait out a budget is the bridge's rule, in
/// `tutti-plugin-ipc`.)
#[test]
fn a_fork_whose_server_dies_mid_render_is_a_crashed_fault() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let _crash = ProbeEnv::new().crash_on_block(8);

    let fork_ed = render_fork_of(probe.client, 40);
    let fault = fork_ed.fork_health().expect_err("the fork crashed");
    assert_eq!(fault.key, NodeKey(3));
    assert_eq!(fault.kind, ForkFaultKind::Crashed, "{fault}");
    assert!(
        matches!(
            fault.cause.downcast_ref::<PluginRenderFault>(),
            Some(PluginRenderFault::Crashed { .. })
        ),
        "{fault:?}"
    );
}

/// **A fork whose server hangs mid-render is a named fault.** The fork's
/// server parks from its 5th block for good; with a 1 s budget the render ends
/// and the forked editor reports `TimedOut` with that budget. (That one budget
/// is spent per render, not per block, is the bridge's rule, in
/// `tutti-plugin-ipc`.)
#[test]
fn a_fork_whose_server_hangs_is_a_timed_out_fault() {
    let _lock = machine_lock();
    let _env = env();
    let config = IpcConfig {
        timeouts: {
            let mut t = Timeouts::default();
            t.call = Duration::from_secs(1);
            t
        },
        ..IpcConfig::new()
    };
    let probe = load_probe_with(config, SAMPLE_RATE);
    let _hang = ProbeEnv::new().block_from(5);

    let fork_ed = render_fork_of(probe.client, 30);
    let fault = fork_ed.fork_health().expect_err("the fork hung");
    assert_eq!(fault.kind, ForkFaultKind::TimedOut, "{fault}");
    assert!(
        matches!(
            fault.cause.downcast_ref::<PluginRenderFault>(),
            Some(PluginRenderFault::TimedOut { budget }) if *budget == Duration::from_secs(1)
        ),
        "{fault:?}"
    );
}

/// **A fork source whose live instance is gone says so**, without
/// launching anything: `PluginForkError::LiveGone`. The source holds the live
/// bridge weakly, so it cannot keep a dropped plugin alive either.
#[test]
fn a_fork_of_a_dropped_plugin_is_live_gone() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let parts = probe
        .client
        .bind(tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"))
        .into_parts();
    let source = parts.fork.expect("a plugin node is forkable");
    drop(parts.node);
    drop(parts.controls);
    drop(probe.handle);

    let cause = match source.fork(RenderMode::Realtime) {
        Ok(_) => panic!("forked a plugin that is gone"),
        Err(cause) => cause,
    };
    assert!(
        matches!(
            cause.downcast_ref::<PluginForkError>(),
            Some(PluginForkError::LiveGone)
        ),
        "{cause:?}"
    );
}

/// This process's direct children, read from `/proc` (every thread's list:
/// a child is listed under the thread that spawned it).
#[cfg(target_os = "linux")]
fn children() -> std::collections::BTreeSet<u32> {
    let mut out = std::collections::BTreeSet::new();
    for task in std::fs::read_dir("/proc/self/task").expect("procfs") {
        let path = task.expect("task").path().join("children");
        if let Ok(text) = std::fs::read_to_string(path) {
            out.extend(
                text.split_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok()),
            );
        }
    }
    out
}

/// **A dropped fork's server is reaped**, on every platform: its pid
/// (`PluginHandle::server_pid`) names no live process — not even a zombie —
/// once the fork and its handle are gone, while the live server's still
/// does. The probe is `clap_probe::is_alive` (`kill(pid, 0)` on Unix,
/// `GetExitCodeProcess` on Windows), shared with `real_plugin_pressure.rs`.
#[test]
fn a_dropped_forks_server_is_reaped() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let live_pid = probe.handle.server_pid().expect("a subprocess plugin");

    let fork = probe
        .client
        .fork_instance(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect("the probe forks");
    let handle = fork.handle();
    let pid = handle.server_pid().expect("the fork has its own server");
    assert_ne!(pid, live_pid, "a process of its own");
    assert!(clap_probe::is_alive(pid), "the fork's server runs");
    drop(handle);
    drop(fork);
    assert!(
        !clap_probe::is_alive(pid),
        "a dropped fork's server is reaped"
    );
    assert!(
        clap_probe::is_alive(live_pid),
        "the live server is untouched"
    );
}

/// **A fork that fails is reaped too** (`LoadState`: the fresh instance
/// started, then refused the state). Its pid never reaches the caller, so
/// this reads this process's children from `/proc` instead: Linux only
/// (`/proc/self/task/*/children`). The guard is the same code everywhere.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_forks_server_is_reaped() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let baseline = children();
    assert!(!baseline.is_empty(), "the live server is a child");

    let _refuse = ProbeEnv::new().refuse_state_load(true);
    let err = probe
        .client
        .fork_instance(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        )
        .err();
    assert!(
        matches!(err, Some(PluginForkError::LoadState(_))),
        "{err:?}"
    );
    assert_eq!(children(), baseline, "a failed fork's server is reaped");
}

/// A live graph holding the plugin at key 9 on the main output, to export from.
fn bound(client: PluginClient) -> PluginClient<Bound> {
    client.bind(
        tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
    )
}

fn live_graph(client: PluginClient<Bound>) -> (Editor, tutti_graph::Executor) {
    let (mut live, exec) = Editor::new(Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(BLOCK)),
    ));
    let key = NodeKey(9);
    let _controls = live.insert(
        key,
        "plugin",
        client,
    );
    live.wire_outputs(vec![PortSource::Node(OutputPort::new(key, 0))])
        .unwrap();
    (live, exec)
}

/// The main output of `live` forked offline at `rate`: the pair an export
/// assembles.
fn fork_pair(live: &Editor, rate: SampleRate) -> (Editor, tutti_graph::Executor) {
    live.fork(
        ForkTarget::MainOutput,
        tutti_export::RenderGraph::prepare(rate),
    )
    .expect("a graph holding a plugin forks")
}

/// [`fork_pair`], assembled as an export does.
fn fork_graph(live: &Editor, rate: SampleRate) -> tutti_export::RenderGraph {
    let (editor, executor) = fork_pair(live, rate);
    tutti_export::RenderGraph::assemble(editor, executor, tutti_export::Setup::new())
        .expect("forked together")
}

/// `config`'s render of `graph` into memory (a job and the planes' width) under
/// a control nobody cancels.
fn render_to_buffers(
    mut graph: tutti_export::RenderGraph,
    config: &(tutti_export::Job, tutti_buffers::ChannelLayout),
    clock: &dyn tutti_timeline::RenderClock,
) -> tutti_export::Result<tutti_export::Outcome<tutti_export::Rendered>> {
    tutti_export::render_one(
        &mut graph,
        &config.0,
        clock,
        tutti_export::Output::main_output(tutti_export::BufferTarget::new(config.1)),
        &mut tutti_export::Control::none(),
    )
    .map(|outcome| outcome.map(|produced| produced.into_buffers().expect("a buffers output")))
}

/// Export the plugin through `tutti_export` exactly as an export does: fork
/// the live graph offline (`fork_graph`), render half a second to
/// buffers.
fn export_through_a_fork(live: &Editor) -> tutti_export::Result<tutti_export::Rendered> {
    let rate = SampleRate::of(SAMPLE_RATE);
    let graph = fork_graph(live, rate);
    let config = (
        tutti_export::Job::new(
            rate,
            tutti_units::Seconds::new(0.5).to_samples(rate),
            Samples::new(0),
            tutti_export::TailPolicy::Frames(Samples::ZERO),
        )
        .expect("a render"),
        tutti_buffers::ChannelLayout::MONO,
    );
    render_to_buffers(graph, &config, &tutti_timeline::FrozenClock).map(|outcome| {
        outcome
            .into_finished()
            .expect("nothing cancels a control with no token")
    })
}

/// **An export through a fork whose plugin server dies, or hangs, fails with
/// a named error** — `Error::ForkFailed { key, kind, cause }` — instead of
/// returning half a second of mostly silence as a successful render. A healthy
/// fork exports fine (and audibly), so the check is not a blanket refusal.
#[test]
fn an_export_through_a_failing_fork_fails_by_name() {
    let _lock = machine_lock();
    let _env = env();
    let config = IpcConfig {
        timeouts: {
            let mut t = Timeouts::default();
            t.call = Duration::from_secs(1);
            t
        },
        ..IpcConfig::new()
    };
    let probe = load_probe_with(config, SAMPLE_RATE);
    probe
        .client
        .handle()
        .send_parameter(GAIN, gain_at(-6.0))
        .expect("a declared parameter");
    let (live, _exec) = live_graph(bound(probe.client));

    let healthy = export_through_a_fork(&live).expect("a healthy fork exports");
    assert!(healthy.planes()[0].iter().any(|&s| s != 0.0), "audible");

    let failed = |kind: ForkFaultKind| match export_through_a_fork(&live) {
        Err(tutti_export::RenderError::ForkFailed {
            key,
            kind: got,
            cause,
        }) => {
            assert_eq!(key, NodeKey(9));
            assert_eq!(got, kind, "{cause}");
            assert!(cause.downcast_ref::<PluginRenderFault>().is_some());
        }
        Err(other) => panic!("expected ForkFailed, got {other}"),
        Ok(_) => panic!("an export through a {kind:?} fork reported success"),
    };
    {
        let _crash = ProbeEnv::new().crash_on_block(8);
        failed(ForkFaultKind::Crashed);
    }
    let _hang = ProbeEnv::new().block_from(5);
    failed(ForkFaultKind::TimedOut);
}

/// **A clip's note reaches an exported plugin on its frame**, through
/// `tutti_export` as an export runs. The export's fork is prepared at the
/// export block (1024 frames), so its chunk is 1024. The probe's `Notes`
/// gate turns the note-on into its first non-zero frame, which lands at the
/// note's frame plus the pipeline's one chunk. The note is at frame 6000, on
/// neither a 64- nor a 1024-frame boundary.
#[test]
fn a_clip_note_reaches_an_exported_plugin_on_its_frame() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    let probe = load_probe(SAMPLE_RATE);
    // 120 BPM at 48 kHz: a beat is 24 000 frames, so beat 0.25 is frame 6000.
    const NOTE_FRAME: usize = 6_000;
    let (mut live, _exec) = live_graph(bound(probe.client));
    feed_note(&mut live, NodeKey(9), NOTE_FRAME);

    // The export's own timeline, rolling from beat 0, and its clock.
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate::of(SAMPLE_RATE),
        ..Default::default()
    }));
    let graph = fork_graph(&live, SampleRate::of(SAMPLE_RATE));
    // The pipeline holds one chunk, the export block. (The probe reports a
    // latency of its own in every mode but delays audio only in `Latency`.)
    let chunk = tutti_export::GRAPH_MAX_BLOCK;
    let config = (
        tutti_export::Job::new(
            SampleRate::of(SAMPLE_RATE),
            tutti_units::Seconds::new(0.25).to_samples(SampleRate::of(SAMPLE_RATE)),
            Samples::new(0),
            tutti_export::TailPolicy::Frames(Samples::ZERO),
        )
        .expect("a render"),
        tutti_buffers::ChannelLayout::MONO,
    );
    let rendered = render_to_buffers(graph, &config, timeline.as_ref())
        .expect("the fork exports")
        .into_finished()
        .expect("nothing cancels a control with no token");
    let onset = rendered.planes()[0].iter().position(|&s| s != 0.0);
    assert_eq!(
        onset,
        Some(NOTE_FRAME + chunk.get()),
        "the note sounds on its frame, one chunk late"
    );
}

/// Feed the plugin at `plugin` from a clip node holding a chord of `n`
/// notes, started on frame `on` and ended on frame `off`.
fn feed_chord(live: &mut Editor, plugin: NodeKey, n: u8, on: usize, off: usize) {
    use tutti_graph::{EventEdge, EventInputPort, EventOutputPort};
    use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
    use tutti_ump::{MidiChannel, MidiGroup};
    use tutti_units::Beat;

    let events = (0..n).flat_map(|k| {
        // Notes 0..100 on channel 0, 100..200 on channel 1.
        let (g, c, key) = (MidiGroup::FIRST, MidiChannel::of(k / 100), k % 100);
        [
            TimedMidiEvent::new(
                Beat::new(on as f64 / 24_000.0),
                Message::midi2(
                    Address::new(g, c),
                    Voice2::note_on(NoteNumber::of(key), NoteVelocity::MAX),
                ),
            )
            .unwrap(),
            TimedMidiEvent::new(
                Beat::new(off as f64 / 24_000.0),
                Message::midi2(
                    Address::new(g, c),
                    Voice2::note_off(NoteNumber::of(key), NoteVelocity::MIN),
                ),
            )
            .unwrap(),
        ]
    });
    let clip = NodeKey(1);
    live.insert(clip, "clip", MidiClipNode::new(events));
    live.connect_events(
        EventInputPort::new(plugin, 0),
        EventEdge::Direct(EventOutputPort::new(clip, 0)),
    )
    .unwrap();
}

/// **A chord bigger than the plugin's chunk of MIDI ends, and the export
/// says what was lost.** 200 notes start on frame 5 200 and end on frame
/// 5 900 of one 1,024-frame chunk (5 120..6 144): 400 events against the
/// chunk's 256. The chunk keeps every note-off: the first 56 fit behind the
/// 200 note-ons, and each of the other 144 takes the place of the note-on of
/// its own key. 144 note-ons never reach the plugin, and that is the loss:
/// by default the export fails with it, counted where the policy sees it;
/// allowed, it is reported with the same figure. The plugin's gate (open
/// while any note is held) opens one chunk after the note-ons and closes one
/// chunk after the note-offs, on exactly those frames, and stays shut: no
/// note is left sounding.
///
/// Before, the chunk kept the first 256 events: all 200 note-ons and 56
/// note-offs, the gate stayed open to the end of the file, and the export
/// returned `Ok`.
#[test]
fn a_chord_larger_than_a_chunk_ends_and_the_export_counts_the_loss() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    let render = |policy: tutti_export::EventLossPolicy| {
        let probe = load_probe(SAMPLE_RATE);
        let (mut live, _exec) = live_graph(bound(probe.client));
        feed_chord(&mut live, NodeKey(9), 200, 5_200, 5_900);
        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            sample_rate: SampleRate::of(SAMPLE_RATE),
            ..Default::default()
        }));
        let graph = fork_graph(&live, SampleRate::of(SAMPLE_RATE));
        let config = (
            tutti_export::Job::new(
                SampleRate::of(SAMPLE_RATE),
                tutti_units::Seconds::new(0.25).to_samples(SampleRate::of(SAMPLE_RATE)),
                Samples::new(0),
                tutti_export::TailPolicy::Frames(Samples::ZERO),
            )
            .expect("a render")
            .event_loss(policy),
            tutti_buffers::ChannelLayout::MONO,
        );
        render_to_buffers(graph, &config, timeline.as_ref()).map(|outcome| {
            outcome
                .into_finished()
                .expect("nothing cancels a control with no token")
        })
    };
    let err = render(tutti_export::EventLossPolicy::Fail).expect_err("144 note-ons were lost");
    assert!(
        matches!(err, tutti_export::RenderError::EventsDropped { count: 144 }),
        "{err:?}"
    );
    let rendered = render(tutti_export::EventLossPolicy::Allow).expect("allowed");
    assert_eq!(rendered.dropped_events(), 144);
    let gate = &rendered.planes()[0];
    let chunk = tutti_export::GRAPH_MAX_BLOCK.get();
    assert_eq!(gate.iter().position(|&s| s != 0.0), Some(5_200 + chunk));
    assert_eq!(
        gate.iter().rposition(|&s| s != 0.0),
        Some(5_900 + chunk - 1),
        "the gate closes on the note-offs' frame and stays shut"
    );
}

/// Feed the plugin at `plugin` from a clip node holding one note-on at frame
/// `frame` of a 120 BPM timeline at [`SAMPLE_RATE`] (a beat is 24 000
/// frames).
fn feed_note(live: &mut Editor, plugin: NodeKey, frame: usize) {
    use tutti_graph::{EventEdge, EventInputPort, EventOutputPort};
    use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
    use tutti_ump::MidiChannel;
    use tutti_units::Beat;

    let clip = NodeKey(1);
    live.insert(
        clip,
        "clip",
        MidiClipNode::new([TimedMidiEvent::new(
            Beat::new(frame as f64 / 24_000.0),
            Message::midi2(
                MidiChannel::FIRST,
                Voice2::note_on(NoteNumber::of(60), NoteVelocity::MAX),
            ),
        )
        .unwrap()]),
    );
    live.connect_events(
        EventInputPort::new(plugin, 0),
        EventEdge::Direct(EventOutputPort::new(clip, 0)),
    )
    .unwrap();
}

/// **A chunk that begins inside a block, or inside a render pass, still
/// places its notes on their frames.** A fork prepared at 480 frames ships
/// 480-frame chunks, rendered in 400-frame blocks: the chunk from frame 6 240
/// begins 240 frames into the block from 6 000 and ends inside the next. The
/// note at 6 300 is in that chunk; the probe's gate opens one chunk after it.
///
/// Rendered twice: in whole blocks, and in 64-frame passes from each block's
/// start with the timeline moved between them (what the engine did while a
/// `Legacy` node was in the graph, and what any host rendering short calls
/// does: the chunk from 6 240 then begins 48 frames into the pass from
/// 6 192).
#[test]
fn a_clip_note_in_a_chunk_that_begins_mid_block_lands_on_its_frame() {
    for passes in [false, true] {
        let _lock = machine_lock();
        let _env = ProbeEnv::new().render_mode(render::NOTES);
        let probe = load_probe(SAMPLE_RATE);
        const CHUNK: usize = 480;
        const BLOCK: usize = 400;
        const NOTE_FRAME: usize = 6_300;

        let prepare = Prepare::new(
            SampleRate::of(SAMPLE_RATE),
            MaxBlock::of(Samples::new(CHUNK)),
        );
        let (mut live, _exec) = Editor::new(prepare);
        let key = NodeKey(9);
        let _controls = live.insert(
            key,
            "plugin",
            probe.client.bind(
                tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0))
                    .expect("a tempo"),
            ),
        );
        feed_note(&mut live, key, NOTE_FRAME);
        live.wire_outputs(vec![PortSource::Node(OutputPort::new(key, 0))])
            .unwrap();
        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            sample_rate: SampleRate::of(SAMPLE_RATE),
            ..Default::default()
        }));
        let (_fork_ed, mut fork_exec) = live
            .fork(
                ForkTarget::MainOutput,
                prepare.with_render_mode(RenderMode::Offline),
            )
            .expect("forks");

        let mut out = Vec::new();
        let mut block = vec![0.0f32; BLOCK];
        let pass = if passes { 64 } else { BLOCK };
        while out.len() < NOTE_FRAME + 2 * CHUNK {
            let mut done = 0;
            while done < BLOCK {
                let n = pass.min(BLOCK - done);
                render_graph(
                    &*timeline,
                    &mut fork_exec,
                    Planes::default(),
                    PlanesMut::new(&mut [&mut block[done..done + n]]).unwrap(),
                );
                done += n;
            }
            out.extend_from_slice(&block);
        }
        assert_eq!(
            out.iter().position(|&s| s != 0.0),
            Some(NOTE_FRAME + CHUNK),
            "64-frame passes: {passes}: the note sounds on its frame, one chunk late"
        );
    }
}

/// **A clip node's note reaches the plugin's event input on its frame**,
/// through the pipeline, in an export and in chunks that begin inside a
/// block. The clip node (key 1) feeds the plugin's MIDI event input;
/// the plugin's own port is empty. As
/// [`a_clip_note_in_a_chunk_that_begins_mid_block_lands_on_its_frame`], but
/// the note arrives on the event port: 480-frame chunks, 400-frame blocks,
/// the note at 6 300 in the chunk from 6 240.
#[test]
fn a_clip_nodes_note_reaches_the_plugins_event_input_on_its_frame() {
    use tutti_graph::{EventEdge, EventInputPort, EventOutputPort};
    use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
    use tutti_ump::MidiChannel;
    use tutti_units::Beat;

    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    let probe = load_probe(SAMPLE_RATE);
    const CHUNK: usize = 480;
    const BLOCK: usize = 400;
    const NOTE_FRAME: usize = 6_300;

    let prepare = Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(CHUNK)),
    );
    let (mut live, _exec) = Editor::new(prepare);
    let (clip, key) = (NodeKey(1), NodeKey(9));
    live.insert(
        clip,
        "clip",
        MidiClipNode::new([TimedMidiEvent::new(
            Beat::new(NOTE_FRAME as f64 / 24_000.0),
            Message::midi2(
                MidiChannel::FIRST,
                Voice2::note_on(NoteNumber::of(60), NoteVelocity::MAX),
            ),
        )
        .unwrap()]),
    );
    let _controls = live.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    live.connect_events(
        EventInputPort::new(key, 0),
        EventEdge::Direct(EventOutputPort::new(clip, 0)),
    )
    .unwrap();
    live.wire_outputs(vec![PortSource::Node(OutputPort::new(key, 0))])
        .unwrap();
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate::of(SAMPLE_RATE),
        ..Default::default()
    }));
    let (_fork_ed, mut fork_exec) = live
        .fork(
            ForkTarget::MainOutput,
            prepare.with_render_mode(RenderMode::Offline),
        )
        .expect("a clip node and a plugin fork");

    let mut out = Vec::new();
    let mut block = vec![0.0f32; BLOCK];
    while out.len() < NOTE_FRAME + 2 * CHUNK {
        render_graph(
            &*timeline,
            &mut fork_exec,
            Planes::default(),
            PlanesMut::new(&mut [&mut block[..]]).unwrap(),
        );
        out.extend_from_slice(&block);
    }
    assert_eq!(
        out.iter().position(|&s| s != 0.0),
        Some(NOTE_FRAME + CHUNK),
        "the note sounds on its frame, one chunk late"
    );
}

/// Records the MIDI reaching its event input as `(frame, first word)`, and
/// renders a silent mono output so a `MainOutput` fork takes it. Forked by clone:
/// the fork records into the same list.
#[derive(Clone)]
struct MidiSink(tutti_fixtures::Record<(u64, u32)>);

impl MidiSink {
    const SHAPE: tutti_graph::Shape =
        tutti_graph::Shape::source_unassigned(tutti_buffers::ChannelLayout::MONO).events(1, 0);
    const IN: tutti_graph::InEvents = Self::SHAPE
        .event_input(0)
        .expect("one event input is declared");
}

impl tutti_graph::Node for MidiSink {
    fn shape(&self) -> tutti_graph::Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(
        &mut self,
        cx: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(Self::IN) {
            if let tutti_graph::EventKind::Midi(ump) = e.kind {
                seen.push((cx.env.frame.get() + u64::from(e.offset.get()), ump.0[0]));
            }
        }
        io.output(0).fill(0.0);
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A plugin's MIDI-out keeps its place against its audio**, on the node's
/// MIDI event output, in an export. The probe in `Notes` mode is a MIDI thru
/// whose gate opens on the note-on's frame, so its audio and its MIDI-out
/// mark the same frame: the note-on (a clip node's, at 6 300) comes out one
/// chunk (480) late on both, in 400-frame blocks, where the chunk the reply
/// belongs to starts playing mid-block. Rendered three times: the export
/// waits for each chunk's reply, so the frame never depends on how the
/// reply raced the audio.
#[test]
fn a_plugins_midi_out_keeps_its_place_against_its_audio() {
    use tutti_graph::{EventEdge, EventInputPort, EventOutputPort};
    use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
    use tutti_ump::MidiChannel;
    use tutti_units::Beat;

    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    const CHUNK: usize = 480;
    const BLOCK: usize = 400;
    const NOTE_FRAME: usize = 6_300;
    for _ in 0..3 {
        let probe = load_probe(SAMPLE_RATE);
        let prepare = Prepare::new(
            SampleRate::of(SAMPLE_RATE),
            MaxBlock::of(Samples::new(CHUNK)),
        );
        let (mut live, _exec) = Editor::new(prepare);
        let (clip, key, sink) = (NodeKey(1), NodeKey(9), NodeKey(3));
        let on = midi::note_on(MidiChannel::FIRST, 60, 0xFFFF);
        live.insert(
            clip,
            "clip",
            MidiClipNode::new([
                TimedMidiEvent::new(Beat::new(NOTE_FRAME as f64 / 24_000.0), on).unwrap(),
            ]),
        );
        let _controls = live.insert(
            key,
            "plugin",
            probe.client.bind(
                tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0))
                    .expect("a tempo"),
            ),
        );
        let seen = tutti_fixtures::Record::new();
        live.insert(
            sink,
            "sink",
            tutti_graph::ForkByClone(MidiSink(seen.clone())),
        );
        live.connect_events(
            EventInputPort::new(key, 0),
            EventEdge::Direct(EventOutputPort::new(clip, 0)),
        )
        .unwrap();
        live.connect_events(
            EventInputPort::new(sink, 0),
            EventEdge::Direct(EventOutputPort::new(key, 0)),
        )
        .unwrap();
        live.wire_outputs(vec![
            PortSource::Node(OutputPort::new(key, 0)),
            PortSource::Node(OutputPort::new(sink, 0)),
        ])
        .unwrap();
        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            sample_rate: SampleRate::of(SAMPLE_RATE),
            ..Default::default()
        }));
        let (_fork_ed, mut fork_exec) = live
            .fork(
                ForkTarget::MainOutput,
                prepare.with_render_mode(RenderMode::Offline),
            )
            .expect("a clip, a plugin and a sink fork");

        let mut out = Vec::new();
        let (mut block, mut silent) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
        while out.len() < NOTE_FRAME + 2 * CHUNK {
            render_graph(
                &*timeline,
                &mut fork_exec,
                Planes::default(),
                PlanesMut::new(&mut [&mut block[..], &mut silent[..]]).unwrap(),
            );
            out.extend_from_slice(&block);
        }
        let heard = out.iter().position(|&s| s != 0.0);
        assert_eq!(
            heard,
            Some(NOTE_FRAME + CHUNK),
            "the gate opens a chunk late"
        );
        let seen = seen.lock().unwrap();
        let note_on = seen.iter().find(|(_, w)| (w >> 20) & 0xf == 0x9);
        assert_eq!(
            note_on.map(|&(f, _)| f),
            Some((NOTE_FRAME + CHUNK) as u64),
            "the MIDI-out's note-on on the frame its audio sounds: {seen:?}"
        );
    }
}

/// **An export is aligned when the plugin's latency moves in offline mode.**
/// The probe adds 24 frames to its latency (and its delay) once it is told to
/// render offline, as a plugin with a higher-quality offline mode does. The
/// fork is told so asynchronously; its `prepare` waits for the change to land
/// (`AudioBridge::settle`) before the fork's graph is compiled, so the
/// export's plan carries 137 + 24 + the render's chunk (its `MaxBlock`,
/// `GRAPH_MAX_BLOCK`: an export has no device) and trims exactly that: the render is
/// the ramp from frame 0, sample for sample, and the fork reports no fault.
#[test]
fn an_export_is_aligned_when_the_plugin_latency_moves_offline() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new()
        .render_mode(render::LATENCY)
        .offline_extra_latency(24);
    let probe = load_probe(SAMPLE_RATE);
    let inputs = probe.client.inputs();

    let rate = SampleRate::of(SAMPLE_RATE);
    let (mut live, _exec) = Editor::new(Prepare::new(rate, MaxBlock::of(Samples::new(BLOCK))));
    let (src, key) = (NodeKey(1), NodeKey(2));
    live.insert(src, "ramp", tutti_graph::ForkByClone(RampSource));
    let _controls = live.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    for port in 0..inputs {
        live.connect_edge(EdgeId::into_port(
            tutti_graph::InputPort::new(key, port as u16),
            tutti_graph::Edge::Direct(PortSource::Node(OutputPort::new(src, 0))),
        ))
        .unwrap();
    }
    live.wire_outputs(vec![PortSource::Node(OutputPort::new(key, 0))])
        .unwrap();

    let graph = fork_graph(&live, rate);
    let latency = graph.reported_latency();
    assert_eq!(
        latency,
        Samples::new(137 + 24 + tutti_export::GRAPH_MAX_BLOCK.get()),
        "the plan carries the offline latency"
    );
    let config = (
        tutti_export::Job::new(
            rate,
            tutti_units::Seconds::new(0.1).to_samples(rate),
            latency,
            tutti_export::TailPolicy::Frames(Samples::ZERO),
        )
        .expect("a render"),
        tutti_buffers::ChannelLayout::MONO,
    );
    let rendered = render_to_buffers(graph, &config, &tutti_timeline::FrozenClock)
        .expect("a fork whose latency settled exports without a fault")
        .into_finished()
        .expect("nothing cancels a control with no token");
    let plane = &rendered.planes()[0];
    assert!(!plane.is_empty());
    let wrong = plane
        .iter()
        .enumerate()
        .filter(|&(j, &s)| s != ramp(j as u64))
        .count();
    assert_eq!(
        wrong,
        0,
        "{wrong} of {} samples are not the ramp: the export is misaligned",
        plane.len()
    );
}

/// **A live fork of a served plugin that was told to render offline renders
/// offline**: the live plugin is told `Offline`, a `RenderMode::Realtime` fork is
/// made, and the fork's declared latency carries the plugin's offline extra
/// (which the probe reports only in its offline mode, `clap.latency`). The
/// fork is the live plugin's mode, not the graph's.
#[test]
fn a_live_fork_of_a_served_plugin_told_offline_is_offline() {
    const BASE: Samples = Samples::new(137);
    const OFFLINE_EXTRA: Samples = Samples::new(24);
    let _lock = machine_lock();
    let _env = ProbeEnv::new()
        .render_mode(render::LATENCY)
        .offline_extra_latency(OFFLINE_EXTRA.get() as u32);
    let probe = load_probe(SAMPLE_RATE);
    let client = bound(probe.client);
    let rebinder = client.rebinder();
    assert!(
        client
            .handle()
            .announce_render_mode(tutti_plugin_types::RenderMode::Offline),
        "the live plugin is told the mode"
    );
    // The plugin's answer reaches its lifecycle at a pump, as a session's
    // update runs it.
    tutti_fixtures::wait_until(
        "the live plugin to answer the mode",
        std::time::Duration::from_secs(60),
        || {
            rebinder.tick(tutti_fixtures::main_thread());
            client.render_mode() == Some(tutti_plugin_types::RenderMode::Offline)
        },
    );
    // Forked through the graph in `RenderMode::Realtime`, prepared in realtime: the
    // graph does not say offline, so only the live plugin's mode can.
    let rate = SampleRate::of(SAMPLE_RATE);
    let (live, _exec) = live_graph(client);
    let (editor, executor) = live
        .fork(
            ForkTarget::MainOutput,
            Prepare::new(rate, MaxBlock::of(Samples::new(BLOCK))),
        )
        .expect("a graph holding a plugin forks");
    let graph = tutti_export::RenderGraph::assemble(editor, executor, tutti_export::Setup::new())
        .expect("forked together");
    assert_eq!(
        graph.reported_latency(),
        BASE + OFFLINE_EXTRA + Samples::new(BLOCK),
        "the fork carries the plugin's offline latency"
    );
}

/// **A fork's restore loads the live preset and the live state into a fresh
/// served instance**: `ForkState::restore` over a launched `Ipc`, against the
/// real server. The plugin's preset file is accepted (a refused preset is a
/// `Rejected` restore), and the state, which goes in after it, is what the
/// instance holds: the live gain, not the preset's.
#[test]
fn a_served_restore_loads_the_preset_and_the_state_into_a_fresh_served_instance() {
    use tutti_plugin_host::ForkState;
    use tutti_plugin_ipc::Ipc;
    use tutti_plugin_types::{PluginFormat, PresetId, RenderMode, StateUse};

    let _lock = machine_lock();
    let _env = env().preset_load();
    let probe = load_probe(SAMPLE_RATE);
    probe
        .client
        .handle()
        .send_parameter(GAIN, gain_at(-6.0))
        .expect("a declared parameter");
    let state = probe
        .handle
        .save_state(StateUse::Duplicate)
        .expect("the probe saves");
    let dir = tempfile::TempDir::new().expect("a directory");
    let preset = dir.path().join("a.tutti-preset");
    std::fs::write(&preset, b"x").expect("a preset file");

    let launched = Ipc::launch(
        IpcConfig::new(),
        &tutti_plugin_ipc::LoadRequest {
            origin: &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
            format: PluginFormat::Clap,
            rate: SampleRate::of(SAMPLE_RATE),
            restore: None,
        },
        None,
        tutti_fixtures::executor(),
    )
    .expect("a fresh probe launches");
    let ipc = launched.ipc;
    ForkState::new(
        Some(PresetId::Location(preset)),
        state,
        RenderMode::Realtime,
    )
    .restore(
        &mut (),
        |(), id| ipc.load_preset(id),
        |(), bytes| ipc.load_state(bytes, StateUse::Duplicate),
    )
    .expect("the preset and the state load");
    assert_eq!(ipc.bridge().audio().parameter(GAIN), Some(gain_at(-6.0)));
}

/// **An export renders a graph of plugins the same on a worker pool as
/// alone** (doc 013, "The parallel executor"): two out-of-process probes side by side, each
/// at its own gain, forked offline twice — once rendered on the export's
/// thread, once on a four-participant `WorkerPool`, where the two plugins'
/// blocks go to their servers from different threads at once — and the two
/// renders agree bit for bit on both channels.
#[test]
fn an_export_of_plugins_is_bit_identical_on_a_pool() {
    let _lock = machine_lock();
    let _env = env();
    let a = load_probe(SAMPLE_RATE);
    let b = load_probe(SAMPLE_RATE);
    a.client
        .handle()
        .send_parameter(GAIN, gain_at(-6.0))
        .expect("a declared parameter");
    b.client
        .handle()
        .send_parameter(GAIN, gain_at(-12.0))
        .expect("a declared parameter");
    let (mut live, _exec) = Editor::new(Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(BLOCK)),
    ));
    let _ = live.insert(
        NodeKey(9),
        "a",
        a.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    let _ = live.insert(
        NodeKey(10),
        "b",
        b.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    live.wire_outputs(vec![
        PortSource::Node(OutputPort::new(NodeKey(9), 0)),
        PortSource::Node(OutputPort::new(NodeKey(10), 0)),
    ])
    .unwrap();
    let export = |pooled: bool| {
        let rate = SampleRate::of(SAMPLE_RATE);
        let (editor, mut executor) = fork_pair(&live, rate);
        if pooled {
            executor.set_pool(Some(Arc::new(
                tutti_graph::WorkerPool::builder()
                    .num_threads(4)
                    .build()
                    .expect("a worker pool"),
            )));
        }
        let parallel = executor.is_parallel();
        let graph =
            tutti_export::RenderGraph::assemble(editor, executor, tutti_export::Setup::new())
                .expect("forked together");
        let config = (
            tutti_export::Job::new(
                rate,
                tutti_units::Seconds::new(0.5).to_samples(rate),
                Samples::new(0),
                tutti_export::TailPolicy::Frames(Samples::ZERO),
            )
            .expect("a render"),
            tutti_buffers::ChannelLayout::STEREO,
        );
        let rendered = render_to_buffers(graph, &config, &tutti_timeline::FrozenClock)
            .expect("the fork exports")
            .into_finished()
            .expect("nothing cancels a control with no token");
        (rendered.into_planes(), parallel)
    };
    let (serial, was_parallel) = export(false);
    assert!(!was_parallel);
    let (pooled, was_parallel) = export(true);
    assert!(was_parallel, "two plugins spread over the pool");
    assert!(
        serial.iter().all(|c| c.iter().any(|&s| s != 0.0)),
        "both channels audible"
    );
    let bits = |p: &[Vec<f32>]| -> Vec<Vec<u32>> {
        p.iter()
            .map(|c| c.iter().map(|s| s.to_bits()).collect())
            .collect()
    };
    assert_eq!(bits(&serial), bits(&pooled));
}

/// **MIDI past a chunk's inline capacity is counted, not lost silently.** The
/// out-of-process node sends a chunk's MIDI in an inline list of
/// `MIDI_STACK_CAPACITY` (256); 300 note-ons on its event input in one
/// 512-frame chunk send 256 and count the other 44 on its controls. A chunk
/// within the cap adds nothing.
#[test]
fn midi_past_the_chunks_capacity_is_counted() {
    use tutti_graph::contract::Direct;
    use tutti_graph::{Event, Offset};
    use tutti_plugin_types::MIDI_STACK_CAPACITY;
    use tutti_ump::MidiChannel;

    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    let probe = load_probe(SAMPLE_RATE);
    let controls = probe.client.controls();
    let mut node = Direct::boxed(
        tutti_graph::IntoNode::into_node(probe.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ))
        .0,
        SampleRate::of(SAMPLE_RATE),
        512,
    );
    let node_in = node
        .node
        .shape()
        .event_input(0)
        .expect("the node declares an event input");
    let notes = |n: usize| -> Vec<Event> {
        (0..n)
            .map(|i| {
                let on = midi::note_on(MidiChannel::FIRST, (i % 128) as u8, 0xFFFF);
                Event::midi(Offset::of(Samples::new(i), Samples::new(512)), on)
            })
            .collect()
    };
    node.events(node_in, &notes(300));
    node.block();
    assert_eq!(
        controls.dropped_midi_in(),
        (300 - MIDI_STACK_CAPACITY) as u64
    );
    node.events(node_in, &notes(10));
    node.block();
    assert_eq!(
        controls.dropped_midi_in(),
        (300 - MIDI_STACK_CAPACITY) as u64
    );
}

/// **An offline fork of a plugin that refuses its rate renders silence,
/// and says why.** The fork's server (only: the switch is armed after the live
/// one loaded) refuses the 96 kHz the fork is prepared at, and the forked
/// editor reports `Failed` with a `PluginRenderFault::RateRefused` cause, not
/// `TimedOut`.
#[test]
fn a_fork_of_a_refused_rate_is_silent_and_a_failed_fault() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let _refuse = ProbeEnv::new().refuse_rate(96_000.0);

    let prepare = Prepare::new(SampleRate::SR_96K, MaxBlock::of(Samples::new(BLOCK)));
    let (mut editor, _exec) = Editor::new(Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(BLOCK)),
    ));
    let key = NodeKey(3);
    let _controls = editor.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    editor
        .wire_outputs(vec![PortSource::Node(OutputPort::new(key, 0))])
        .unwrap();
    let (fork_ed, fork_exec) = editor
        .fork(
            ForkTarget::Node(key),
            prepare.with_render_mode(RenderMode::Offline),
        )
        .expect("forks");
    let mut renderer = Renderer::new(fork_ed, fork_exec);
    let out = renderer.render(BLOCK * 20);
    assert!(out[0].iter().all(|&s| s == 0.0), "silence");
    let (fork_ed, _exec) = renderer.into_parts();
    let fault = fork_ed
        .fork_health()
        .expect_err("the fork refused its rate");
    assert_eq!(fault.kind, ForkFaultKind::Failed, "{fault}");
    assert!(
        matches!(
            fault.cause.downcast_ref::<PluginRenderFault>(),
            Some(PluginRenderFault::RateRefused(refusal)) if refusal.rate.get() == 96_000.0
        ),
        "{fault:?}"
    );
}
