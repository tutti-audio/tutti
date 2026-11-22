//! Does a render mode (or an automation mode) set on the *handle* reach the plugin that renders?
//!
//! `PluginHandle::announce_render_mode` through the handle is the route a bounce driver uses, because `into_parts`
//! consumes the `Plugin` at load and only the handle survives.
//!
//! The in-process VST2 path must populate that route. With the `render_mode`
//! slot left `None`, `announce_render_mode` returns `false` and does nothing,
//! and an offline bounce renders every in-process VST2 plugin at live quality
//! while telling the other three formats otherwise.
//!
//! # What makes this observable
//!
//! VST2 carries the mode on `audioMasterGetCurrentProcessLevel`, a callback the
//! *plugin* polls rather than a property the host pushes. So the only witness is
//! a plugin that asks, and the reference probe now does — once per render,
//! recording the answer in `ProcessCapture::process_level`.
//!
//! Two halves had to exist for that to be possible, and the second was missing
//! too: the host answered the opcode (`tutti-vst2-ffi`'s `interfaces.rs`), but the
//! plugin-side `Host` trait had no `get_process_level` to send it with. No VST2
//! plugin hosted here could ask. Both halves ship together.
//!
//! `process_level_queries` is asserted alongside every value, because a probe
//! that never asked and a host that answered `0` (unknown) both leave
//! `process_level` at zero — and only one of those is a bug in what is under
//! test.

use std::path::PathBuf;
use std::sync::MutexGuard;
use tutti_buffers::Planes;
use tutti_fixtures::main_thread;
use tutti_rt::Mutex;
use tutti_vst2_host::Vst2Loaded;

use tutti_graph::RenderMode;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_host::{PluginNode, load_client};
use tutti_units::SampleRate;
use tutti_vst2_test_plugin::ProcessCapture;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// `kVstProcessLevelRealtime`.
const LEVEL_REALTIME: i32 = 2;
/// `kVstProcessLevelOffline`.
const LEVEL_OFFLINE: i32 = 4;

/// Serializes against the probe's one process-global capture.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock()
}

/// Re-open the image the host loaded and call one of the probe's exports.
///
/// Must go through `dlopen`: the linked rlib is a separate image with separate
/// statics, and only the cdylib's globals see the host's calls.
fn probe_call<F, R>(path: &PathBuf, symbol: &[u8], f: F) -> R
where
    F: FnOnce(libloading::Symbol<'_, *mut std::ffi::c_void>) -> R,
{
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: the symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(symbol) }.unwrap_or_else(|e| {
            panic!(
                "probe missing symbol {}: {e}",
                String::from_utf8_lossy(symbol)
            )
        });
    let r = f(sym);

    // Leak the handle, deliberately. The switches these helpers touch are
    // `static`s inside the probe's image, and they only survive while that
    // image stays mapped — dropping `lib` decrements the refcount that keeps it
    // mapped. With no `Vst2Loaded` holding the probe open at that moment, the
    // write is discarded with the unload and the next load maps a fresh image
    // reading the default.
    //
    // Measured on this bug in `vst2_latency.rs`: 2 of 6 runs failed without
    // this, 0 of 6 with it. It reads as flakiness because it passes whenever
    // another test's instance happens to keep the image resident.
    std::mem::forget(lib);
    r
}

fn reset_probe(path: &PathBuf) {
    probe_call(path, b"tutti_vst2_probe_reset_capture\0", |sym| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
    probe_call(path, b"tutti_vst2_probe_reset_switches\0", |sym| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
}

fn read_capture(path: &PathBuf) -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    probe_call(path, b"tutti_vst2_probe_capture\0", |sym| {
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(*sym) };
        unsafe { f(&mut cap) }
    });
    cap
}

struct Loaded {
    unit: PluginNode<Vst2Loaded>,
    handle: PluginHandle,
    path: PathBuf,
}

fn load() -> Loaded {
    let path = tutti_fixtures::vst2_probe().to_path_buf();
    reset_probe(&path);
    let (unit, handle) = load_client::<Vst2Loaded, f32>(
        main_thread(),
        &tutti_fixtures::detached_work(),
        &tutti_plugin_types::PluginRef::file(&path),
        SampleRate::of(SAMPLE_RATE),
    )
    .unwrap_or_else(|e| panic!("in-process load of reference plugin at {path:?}: {e:?}"));
    Loaded {
        unit: unit.bind(tutti_fixtures::tempo()),
        handle,
        path,
    }
}

/// Render one block through the node, prepared as a graph prepares it
/// (`Node::prepare`, which sizes its scratch and parks its rate), then
/// `Node::process` by hand (`tutti_graph::contract::drive_input`).
fn drive_block(unit: &mut PluginNode<Vst2Loaded>) {
    drive_blocks(unit, BLOCK, BLOCK);
}

/// One block of `frames` through `unit` prepared for blocks of up to `max`.
fn drive_blocks(unit: &mut PluginNode<Vst2Loaded>, max: usize, frames: usize) {
    drive_blocks_at(unit, SAMPLE_RATE, max, frames);
}

/// [`drive_blocks`] at `hz`: the node is prepared at that rate.
fn drive_blocks_at(unit: &mut PluginNode<Vst2Loaded>, hz: f64, max: usize, frames: usize) {
    drive_blocks_in(unit, hz, max, frames, RenderMode::Realtime);
}

/// [`drive_blocks_at`] with the node prepared under `mode`.
fn drive_blocks_in(
    unit: &mut PluginNode<Vst2Loaded>,
    hz: f64,
    max: usize,
    frames: usize,
    mode: RenderMode,
) {
    use tutti_graph::{Env, Node, Prepare, TransportChanges, TransportPosition};
    let rate = SampleRate::of(hz);
    Node::prepare(
        unit,
        &Prepare::new(
            rate,
            tutti_buffers::MaxBlock::of(tutti_units::Samples::new(max)),
        )
        .with_render_mode(mode),
    );
    let silence = vec![0.0f32; frames];
    let inputs: Vec<&[f32]> = (0..Node::shape(unit).audio_in().count())
        .map(|_| &silence[..])
        .collect();
    let env = Env {
        frame: tutti_units::Frame::new(0),
        sample_rate: rate,
        block_len: tutti_units::Samples::new(frames),
        transport: TransportPosition::default(),
        changes: TransportChanges::NONE,
        play: tutti_graph::PlayClock::ZERO,
    };
    let _ = tutti_graph::contract::drive_input(unit, &env, Planes::new(&inputs).unwrap(), &[], &[]);
}

/// The handle carries a render-mode route at all.
///
/// `false` here is what the bug was: no route, silently, with the call
/// reporting exactly what a plugin that *declined* the mode reports. The two
/// were indistinguishable to a caller, which is why this asserts the plain
/// boolean before anything reads the plugin's side.
fn the_handle_accepts_a_render_mode() {
    let _guard = lock_probe();
    let loaded = load();

    assert!(
        loaded.handle.announce_render_mode(RenderMode::Offline),
        "VST2 answers the process level from a host callback, so there is no \
         query a plugin could decline — this must not report a refusal"
    );
}

/// A plugin renders at realtime level when nothing set otherwise.
///
/// The baseline, and the half that makes the offline assertion meaningful: an
/// implementation that hard-coded `4` would pass the next test and fail this
/// one.
fn a_plugin_renders_at_realtime_level_by_default() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let cap = read_capture(&loaded.path);

    assert!(
        cap.process_level_queries > 0,
        "the probe never asked for the process level, so this test proved \
         nothing — check that `capture_process_level` still runs per render"
    );
    assert_eq!(
        cap.process_level, LEVEL_REALTIME,
        "a plugin nobody told anything must see realtime, not unknown"
    );
}

/// A mode set through the handle reaches the rendering plugin.
///
/// The whole point: this is the call a bounce driver makes, and the level the
/// plugin reads is the one it would size an oversampling buffer from.
fn an_offline_mode_set_on_the_handle_reaches_the_plugin() {
    let _guard = lock_probe();
    let mut loaded = load();

    assert!(loaded.handle.announce_render_mode(RenderMode::Offline));
    drive_block(&mut loaded.unit);
    let cap = read_capture(&loaded.path);

    assert!(cap.process_level_queries > 0, "the probe never asked");
    assert_eq!(
        cap.process_level, LEVEL_OFFLINE,
        "the plugin still reports realtime, so the mode set on the handle never \
         reached the instance the node renders"
    );
}

/// **The mode a graph prepares the node under reaches the plugin**, with no
/// call on the handle: the session's render mode is carried by `Prepare`.
#[test]
fn a_node_prepared_offline_renders_the_plugin_offline() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_blocks_in(
        &mut loaded.unit,
        SAMPLE_RATE,
        BLOCK,
        BLOCK,
        RenderMode::Offline,
    );
    let cap = read_capture(&loaded.path);

    assert!(cap.process_level_queries > 0, "the probe never asked");
    assert_eq!(
        cap.process_level, LEVEL_OFFLINE,
        "the node was prepared offline and the plugin still reports realtime"
    );
}

/// **A re-prepare at the mode the last prepare carried changes nothing**: a
/// mode set on the handle since survives it, and a changed one is told.
#[test]
fn a_re_prepare_at_the_same_mode_keeps_what_the_handle_set() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_blocks_in(
        &mut loaded.unit,
        SAMPLE_RATE,
        BLOCK,
        BLOCK,
        RenderMode::Offline,
    );
    assert_eq!(read_capture(&loaded.path).process_level, LEVEL_OFFLINE);
    assert!(loaded.handle.announce_render_mode(RenderMode::Realtime));
    drive_blocks_in(
        &mut loaded.unit,
        SAMPLE_RATE,
        BLOCK,
        BLOCK,
        RenderMode::Offline,
    );
    assert_eq!(
        read_capture(&loaded.path).process_level,
        LEVEL_REALTIME,
        "a prepare at the mode the last one carried must not undo the handle's"
    );
    drive_blocks_in(
        &mut loaded.unit,
        SAMPLE_RATE,
        BLOCK,
        BLOCK,
        RenderMode::Realtime,
    );
    drive_blocks_in(
        &mut loaded.unit,
        SAMPLE_RATE,
        BLOCK,
        BLOCK,
        RenderMode::Offline,
    );
    assert_eq!(
        read_capture(&loaded.path).process_level,
        LEVEL_OFFLINE,
        "a prepare at a changed mode is told"
    );
}

/// Restoring realtime reaches the plugin too.
///
/// A bounce must put the session back. Half a route — offline arrives, realtime
/// does not — leaves every plugin in offline mode for the rest of the session,
/// which is the failure the driver's guard design exists to prevent; this pins
/// that the underlying call can actually undo itself.
fn restoring_realtime_reaches_the_plugin() {
    let _guard = lock_probe();
    let mut loaded = load();

    assert!(loaded.handle.announce_render_mode(RenderMode::Offline));
    drive_block(&mut loaded.unit);
    assert_eq!(
        read_capture(&loaded.path).process_level,
        LEVEL_OFFLINE,
        "precondition: the plugin must have gone offline first"
    );

    assert!(loaded.handle.announce_render_mode(RenderMode::Realtime));
    drive_block(&mut loaded.unit);
    let cap = read_capture(&loaded.path);

    assert!(cap.process_level_queries > 0, "the probe never asked");
    assert_eq!(
        cap.process_level, LEVEL_REALTIME,
        "the plugin is stuck offline after the render ended"
    );
}

/// **A rate change keeps the render mode**: re-activating the plugin at a new
/// rate (a rejoin and an activation) must not put it back to realtime. An export
/// sets the mode and then prepares at its own rate.
fn a_rate_change_keeps_the_render_mode() {
    let _guard = lock_probe();
    let mut loaded = load();

    assert!(loaded.handle.announce_render_mode(RenderMode::Offline));
    drive_blocks_at(&mut loaded.unit, 44_100.0, BLOCK, BLOCK);
    // The drain point of the rate the audio side left pending.
    loaded.handle.editor_idle(main_thread());
    drive_blocks_at(&mut loaded.unit, 44_100.0, BLOCK, BLOCK);
    let cap = read_capture(&loaded.path);

    assert_eq!(cap.sample_rate, 44_100.0, "the rate change never happened");
    assert!(cap.process_level_queries > 0, "the probe never asked");
    assert_eq!(
        cap.process_level, LEVEL_OFFLINE,
        "the rate change re-activated the plugin in realtime mode"
    );
}

/// **An automation mode set through the handle reaches the plugin on the main
/// thread**: the probe asks `audioMasterGetAutomationState` once per render, and
/// its answer follows the mode (off 1, read 2, write 3, read and write 4) with
/// the call taking effect at the next block.
fn an_automation_mode_set_on_the_handle_reaches_the_plugin() {
    use tutti_plugin_types::AutomationMode;
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    assert_eq!(
        read_capture(&loaded.path).automation_state,
        1,
        "off before any mode is set"
    );
    for (mode, state) in [
        (AutomationMode::Reading, 2),
        (AutomationMode::Writing, 3),
        (AutomationMode::ReadWriting, 4),
        (AutomationMode::Off, 1),
    ] {
        assert_eq!(loaded.handle.set_automation_mode(mode), Ok(()), "{mode:?}");
        drive_block(&mut loaded.unit);
        assert_eq!(
            read_capture(&loaded.path).automation_state,
            state,
            "{mode:?} never reached the plugin the node renders"
        );
    }
}

/// **A VST2 plugin carries the automation-state feature**, so the session
/// forwards a mode to it: the host answers the query for every plugin.
fn a_vst2_plugin_claims_the_automation_state_feature() {
    let _guard = lock_probe();
    let loaded = load();

    assert!(
        loaded
            .handle
            .loaded()
            .features
            .contains(tutti_plugin_types::Features::AUTOMATION_STATE)
    );
}

/// The cases run on the process's main thread (`harness = false`): the node's `prepare`
/// and a fork tell the plugin at once only there.
fn main() {
    tutti_fixtures::main_thread_cases(tutti_fixtures::cases![
        the_handle_accepts_a_render_mode,
        a_plugin_renders_at_realtime_level_by_default,
        an_offline_mode_set_on_the_handle_reaches_the_plugin,
        restoring_realtime_reaches_the_plugin,
        a_rate_change_keeps_the_render_mode,
        an_automation_mode_set_on_the_handle_reaches_the_plugin,
        a_vst2_plugin_claims_the_automation_state_feature,
    ]);
}
