//! What the in-process VST2 node dispatches from the audio thread.
//!
//! `Node::reset` and `Node::prepare` may run while the plugin is rendering
//! (a graph resets and re-prepares a node between blocks, on another thread
//! from the one that renders it), so neither may reach
//! `Instance::set_sample_rate`: that brackets `effSetSampleRate`(10) in
//! `effMainsChanged`(12) — the opcode plugins allocate and free their
//! rate-dependent buffers in. Routing there dispatches two main-thread-only
//! opcodes, plus `effStopProcess`(71) / `effStartProcess`(72), racing the
//! render.
//!
//! Asserting a *negative* — that no opcode was dispatched — needs the plugin's
//! own view, not the host's. The reference probe counts `effMainsChanged`
//! separately per direction (`resume_count` / `suspend_count`) and records the
//! last `effSetSampleRate` it saw, so what crossed the AEffect seam is directly
//! readable. Every test below reads those counters rather than any host-side
//! flag, and each pairs its negative with a positive that moves the same
//! counter — an assertion that a number stayed at zero passes just as well
//! against a probe that was never loaded.
//!
//! Two tests read the render itself: what the node writes is what the
//! plugin wrote, and silence past the channels it hands the plugin. The last
//! counts the parameter reports a burst from `process` loses, on the handle.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use tutti_buffers::Planes;
use tutti_fixtures::main_thread;
use tutti_rt::LockExt;
use tutti_vst2_host::Vst2Loaded;

use tutti_plugin_host::PluginHandle;
use tutti_plugin_host::{PluginNode, load_client};
use tutti_units::SampleRate;
use tutti_vst2_test_plugin::ProcessCapture;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// Serializes the whole load→drive→read sequence against the probe's one
/// process-global capture, and against other tests loading the same image.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock_recover()
}

/// Re-open the image the host loaded and call one of the probe's exports.
///
/// Must go through `dlopen`: the linked rlib is a separate image with separate
/// statics, and only the cdylib's globals see the host's calls. The rlib is
/// still linked, for the shared `#[repr(C)]` type definitions.
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

/// The graph node plus the handle that keeps it alive, loaded against the
/// reference probe with its capture zeroed.
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

/// `unit` prepared at `rate`, for blocks of up to [`BLOCK`].
fn prepare_at(unit: &mut PluginNode<Vst2Loaded>, rate: SampleRate) {
    tutti_graph::Node::prepare(
        unit,
        &tutti_graph::Prepare::new(
            rate,
            tutti_buffers::MaxBlock::of(tutti_units::Samples::new(BLOCK)),
        ),
    );
}

/// Render one block through the node, prepared as a graph prepares it
/// (`Node::prepare`, which sizes its scratch and parks its rate), then
/// `Node::process` by hand (`tutti_graph::contract::drive_input`).
fn drive_block(unit: &mut PluginNode<Vst2Loaded>) {
    drive_blocks(unit, BLOCK, BLOCK);
}

/// One block of `frames` through `unit` prepared for blocks of up to `max`.
fn drive_blocks(unit: &mut PluginNode<Vst2Loaded>, max: usize, frames: usize) {
    use tutti_graph::{Node, Prepare};
    let rate = SampleRate::of(SAMPLE_RATE);
    Node::prepare(
        unit,
        &Prepare::new(
            rate,
            tutti_buffers::MaxBlock::of(tutti_units::Samples::new(max)),
        ),
    );
    process_block(unit, frames);
}

/// One block of `frames` through `unit` as it is prepared now: no prepare.
fn process_block(unit: &mut PluginNode<Vst2Loaded>, frames: usize) {
    use tutti_graph::{Env, Node, TransportChanges, TransportPosition};
    let rate = SampleRate::of(SAMPLE_RATE);
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

/// `Node::reset` must not dispatch `effMainsChanged`: it asks the plugin to forget with
/// `effStopProcess` (through the render handle, which is the audio thread's), and never
/// suspends or resumes it.
///
/// It ran `Instance::set_sample_rate`, so pre-fix this saw the full
/// `effStopProcess` → `effMainsChanged(0)` → `effSetSampleRate` →
/// `effMainsChanged(1)` → `effStartProcess` cycle: `suspend_count` and
/// `resume_count` each up by one, from the audio thread, with an allocation
/// inside each `effMainsChanged`.
///
/// The two counters are read *before* as well as after, so this asserts they
/// did not move rather than that they hold some absolute value — `load` itself
/// resumes once, and hard-coding that constant would make the test a statement
/// about the loader instead of about `reset`.
fn reset_dispatches_no_mains_changed_to_the_plugin() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let before = read_capture(&loaded.path);
    assert!(
        before.valid,
        "the probe observed no render — a reset that dispatches nothing is \
         trivially true against a plugin that was never driven"
    );

    tutti_graph::Node::reset(&mut loaded.unit);

    let after = read_capture(&loaded.path);
    assert_eq!(
        after.suspend_count, before.suspend_count,
        "reset dispatched effMainsChanged(0) from the audio thread"
    );
    assert_eq!(
        after.resume_count, before.resume_count,
        "reset dispatched effMainsChanged(1) from the audio thread"
    );

    drop(loaded.handle);
}

/// Blocks of any length up to the prepared maximum reach the plugin, and a
/// re-prepare for a longer maximum dispatches nothing to it.
///
/// The render scratch is the Processor's, sized at the activation to the block
/// the plugin was told, so a prepare has nothing to resize and never grows
/// anything on the audio thread.
fn blocks_past_64_frames_reach_the_plugin_after_a_prepare() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let before = read_capture(&loaded.path);
    assert!(before.valid, "the probe observed no render");

    drive_blocks(&mut loaded.unit, 1_024, 1_000);
    drive_blocks(&mut loaded.unit, 1_024, 1);
    let after = read_capture(&loaded.path);
    assert_eq!(
        after.suspend_count, before.suspend_count,
        "a re-prepare dispatched effMainsChanged(0)"
    );
    assert_eq!(
        after.resume_count, before.resume_count,
        "a re-prepare dispatched effMainsChanged(1)"
    );

    drop(loaded.handle);
}

/// [`prepare_at`], on a thread that is not the main thread: where a graph's
/// `prepare` runs when its commit is off the main thread.
fn prepare_off_main_thread(unit: &mut PluginNode<Vst2Loaded>, rate: SampleRate) {
    std::thread::scope(|s| {
        s.spawn(|| prepare_at(unit, rate))
            .join()
            .expect("the prepare returns");
    });
}

/// `Node::prepare` at a new rate **off the main thread** (a commit there
/// re-prepares the graph) must park the rate rather than dispatch it: telling
/// a VST2 plugin its rate needs the main thread.
///
/// The rate is *also* asserted not to have reached the plugin, not only the
/// mains counters: a host that skipped the bracket but still dispatched
/// `effSetSampleRate` would leave the counters clean while reconfiguring a
/// running plugin, which is the same class of bug one opcode narrower.
fn set_sample_rate_parks_the_rate_instead_of_dispatching_it() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let before = read_capture(&loaded.path);
    assert!(before.valid, "the probe observed no render");
    // Load told the plugin 48k; the new rate must be distinguishable from it.
    assert_eq!(
        before.sample_rate, SAMPLE_RATE as f32,
        "load must have told the plugin the rate it was loaded at"
    );

    prepare_off_main_thread(&mut loaded.unit, SampleRate::SR_96K);

    let after = read_capture(&loaded.path);
    assert_eq!(
        after.suspend_count, before.suspend_count,
        "prepare dispatched effMainsChanged(0)"
    );
    assert_eq!(
        after.resume_count, before.resume_count,
        "prepare dispatched effMainsChanged(1)"
    );
    assert_eq!(
        after.sample_rate, SAMPLE_RATE as f32,
        "prepare dispatched effSetSampleRate"
    );

    drop(loaded.handle);
}

/// The pending rate is not dropped: the main-thread drain delivers it, once
/// the audio thread has handed the Processor back.
///
/// This is the positive half of the test above. Without it, "nothing was
/// dispatched" is satisfied by a node that discards the rate entirely, which
/// would be the worse bug — the plugin keeps rendering at its previous rate
/// forever, with no diagnostic anywhere.
///
/// The drain recalls the Processor, which is on the audio thread; the next
/// block hands it back (and renders silence), and the next drain runs the
/// change.
fn the_pending_rate_reaches_the_plugin_on_the_main_thread_drain() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let before = read_capture(&loaded.path);
    assert_eq!(before.sample_rate, SAMPLE_RATE as f32);

    prepare_off_main_thread(&mut loaded.unit, SampleRate::SR_96K);
    // The drain point: `editor_idle` is the per-frame main-thread call, and it
    // is reached through the handle rather than the node — the two ends of the
    // deferral are deliberately on opposite sides of the audio/control split.
    loaded.handle.editor_idle(main_thread());
    assert_eq!(
        read_capture(&loaded.path).sample_rate,
        SAMPLE_RATE as f32,
        "the Processor is still on the audio thread"
    );
    process_block(&mut loaded.unit, BLOCK);
    loaded.handle.editor_idle(main_thread());

    let after = read_capture(&loaded.path);
    assert_eq!(
        after.sample_rate, 96_000.0,
        "the rate left pending by prepare never reached the plugin"
    );
    // And it arrived through the bracket, which is what makes it safe to
    // deliver at all: one suspend and one resume, on the main thread.
    assert_eq!(
        after.suspend_count,
        before.suspend_count + 1,
        "the deferred rate change must be bracketed in effMainsChanged(0)"
    );
    assert_eq!(
        after.resume_count,
        before.resume_count + 1,
        "the deferred rate change must be bracketed in effMainsChanged(1)"
    );

    drop(loaded.handle);
}

/// `Node::prepare` at a new rate **on the main thread** tells the plugin in
/// the call: the node is out of any executor while it is prepared, so its
/// realtime end hands the Processor back and takes the new one there, with no
/// block of silence and no pump.
fn a_prepare_on_the_main_thread_tells_the_plugin_at_once() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let before = read_capture(&loaded.path);
    assert_eq!(before.sample_rate, SAMPLE_RATE as f32);

    let _main = main_thread();
    prepare_at(&mut loaded.unit, SampleRate::SR_96K);

    let after = read_capture(&loaded.path);
    assert_eq!(after.sample_rate, 96_000.0, "told in the prepare");
    assert_eq!(after.suspend_count, before.suspend_count + 1);
    assert_eq!(after.resume_count, before.resume_count + 1);
    assert_eq!(
        loaded.unit.controls().block_losses().unlent,
        0,
        "no block went without it"
    );
    process_block(&mut loaded.unit, BLOCK);
    assert_eq!(
        loaded.unit.controls().block_losses().unlent,
        0,
        "the new Processor renders"
    );

    drop(loaded.handle);
}

/// A second drain with nothing pending must dispatch nothing.
///
/// `effMainsChanged` is not documented as idempotent and the probe counts each
/// direction, so a drain that fired unconditionally would suspend and resume a
/// running plugin on every editor frame.
fn a_drain_with_nothing_pending_dispatches_nothing() {
    let _guard = lock_probe();
    let loaded = load();

    loaded.handle.editor_idle(main_thread());
    let before = read_capture(&loaded.path);
    loaded.handle.editor_idle(main_thread());
    let after = read_capture(&loaded.path);

    assert_eq!(
        after.suspend_count, before.suspend_count,
        "an empty drain suspended the plugin"
    );
    assert_eq!(
        after.resume_count, before.resume_count,
        "an empty drain resumed the plugin"
    );

    drop(loaded.handle);
}

/// What the node writes is what the plugin wrote, channel by channel and
/// frame by frame: the graph's input reaches the plugin unshifted, and each
/// output channel is the plugin's own (the probe's oracle, `in + tag(ch)`).
///
/// Two blocks with different inputs, the second shorter than the first, so
/// a channel table left pointing at an earlier block's staging, an offset
/// slice, or a swapped channel each shows as a wrong sample. Every value is
/// a small integer, so the comparison is exact on every platform.
fn the_output_is_what_the_plugin_wrote() {
    use tutti_graph::{Env, Node, Prepare, TransportChanges, TransportPosition};
    use tutti_vst2_test_plugin::channel_tag;

    let _guard = lock_probe();
    let mut loaded = load();
    let rate = SampleRate::of(SAMPLE_RATE);
    Node::prepare(
        &mut loaded.unit,
        &Prepare::new(
            rate,
            tutti_buffers::MaxBlock::of(tutti_units::Samples::new(BLOCK)),
        ),
    );
    let channels = usize::from(Node::shape(&loaded.unit).audio_in().count());
    assert_eq!(channels, 2, "the probe's default layout is stereo");

    for (block, frames) in [(0usize, BLOCK), (1, BLOCK - 17)] {
        // Channel `c`, frame `i`: `1000·c + 10·i + block`, distinct across
        // channels, frames and blocks.
        let input: Vec<Vec<f32>> = (0..channels)
            .map(|c| {
                (0..frames)
                    .map(|i| (1_000 * c + 10 * i + block) as f32)
                    .collect()
            })
            .collect();
        let inputs: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
        let env = Env {
            frame: tutti_units::Frame::new(0),
            sample_rate: rate,
            block_len: tutti_units::Samples::new(frames),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        };
        let out = tutti_graph::contract::drive_input(
            &mut loaded.unit,
            &env,
            Planes::new(&inputs).unwrap(),
            &[],
            &[],
        );
        assert_eq!(out.audio.len(), channels);
        for (c, ch) in out.audio.iter().enumerate() {
            let want: Vec<f32> = input[c].iter().map(|s| s + channel_tag(c)).collect();
            assert_eq!(ch, &want, "block {block}, channel {c}");
        }
    }

    drop(loaded.handle);
}

/// A plugin wider than a block serves (sixteen channels a direction) is
/// refused at load by name, not run on its first sixteen channels.
///
/// The probe is loaded with eighteen inputs and outputs: no node is built, and
/// the load's reason is the [`PluginTooWide`] text for the input direction,
/// the first one checked.
fn a_plugin_wider_than_a_block_is_refused_at_load() {
    use tutti_plugin_types::{IpcError, LoadStage, PluginTooWide};

    const WIDE: usize = 18;

    let _guard = lock_probe();
    let path = tutti_fixtures::vst2_probe().to_path_buf();
    reset_probe(&path);
    // SAFETY: `PROBE_LOCK` is held, so no other test thread touches the
    // environment; the variables are cleared once the AEffect is built.
    unsafe {
        std::env::set_var("TUTTI_VST2_PROBE_INPUTS", WIDE.to_string());
        std::env::set_var("TUTTI_VST2_PROBE_OUTPUTS", WIDE.to_string());
    }
    let loaded = load_client::<Vst2Loaded, f32>(
        main_thread(),
        &tutti_fixtures::detached_work(),
        &tutti_plugin_types::PluginRef::file(&path),
        SampleRate::of(SAMPLE_RATE),
    );
    unsafe {
        std::env::remove_var("TUTTI_VST2_PROBE_INPUTS");
        std::env::remove_var("TUTTI_VST2_PROBE_OUTPUTS");
    }
    let Err(tutti_plugin_host::LoadClientError::Failed(IpcError::LoadFailed {
        stage, reason, ..
    })) = loaded
    else {
        panic!("a plugin of {WIDE} channels has no node");
    };
    assert_eq!(stage, LoadStage::Activation);
    assert_eq!(
        reason,
        PluginTooWide {
            direction: "input",
            channels: WIDE
        }
        .to_string()
    );
}

/// A plugin's parameter reports the host lost are counted on its
/// `PluginHandle`, through `Control::dropped_reported_edits`. The probe sends a burst of
/// `audioMasterAutomate` reports from inside each `process` call, more than
/// the instance's bounded queue holds, and nothing drains the queue between
/// the two blocks: the first block overflows it, and every report of the
/// second is refused.
///
/// The second block's count is the first's plus the whole burst, so the
/// assertion does not depend on the queue's capacity.
fn the_handle_counts_lost_parameter_reports() {
    const BURST: u32 = 600;

    let _guard = lock_probe();
    let mut loaded = load();
    assert_eq!(loaded.handle.dropped_parameter_events(), 0);

    probe_call(
        &loaded.path,
        b"tutti_vst2_probe_set_automate_burst\0",
        |sym| {
            let f: extern "C" fn(u32) = unsafe { std::mem::transmute(*sym) };
            f(BURST);
        },
    );
    drive_block(&mut loaded.unit);
    let first = loaded.handle.dropped_parameter_events();
    assert!(first > 0, "a burst past the queue dropped nothing");
    assert!(first < u64::from(BURST), "the queue held none of the burst");

    drive_block(&mut loaded.unit);
    assert_eq!(
        loaded.handle.dropped_parameter_events(),
        first + u64::from(BURST),
        "a full queue refuses the whole second burst"
    );

    reset_probe(&loaded.path);
    drop(loaded.handle);
}

/// **A graph block longer than the plugin was told never reaches it whole.**
/// The loader announces 4096 (`effSetBlockSize`); the engine renders up to
/// 8192 (`DEFAULT_BLOCK_CAPACITY`, more with `Engine::with_capacity`). An
/// 8192-frame block is rendered as two calls of 4096, and the largest call
/// the plugin saw is the announced maximum.
fn a_block_past_the_announced_maximum_is_split_before_the_plugin() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_blocks(&mut loaded.unit, 8_192, 8_192);

    let cap = read_capture(&loaded.path);
    assert!(cap.valid, "the probe observed no render");
    assert_eq!(cap.max_block_size, 4_096, "the announced maximum");
    assert_eq!(
        cap.largest_block, 4_096,
        "the plugin was entered with more frames than it was told"
    );
    assert_eq!(cap.process_calls, 2, "4096 + 4096");

    drop(loaded.handle);
}

/// A plugin's `audioMasterIOChanged` restarts it and reaches the host's
/// `PluginHandle::subscribe` as its new latency and an I/O change, on the
/// next editor pump (the main-thread tick an in-process plugin has), and the
/// node's shape declares the new latency.
fn an_io_change_reaches_a_subscriber_and_the_shape() {
    use std::sync::{Arc, Mutex};
    use tutti_graph::Node as _;
    use tutti_plugin_types::PluginInvalidation;

    let _guard = lock_probe();
    let loaded = load();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let _subscription = loaded.handle.subscribe(move |n| {
        if let tutti_plugin_host::PluginSignal::Invalidate(e) = n {
            sink.lock().unwrap().push(e);
        }
    });

    // The restart an `audioMasterIOChanged` asks for resumes the plugin, which settles its
    // latency there (the probe's late latency): the figure read after it is 300.
    probe_call(
        &loaded.path,
        b"tutti_vst2_probe_set_late_latency\0",
        |sym| {
            let f: extern "C" fn(i32) = unsafe { std::mem::transmute(*sym) };
            f(300);
        },
    );
    let answered = probe_call(&loaded.path, b"tutti_vst2_probe_fire_io_changed\0", |sym| {
        let f: extern "C" fn(i32, i32) -> isize = unsafe { std::mem::transmute(*sym) };
        f(300, 2)
    });
    assert_eq!(answered, 1);
    assert!(seen.lock().unwrap().is_empty(), "nothing before the pump");

    // The first pump asks the audio thread for the Processor back; the block hands it over
    // (between blocks), and the next pump restarts the plugin and reads it again.
    let mut loaded = loaded;
    loaded.handle.editor_idle(main_thread());
    drive_block(&mut loaded.unit);
    loaded.handle.editor_idle(main_thread());

    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().any(|n| matches!(
            n,
            PluginInvalidation::Latency { samples } if *samples == tutti_units::Samples::new(300)
        )),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|n| matches!(n, PluginInvalidation::Io)),
        "{seen:?}"
    );
    assert_eq!(loaded.unit.shape().latency.samples().get(), 300);
}

/// The node's shape declares the plugin's buses: VST2 reports a width and no
/// placement, so each bus is `Unassigned`, the first is `Main`, and together the
/// buses cover every port the shape has.
///
/// Whether `shape` fills the buses at all is `chunked_node`'s test (this plugin states
/// no placement, so the default buses are what it declares as well).
fn the_shape_declares_the_plugins_buses() {
    use tutti_buffers::Speaker;
    use tutti_graph::{BusRole, Node as _};

    let _guard = lock_probe();
    let loaded = load();
    let shape = loaded.unit.shape();
    for (buses, ports) in [
        (shape.buses_in(), shape.audio_in()),
        (shape.buses_out(), shape.audio_out()),
    ] {
        assert_eq!(buses.width(), usize::from(ports.count()), "cover the ports");
        assert!(!buses.is_empty(), "the test plugin has audio on both sides");
        assert_eq!(buses.get(0).map(|b| b.role), Some(BusRole::Main));
        assert!(
            buses.iter().all(|b| b
                .topology
                .positions()
                .unwrap()
                .iter()
                .all(|&s| s == Speaker::Unassigned)),
            "VST2 states no placement"
        );
    }
}

/// **A block whose plugin refuses its audio entry is counted though it carries no MIDI**:
/// the plugin raises its input count past the buffer it was activated with (before the host
/// restarts it), so every entry is refused, the block renders silence without failing, and
/// the node's `entries_refused` count says so.
fn a_refused_block_with_no_midi_is_counted() {
    let _guard = lock_probe();
    let mut loaded = load();
    drive_block(&mut loaded.unit);
    assert_eq!(
        loaded.unit.controls().block_losses().entries_refused,
        0,
        "a live plugin refuses none"
    );

    probe_call(&loaded.path, b"tutti_vst2_probe_fire_io_changed\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(i32, i32) -> isize = unsafe { std::mem::transmute(*sym) };
        f(0, 64)
    });
    process_block(&mut loaded.unit, BLOCK);
    assert!(
        loaded.unit.controls().block_losses().entries_refused > 0,
        "the plugin's refusal reaches the node"
    );
    assert_eq!(
        loaded.unit.controls().block_losses().failed,
        0,
        "the block did not fail"
    );
    drop(loaded.handle);
}

/// The cases run on the process's main thread (`harness = false`): the node's `prepare`
/// and a fork tell the plugin at once only there.
fn main() {
    tutti_fixtures::main_thread_cases(tutti_fixtures::cases![
        reset_dispatches_no_mains_changed_to_the_plugin,
        blocks_past_64_frames_reach_the_plugin_after_a_prepare,
        set_sample_rate_parks_the_rate_instead_of_dispatching_it,
        the_pending_rate_reaches_the_plugin_on_the_main_thread_drain,
        a_prepare_on_the_main_thread_tells_the_plugin_at_once,
        a_drain_with_nothing_pending_dispatches_nothing,
        the_output_is_what_the_plugin_wrote,
        a_plugin_wider_than_a_block_is_refused_at_load,
        the_handle_counts_lost_parameter_reports,
        a_block_past_the_announced_maximum_is_split_before_the_plugin,
        an_io_change_reaches_a_subscriber_and_the_shape,
        the_shape_declares_the_plugins_buses,
        a_refused_block_with_no_midi_is_counted,
    ]);
}

/// **A plugin that does not process 64-bit audio fails a 64-bit load with its
/// widths, and hands back the plugin activated at 32-bit**: the reference
/// plugin declares no double-precision processing, so the error names
/// `Widths::F32`, and `into_inner` is a client that hosts in a 32-bit graph.
#[test]
fn a_plugin_without_64_bit_audio_fails_a_64_bit_load_and_returns_at_32() {
    let _probe = lock_probe();
    let path = tutti_fixtures::vst2_probe().to_path_buf();
    reset_probe(&path);
    let loaded = load_client::<Vst2Loaded, f64>(
        main_thread(),
        &tutti_fixtures::detached_work(),
        &tutti_plugin_types::PluginRef::file(&path),
        SampleRate::of(SAMPLE_RATE),
    );
    let Err(tutti_plugin_host::LoadClientError::Width(refused)) = loaded else {
        panic!("a 32-bit plugin loads at 64-bit");
    };
    assert_eq!(refused.requested(), tutti_buffers::FloatWidth::F64);
    assert_eq!(refused.widths(), tutti_plugin_types::Widths::F32);
    let (client, handle) = refused.into_inner();
    let mut node: PluginNode<Vst2Loaded, f32> = client.bind(tutti_fixtures::tempo());
    // The plugin came back whole: its handle is live, and the node renders a
    // block through the 32-bit entry point.
    assert!(
        !handle.descriptor().id.is_empty(),
        "the handle reads the plugin"
    );
    drive_block(&mut node);
    let capture = read_capture(&path);
    assert!(capture.valid);
    assert_eq!(capture.process_calls, 1);
    assert_eq!(
        capture.entry,
        tutti_vst2_test_plugin::ProcessEntry::Replacing
    );
}
