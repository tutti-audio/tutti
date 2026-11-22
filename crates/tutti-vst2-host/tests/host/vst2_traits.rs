//! The VST2 host against the shared traits: the halves of an activation and the
//! transitions between them, driven on the real reference plugin.
//!
//! The plugin's switches, capture and trace are process-global, so every test
//! holds [`PROBE_LOCK`].

use std::sync::MutexGuard;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tutti_buffers::InputOutput;
use tutti_fixtures::main_thread;
use tutti_rt::Mutex;

use assert_no_alloc::assert_no_alloc;
use tutti_buffers::FloatWidth;
use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_plugin_types::fake::{at_frame, unit_value};
use tutti_plugin_types::{
    AutomationMode, Control, Controller, Describe, EditorWindow, Event, Hand, HostOp, Lifecycle,
    Loaded, MaxBlock, Notice, ParameterChanges, PluginError, PluginIo, Prepare, ProcessContext,
    ProcessOutput, Processor, RenderMode, SampleRate, Samples, Stage,
};
use tutti_ump::{Address, MidiChannel, MidiGroup};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_vst2_host::{Vst2Controller, Vst2Loaded, Vst2Processor};
use tutti_vst2_test_plugin::{ProcessCapture, ProcessEntry};

static PROBE_LOCK: Mutex<()> = Mutex::new(());

const SAMPLE_RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;

// The opcodes the probe's trace records, as the SDK numbers them.
const MAINS_CHANGED: i32 = 12;
const START_PROCESS: i32 = 71;
const STOP_PROCESS: i32 = 72;

fn prepare() -> Prepare {
    Prepare::new(SAMPLE_RATE, MaxBlock::of(Samples::new(BLOCK)))
}

/// Holds the probe for one test and puts its switches, capture, trace and
/// close count back to their defaults.
struct Probe {
    _lock: MutexGuard<'static, ()>,
}

impl Probe {
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock();
        // SAFETY: the lock is held, so nothing else reads or writes the
        // environment.
        unsafe { std::env::remove_var("TUTTI_VST2_PROBE_F64") };
        call("tutti_vst2_probe_reset_capture");
        call("tutti_vst2_probe_reset_switches");
        call("tutti_vst2_probe_reset_trace");
        call("tutti_vst2_probe_reset_close_count");
        Probe { _lock: lock }
    }

    /// A probe that declares `effFlagsCanDoubleReplacing`.
    fn declaring_f64(&self) {
        // SAFETY: as in `acquire`.
        unsafe { std::env::set_var("TUTTI_VST2_PROBE_F64", "1") };
    }

    fn load(&self) -> Vst2Loaded {
        let loaded = Vst2Loaded::load(
            main_thread(),
            &tutti_fixtures::detached_work(),
            tutti_fixtures::vst2_probe(),
        )
        .expect("the plugin loads");
        // The plugin is built; clearing keeps the variable from outliving it.
        // SAFETY: as in `acquire`.
        unsafe { std::env::remove_var("TUTTI_VST2_PROBE_F64") };
        loaded
    }

    fn activate(&self) -> (Vst2Processor, Vst2Controller) {
        match self.load().activate(main_thread(), prepare()) {
            Ok(pair) => pair,
            Err(refused) => panic!("the plugin refused: {}", refused.reason),
        }
    }
}

/// Calls one of the probe's `extern "C"` exports through `dlopen`: the linked
/// rlib is a separate image with separate statics. The handle is leaked so the
/// image stays mapped and the write survives.
fn with_symbol<R>(symbol: &str, f: impl FnOnce(*mut std::ffi::c_void) -> R) -> R {
    let name = format!("{symbol}\0");
    let path = tutti_fixtures::vst2_probe();
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }.expect("re-open reference plugin");
    // SAFETY: the symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(name.as_bytes()) }.unwrap_or_else(|e| panic!("probe lacks {symbol}: {e}"));
    let r = f(*sym);
    std::mem::forget(lib);
    r
}

fn call(symbol: &str) {
    with_symbol(symbol, |p| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn() = unsafe { std::mem::transmute(p) };
        f();
    });
}

fn set_flag(symbol: &str, on: bool) {
    with_symbol(symbol, |p| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(bool) = unsafe { std::mem::transmute(p) };
        f(on);
    });
}

fn set_late_latency(samples: i32) {
    with_symbol("tutti_vst2_probe_set_late_latency", |p| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(i32) = unsafe { std::mem::transmute(p) };
        f(samples);
    });
}

fn fire_io_changed(latency: i32, inputs: i32) -> isize {
    with_symbol("tutti_vst2_probe_fire_io_changed", |p| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(i32, i32) -> isize = unsafe { std::mem::transmute(p) };
        f(latency, inputs)
    })
}

fn capture() -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    with_symbol("tutti_vst2_probe_capture", |p| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(p) };
        // SAFETY: `f` is the probe's capture export; it writes one
        // `ProcessCapture` through the pointer, and `cap` is a live local.
        unsafe { f(&mut cap) };
    });
    cap
}

fn close_count() -> u32 {
    with_symbol("tutti_vst2_probe_close_count", |p| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(p) };
        f()
    })
}

/// The lifecycle trace as `(opcode, value)`, oldest first.
fn trace() -> Vec<(i32, i32)> {
    let len = with_symbol("tutti_vst2_probe_trace_len", |p| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(p) };
        f()
    });
    (0..len)
        .map(|i| {
            let packed = with_symbol("tutti_vst2_probe_trace_at", |p| {
                // SAFETY: the probe exports this symbol with exactly this signature.
                let f: extern "C" fn(u32) -> i64 = unsafe { std::mem::transmute(p) };
                f(i)
            });
            ((packed >> 32) as i32, packed as i32)
        })
        .collect()
}

fn clear_trace() {
    call("tutti_vst2_probe_reset_trace");
}

/// What `trace()` holds of `opcode`, as values.
fn count_of(opcode: i32) -> usize {
    trace().iter().filter(|(op, _)| *op == opcode).count()
}

/// One silent stereo block through `processor` as the trait takes it; the
/// outputs come back.
fn render_f32(processor: &mut Vst2Processor, ctx: &ProcessContext<'_>) -> Vec<Vec<f32>> {
    let input = [0.0f32; BLOCK];
    let mut left = [9.0f32; BLOCK];
    let mut right = [9.0f32; BLOCK];
    let inputs = [input.as_slice(), input.as_slice()];
    let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
    let mut out = ProcessOutput::default();
    let mut io = PluginIo {
        buffer: InputOutput::from_slices(
            tutti_plugin_types::Samples::new(inputs[0].len()),
            &inputs,
            &mut outputs,
        )
        .expect("equal channels"),
        ctx,
        out: &mut out,
    };
    processor.process(&mut io).expect("the block renders");
    vec![left.to_vec(), right.to_vec()]
}

/// The probe's record of the program opcodes it received, in order: [`preset_event`]s.
fn preset_trace() -> Vec<i32> {
    let packed = with_symbol("tutti_vst2_probe_preset_trace", |p| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn() -> i32 = unsafe { std::mem::transmute(p) };
        f()
    });
    let len = with_symbol("tutti_vst2_probe_preset_trace_len", |p| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn() -> i32 = unsafe { std::mem::transmute(p) };
        f()
    });
    (0..len).map(|i| (packed >> (i * 2)) & 0b11).collect()
}

/// The program opcodes `preset_trace` records. Mirrored from the probe.
mod preset_event {
    pub const BEGIN: i32 = 1;
    pub const CHANGE: i32 = 2;
    pub const END: i32 = 3;
}

/// What one silent stereo block reports in its [`ProcessOutput`].
fn render_out(processor: &mut Vst2Processor, ctx: &ProcessContext<'_>) -> ProcessOutput {
    let input = [0.0f32; BLOCK];
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    let inputs = [input.as_slice(), input.as_slice()];
    let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
    let mut out = ProcessOutput::default();
    let mut io = PluginIo {
        buffer: InputOutput::from_slices(Samples::new(BLOCK), &inputs, &mut outputs)
            .expect("equal channels"),
        ctx,
        out: &mut out,
    };
    processor.process(&mut io).expect("the block renders");
    out
}

fn rejoin(processor: Vst2Processor, controller: Vst2Controller) -> Vst2Loaded {
    match Vst2Loaded::deactivate(main_thread(), processor, controller) {
        Ok(loaded) => loaded,
        Err(stuck) => panic!("the halves would not rejoin: {}", stuck.reason),
    }
}

/// **Activation splits the plugin and renders what the oracle says**: the probe
/// adds a tag per channel (1.0, 101.0, pinned in the probe's own tests) to its
/// input, so a swapped, duplicated or unwritten channel is arithmetically wrong.
#[test]
fn activation_splits_the_plugin_and_renders_through_the_trait() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    let out = render_f32(&mut processor, &ProcessContext::new());
    assert!(out[0].iter().all(|&v| v == 1.0), "{:?}", &out[0][..4]);
    assert!(out[1].iter().all(|&v| v == 101.0), "{:?}", &out[1][..4]);
    assert_eq!(capture().entry, ProcessEntry::Replacing);
    drop(rejoin(processor, controller));
}

/// **Describing a plugin does not activate it**: nothing is resumed, so a catalog scan
/// leaves the plugin's audio state alone. The control is a real activation, which the
/// capture sees, so a capture that never saw anything cannot pass.
#[test]
fn describing_a_plugin_does_not_resume_it() {
    let probe = Probe::acquire();
    let (processor, controller) = probe.activate();
    assert_eq!(capture().resume_count, 1, "the capture sees an activation");
    drop(rejoin(processor, controller));

    call("tutti_vst2_probe_reset_capture");
    let plugins =
        Vst2Loaded::describe(main_thread(), tutti_fixtures::vst2_probe()).expect("describes");
    let [descriptor] = plugins.as_slice() else {
        panic!("a plugin that is no shell is one plugin: {plugins:?}");
    };
    assert_eq!(descriptor.1.name, "Tutti VST2 Probe");
    assert_eq!(
        descriptor.0,
        tutti_plugin_types::PluginId::Vst2(tutti_vst2_test_plugin::PROBE_UNIQUE_ID as u32)
    );
    assert_eq!(
        capture().resume_count,
        0,
        "a description resumed the plugin"
    );
}

/// **A 64-bit activation tells the plugin so and enters `processReplacingF64`;
/// a 32-bit block for it is refused without entering the plugin.** The width the
/// plugin was told is the width it is called at.
#[test]
fn a_64_bit_activation_is_entered_at_64_and_refuses_a_32_bit_block() {
    let probe = Probe::acquire();
    probe.declaring_f64();
    let wanted = prepare().with_format(FloatWidth::F64);
    let (mut processor, controller) = probe
        .load()
        .activate(main_thread(), wanted)
        .expect("activates");
    assert_eq!(capture().set_precision_value, 1, "told 64-bit");

    let input = [0.0f64; BLOCK];
    let mut left = [0.0f64; BLOCK];
    let mut right = [0.0f64; BLOCK];
    let inputs = [input.as_slice(), input.as_slice()];
    let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    processor
        .process(&mut PluginIo {
            buffer: InputOutput::from_slices(
                tutti_plugin_types::Samples::new(inputs[0].len()),
                &inputs,
                &mut outputs,
            )
            .expect("equal channels"),
            ctx: &ctx,
            out: &mut out,
        })
        .expect("the 64-bit block renders");
    assert_eq!(capture().entry, ProcessEntry::ReplacingF64);
    assert!(left.iter().all(|&v| v == 1.0));

    let calls = capture().process_calls;
    let input32 = [0.0f32; BLOCK];
    let mut left32 = [0.0f32; BLOCK];
    let mut right32 = [0.0f32; BLOCK];
    let inputs32 = [input32.as_slice(), input32.as_slice()];
    let mut outputs32 = [left32.as_mut_slice(), right32.as_mut_slice()];
    let refused = processor.process(&mut PluginIo {
        buffer: InputOutput::from_slices(
            tutti_plugin_types::Samples::new(inputs32[0].len()),
            &inputs32,
            &mut outputs32,
        )
        .expect("equal channels"),
        ctx: &ctx,
        out: &mut out,
    });
    assert!(matches!(refused, Err(PluginError::Block(_))), "{refused:?}");
    assert_eq!(capture().process_calls, calls, "the plugin was not entered");
    drop(rejoin(processor, controller));
}

/// **A 32-bit activation refuses a 64-bit block without entering the plugin**:
/// no silent conversion in either direction.
#[test]
fn a_32_bit_activation_refuses_a_64_bit_block() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    let calls = capture().process_calls;

    let input = [0.0f64; BLOCK];
    let mut left = [0.0f64; BLOCK];
    let mut right = [0.0f64; BLOCK];
    let inputs = [input.as_slice(), input.as_slice()];
    let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    let refused = processor.process(&mut PluginIo {
        buffer: InputOutput::from_slices(
            tutti_plugin_types::Samples::new(inputs[0].len()),
            &inputs,
            &mut outputs,
        )
        .expect("equal channels"),
        ctx: &ctx,
        out: &mut out,
    });
    assert!(matches!(refused, Err(PluginError::Block(_))), "{refused:?}");
    assert_eq!(capture().process_calls, calls, "the plugin was not entered");
    assert!(left.iter().all(|&v| v == 0.0), "the outputs are untouched");
    drop(rejoin(processor, controller));
}

/// **No `Prepare` holds a block that is none**: a block of no frames is refused
/// where the block is built, so the host is never asked to activate at one (a
/// rate is a `SampleRate`, which cannot be none). A real one then activates and
/// is the first resume. (The server loaders' tests drive the refusal through
/// the load and `set_sample_rate` entry points.)
#[test]
fn no_prepare_holds_a_block_that_is_none() {
    let probe = Probe::acquire();
    let loaded = probe.load();
    assert!(MaxBlock::new(Samples::new(0)).is_none());
    let (processor, controller) = loaded
        .activate(main_thread(), prepare())
        .expect("a valid rate");
    assert_eq!(capture().resume_count, 1, "the first resume");
    drop(rejoin(processor, controller));
}

/// **Halves of two activations are stuck and returned whole.**
#[test]
fn halves_of_two_activations_are_stuck_and_returned() {
    let probe = Probe::acquire();
    let (p1, c1) = probe.activate();
    let (p2, c2) = probe.activate();
    let stuck = match Vst2Loaded::deactivate(main_thread(), p1, c2) {
        Ok(_) => panic!("halves of two activations must not rejoin"),
        Err(stuck) => stuck,
    };
    assert!(stuck.reason.to_string().contains("two activations"));
    // Whole again, and each pairs with its own.
    drop(rejoin(stuck.processor, c1));
    drop(rejoin(p2, stuck.controller));
}

/// **A Controller write reaches the plugin at once**, beside a Processor that
/// has not rendered, and a block's own automation lands at its head: the last
/// point of each queue, an id the plugin does not have skipped.
#[test]
fn a_controller_write_is_direct_and_automation_lands_at_the_block_head() {
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe.activate();
    let id = ParamId::new(1);
    controller
        .set_parameter(main_thread(), id, Normalized::of(0.5))
        .expect("a declared, writable parameter");
    assert_eq!(
        Control::parameter(&controller, main_thread(), id),
        Some(Normalized::of(0.5))
    );

    let mut changes = ParameterChanges::reserved(2, 4);
    changes.add_change(id, at_frame(0), unit_value(0.25));
    changes.add_change(id, at_frame(32), unit_value(0.75));
    changes.add_change(ParamId::new(u32::MAX), at_frame(0), unit_value(0.5));
    render_f32(&mut processor, &ProcessContext::new().params(&changes));
    assert_eq!(
        Control::parameter(&controller, main_thread(), id),
        Some(Normalized::of(0.75))
    );
    drop(rejoin(processor, controller));
}

/// **The block's automation is the host's write, not the plugin's edit**: a
/// plugin echoing `setParameter` through `audioMasterAutomate` from inside the
/// call reports nothing to the host.
#[test]
fn automation_is_a_host_write_and_its_echo_is_not_reported() {
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe.activate();
    set_flag("tutti_vst2_probe_set_echo_set_parameter", true);
    let mut changes = ParameterChanges::reserved(1, 2);
    changes.add_change(ParamId::new(0), at_frame(0), unit_value(0.5));
    render_f32(&mut processor, &ProcessContext::new().params(&changes));
    set_flag("tutti_vst2_probe_set_echo_set_parameter", false);
    let mut edits = Vec::new();
    controller.take_reported_edits(main_thread(), &mut edits);
    assert_eq!(edits, Vec::new());
    assert_eq!(controller.loaded().dropped_param_changes().get(), 0);
    drop(rejoin(processor, controller));
}

/// **A block that writes a parameter the plugin lacks reports the refusal in
/// `out`**, once per refused queue, and a block that writes only declared ones
/// reports none (`clear` resets the count). The node reads it to count the
/// writes the plugin refused.
#[test]
fn a_write_to_a_parameter_the_plugin_lacks_is_reported_in_the_output() {
    let probe = Probe::acquire();
    let (mut processor, _controller) = probe.activate();
    let input = [0.0f32; BLOCK];
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    let mut out = ProcessOutput::default();
    let mut block = |changes: &ParameterChanges, out: &mut ProcessOutput| {
        let ctx = ProcessContext::new().params(changes);
        let inputs = [input.as_slice(), input.as_slice()];
        let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
        let mut io = PluginIo {
            buffer: InputOutput::from_slices(
                tutti_plugin_types::Samples::new(inputs[0].len()),
                &inputs,
                &mut outputs,
            )
            .expect("equal channels"),
            ctx: &ctx,
            out,
        };
        processor.process(&mut io).expect("renders");
    };

    let mut lacking = ParameterChanges::reserved(2, 4);
    lacking.add_change(ParamId::new(0), at_frame(0), unit_value(0.25));
    lacking.add_change(ParamId::new(9_999), at_frame(0), unit_value(0.5));
    block(&lacking, &mut out);
    assert_eq!(out.params_refused(), 1, "the undeclared one only");

    out.clear();
    let mut declared = ParameterChanges::reserved(1, 4);
    declared.add_change(ParamId::new(0), at_frame(0), unit_value(0.75));
    block(&declared, &mut out);
    assert_eq!(out.params_refused(), 0);
}

/// **A block with automation, MIDI and a transport allocates nothing.**
#[test]
fn a_block_with_automation_midi_and_transport_does_not_allocate() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();

    let mut changes = ParameterChanges::reserved(2, 4);
    changes.add_change(ParamId::new(0), at_frame(0), unit_value(0.25));
    changes.add_change(ParamId::new(1), at_frame(16), unit_value(0.75));
    let midi = [(Offset::ZERO, midi::note_on(MidiChannel::of(1), 60, 100))];
    let ctx = ProcessContext::new().params(&changes).midi(&midi);

    let input = [0.0f32; BLOCK];
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    let mut out = ProcessOutput::default();
    {
        let inputs = [input.as_slice(), input.as_slice()];
        let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
        let mut io = PluginIo {
            buffer: InputOutput::from_slices(
                tutti_plugin_types::Samples::new(inputs[0].len()),
                &inputs,
                &mut outputs,
            )
            .expect("equal channels"),
            ctx: &ctx,
            out: &mut out,
        };
        // Warm: the first block may size what it keeps.
        processor.process(&mut io).expect("renders");
        assert_no_alloc(|| {
            processor.process(&mut io).expect("renders");
        });
    }
    drop(rejoin(processor, controller));
}

/// **The Controller serves the whole surface while the Processor renders**, with
/// no gate: every block is exact, and no `effStopProcess`, `effStartProcess` or
/// `effMainsChanged` crosses the seam while the Controller saves and loads
/// state, switches programs, writes and reads parameters and reads the wiring.
#[test]
fn the_controller_serves_the_surface_while_the_processor_renders() {
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe.activate();
    clear_trace();
    let stop = AtomicBool::new(false);
    let rendered = AtomicU32::new(0);
    let blocks = std::thread::scope(|scope| {
        let renderer = scope.spawn(|| {
            let ctx = ProcessContext::new();
            while !stop.load(Ordering::Acquire) {
                let out = render_f32(&mut processor, &ctx);
                let blocks = rendered.load(Ordering::Relaxed);
                assert!(out[0].iter().all(|&v| v == 1.0), "block {blocks}");
                assert!(out[1].iter().all(|&v| v == 101.0), "block {blocks}");
                rendered.store(blocks + 1, Ordering::Release);
            }
            rendered.load(Ordering::Acquire)
        });
        // Start beside a Processor that is already rendering.
        while rendered.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        for round in 0..40 {
            let saved = controller
                .save_state(main_thread(), tutti_plugin_types::StateUse::Project)
                .expect("saves");
            controller
                .load_state(main_thread(), &saved, tutti_plugin_types::StateUse::Project)
                .expect("loads");
            controller
                .set_parameter(
                    main_thread(),
                    ParamId::new(0),
                    Normalized::clamped(f64::from(round) / 40.0),
                )
                .expect("a declared, writable parameter");
            let _ = controller.parameter(main_thread(), ParamId::new(0));
            let _ = controller.wiring(main_thread());
            let _ = controller.presets(main_thread());
            let _ = controller.load_preset(main_thread(), &tutti_plugin_types::PresetId::Number(0));
            let _ = Controller::latency(&controller, main_thread());
            let _ = Controller::tail(&controller, main_thread());
        }
        // And leave it rendering after the last call.
        let seen = rendered.load(Ordering::Acquire);
        while rendered.load(Ordering::Acquire) < seen + 3 {
            std::thread::yield_now();
        }
        stop.store(true, Ordering::Release);
        renderer.join().expect("the renderer did not panic")
    });
    assert!(blocks > 0, "the Processor rendered beside the Controller");
    assert_eq!(count_of(STOP_PROCESS), 0);
    assert_eq!(count_of(START_PROCESS), 0);
    assert_eq!(count_of(MAINS_CHANGED), 0);
    drop(rejoin(processor, controller));
}

/// **A Processor that outlives its Controller is not the plugin's owner**: the
/// Controller's drop (the control side) stops, suspends and closes the plugin, once each; the
/// Processor then renders silence without calling the closed plugin, and its own drop, on
/// the audio thread, calls nothing and closes nothing. (The retirement queue this replaces
/// existed because the Processor could be the last holder; it holds only an audio handle
/// now, which shares the plugin's core and never owns it.)
#[test]
fn a_processor_that_outlives_its_controller_renders_silence_and_destroys_nothing() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    clear_trace();
    drop(controller);
    assert_eq!(close_count(), 1, "the control side closed the plugin");
    assert_eq!(count_of(STOP_PROCESS), 1, "stopped once");
    assert_eq!(count_of(MAINS_CHANGED), 1, "suspended once");
    let stop = trace().iter().position(|(op, _)| *op == STOP_PROCESS);
    let suspend = trace().iter().position(|(op, _)| *op == MAINS_CHANGED);
    assert!(stop < suspend, "in the SDK's order");

    clear_trace();
    let rendered = render_f32(&mut processor, &ProcessContext::new());
    assert!(
        rendered.iter().flatten().all(|s| *s == 0.0),
        "a closed plugin renders silence, not the probe's tags"
    );
    assert_eq!(trace(), [], "and a closed plugin is called by nothing");

    std::thread::spawn(move || drop(processor))
        .join()
        .expect("the drop did not panic");
    assert_eq!(close_count(), 1, "the audio thread closed nothing more");
    assert_eq!(trace(), [], "and called nothing in it");
}

/// **A block reports the MIDI it lost, once**: a message of a Group other than 0 reaches
/// VST2's one input nowhere (M2-104 2.1.2), and the block's `ProcessOutput` counts it, the next
/// block reporting only what is new.
#[test]
fn a_block_reports_the_midi_it_lost_once() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    let at = |group: u8| {
        (
            Offset::of(Samples::new(3), Samples::new(BLOCK)),
            midi::note_on(
                Address::new(MidiGroup::new(group).expect("a group"), MidiChannel::FIRST),
                60,
                0x4000,
            ),
        )
    };
    let events = [at(0), at(1), at(2)];
    let out = render_out(&mut processor, &ProcessContext::new().midi(&events));
    assert_eq!(out.input_refused(), 2, "the two of other Groups");
    let out = render_out(&mut processor, &ProcessContext::new());
    assert_eq!(out.input_refused(), 0, "nothing new");
    drop(rejoin(processor, controller));
}

/// **A refused block reports the events the plugin never heard, and the refusal itself**:
/// a closed plugin refuses every audio entry, so the block's MIDI is lost, and a block with
/// no MIDI still counts its refused entries.
#[test]
fn a_refused_block_reports_its_midi_and_its_refused_entries() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    let events = [(
        Offset::of(Samples::new(3), Samples::new(BLOCK)),
        midi::note_on(MidiChannel::FIRST, 60, 0x4000),
    )];
    let out = render_out(&mut processor, &ProcessContext::new().midi(&events));
    assert_eq!(out.input_refused(), 0, "a live plugin hears it");
    assert_eq!(out.entries_refused(), 0);

    drop(controller);
    let out = render_out(&mut processor, &ProcessContext::new().midi(&events));
    assert_eq!(out.input_refused(), 1, "a closed plugin hears nothing");
    assert!(out.entries_refused() > 0);
    let out = render_out(&mut processor, &ProcessContext::new());
    assert_eq!(out.input_refused(), 0, "no MIDI, none lost");
    assert!(out.entries_refused() > 0, "and the refusal still shows");
}

/// **A refused piece counts what staging handed it, once**: of a Group 1 note (dropped at
/// staging and counted there), a SysEx message of several UMPs (one staged event) and a note,
/// the refusal adds the two staged events; counting UMPs, or the Group 1 note again, would
/// give more than three.
#[test]
fn a_refused_piece_counts_each_lost_event_once() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    let at = |frame: usize, ump| (Offset::of(Samples::new(frame), Samples::new(BLOCK)), ump);
    let body: Vec<u8> = (1..=20).collect();
    let packets: Vec<_> = tutti_plugin_types::sysex_packets(MidiGroup::FIRST, &body)
        .expect("a message")
        .collect();
    assert!(packets.len() >= 3, "fixture: several UMPs");
    let group_one = Address::new(MidiGroup::new(1).expect("a group"), MidiChannel::FIRST);
    let mut events = vec![at(1, midi::note_on(group_one, 60, 0x4000))];
    events.extend(packets.into_iter().map(|ump| at(2, ump)));
    events.push(at(3, midi::note_on(MidiChannel::FIRST, 61, 0x4000)));

    drop(controller);
    let out = render_out(&mut processor, &ProcessContext::new().midi(&events));
    assert_eq!(
        out.input_refused(),
        3,
        "the Group 1 note, the SysEx, the note"
    );
}

/// **Losses counted between activations are reported by the next block**: a reset drops the
/// SysEx open across it, and a rejoin and a new activation between that and the next block do
/// not forget it.
#[test]
fn a_loss_between_activations_is_reported_once() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    let body: Vec<u8> = (1..=20).collect();
    let start = tutti_plugin_types::sysex_packets(MidiGroup::FIRST, &body)
        .expect("a message")
        .next()
        .expect("a START");
    let events = [(Offset::of(Samples::new(0), Samples::new(BLOCK)), start)];
    let out = render_out(&mut processor, &ProcessContext::new().midi(&events));
    assert_eq!(out.input_refused(), 0, "open across the block, not lost");

    processor.reset();
    let loaded = rejoin(processor, controller);
    let (mut processor, controller) = loaded
        .activate(main_thread(), prepare())
        .expect("activates again");
    let out = render_out(&mut processor, &ProcessContext::new());
    assert_eq!(out.input_refused(), 1, "the SysEx the reset dropped");
    let out = render_out(&mut processor, &ProcessContext::new());
    assert_eq!(out.input_refused(), 0, "told once");
    drop(rejoin(processor, controller));
}

/// **A snapshot's values are host writes, and its program is selected in a bracket**: the
/// plugin's `audioMasterAutomate` echo of each value is not reported as its own edit, and
/// the program is selected between `effBeginSetProgram` and `effEndSetProgram`.
#[test]
fn a_snapshots_values_are_host_writes_and_its_program_is_bracketed() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();
    let state = parameter_state(1, &[0.25, 0.75]);
    set_flag("tutti_vst2_probe_set_echo_set_parameter", true);
    Control::load_state(
        &mut loaded,
        main_thread(),
        &state,
        tutti_plugin_types::StateUse::Project,
    )
    .expect("restores");
    set_flag("tutti_vst2_probe_set_echo_set_parameter", false);

    assert_eq!(
        preset_trace(),
        [preset_event::BEGIN, preset_event::CHANGE, preset_event::END]
    );
    assert_eq!(
        Control::parameter(&loaded, main_thread(), ParamId::new(1)),
        Some(Normalized::of(0.75))
    );
    let mut edits = Vec::new();
    loaded.take_reported_edits(main_thread(), &mut edits);
    assert_eq!(edits, Vec::new(), "the echo is the host's own write");
}

/// **A saved value off `0..=1` refuses the whole restore**: NaN is not a value to clamp,
/// and no earlier value of the snapshot is written either.
#[test]
fn a_saved_value_that_is_not_a_normalized_one_refuses_the_restore() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();
    let before = Control::parameter(&loaded, main_thread(), ParamId::new(0));
    for bad in [f32::NAN, f32::INFINITY, -0.5, 1.5] {
        let state = parameter_state(0, &[0.9, bad]);
        let error = Control::load_state(
            &mut loaded,
            main_thread(),
            &state,
            tutti_plugin_types::StateUse::Project,
        )
        .expect_err("refused");
        assert!(matches!(error, PluginError::State(_)), "{error:?}");
        assert_eq!(
            Control::parameter(&loaded, main_thread(), ParamId::new(0)),
            before,
            "{bad}: nothing was written"
        );
    }
}

/// A `PRM\x01` state of `program` and `values`.
fn parameter_state(program: i32, values: &[f32]) -> Vec<u8> {
    let mut state = b"PRM\x01".to_vec();
    state.extend_from_slice(&program.to_le_bytes());
    state.extend_from_slice(&(values.len() as i32).to_le_bytes());
    for value in values {
        state.extend_from_slice(&value.to_le_bytes());
    }
    state
}

/// **No duplicate stop or suspend**, whichever way the plugin goes: a stop is
/// idempotent, a rejoin after one adds no second, and a Controller dropped last
/// stops and suspends once.
#[test]
fn stopping_twice_and_rejoining_sends_one_stop_and_one_suspend() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    clear_trace();
    Processor::stop(&mut processor);
    Processor::stop(&mut processor);
    drop(rejoin(processor, controller));
    assert_eq!(count_of(STOP_PROCESS), 1);
    assert_eq!(count_of(MAINS_CHANGED), 1);

    clear_trace();
    let (processor, controller) = probe.activate();
    clear_trace();
    drop(processor);
    drop(controller);
    assert_eq!(count_of(STOP_PROCESS), 1, "the last holder stopped it");
    assert_eq!(count_of(MAINS_CHANGED), 1, "and suspended it");
    let stop = trace().iter().position(|(op, _)| *op == STOP_PROCESS);
    let suspend = trace().iter().position(|(op, _)| *op == MAINS_CHANGED);
    assert!(stop < suspend, "in the SDK's order");
}

/// **The render mode needs no restart**: an offline activation answers
/// `audioMasterGetCurrentProcessLevel` 4, a live change to realtime answers 2
/// from the next block, and no suspend or resume crosses the seam for it.
#[test]
fn the_render_mode_changes_live_without_a_restart() {
    let probe = Probe::acquire();
    let wanted = prepare().with_mode(RenderMode::Offline);
    let (mut processor, mut controller) = probe
        .load()
        .activate(main_thread(), wanted)
        .expect("activates");
    render_f32(&mut processor, &ProcessContext::new());
    assert_eq!(capture().process_level, 4, "offline");
    assert!(controller.loaded().is_offline_render());

    assert!(controller.set_render_mode(main_thread(), RenderMode::Realtime));
    render_f32(&mut processor, &ProcessContext::new());
    assert_eq!(capture().process_level, 2, "realtime");
    let cap = capture();
    assert_eq!((cap.resume_count, cap.suspend_count), (1, 0), "no restart");
    drop(rejoin(processor, controller));
}

/// **The automation mode reaches the plugin both ways it can ask**: the session's mode
/// answers `audioMasterGetAutomationState` (off 1, read 2, write 3, read and write 4) and
/// sets `kVstAutomationReading` (bit 7) and `kVstAutomationWriting` (bit 6) in the next
/// block's time info, with no restart.
#[test]
fn the_automation_mode_reaches_the_plugin_as_state_and_time_info_flags() {
    const AUTOMATION_WRITING: i32 = 1 << 6;
    const AUTOMATION_READING: i32 = 1 << 7;
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe.activate();
    let transport = tutti_vst2_host::TransportInfo::new(tutti_plugin_types::SampleRate::SR_48K);
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..ProcessContext::new()
    };
    render_f32(&mut processor, &ctx);
    assert_eq!(capture().automation_state, 1, "off before any mode is set");

    for (mode, state, reading, writing) in [
        (AutomationMode::Reading, 2, true, false),
        (AutomationMode::Writing, 3, false, true),
        (AutomationMode::ReadWriting, 4, true, true),
        (AutomationMode::Off, 1, false, false),
    ] {
        controller.set_automation_state(main_thread(), mode);
        render_f32(&mut processor, &ctx);
        let cap = capture();
        assert_eq!(cap.automation_state, state, "{mode:?} state");
        assert_eq!(
            cap.time_flags & AUTOMATION_READING != 0,
            reading,
            "{mode:?}"
        );
        assert_eq!(
            cap.time_flags & AUTOMATION_WRITING != 0,
            writing,
            "{mode:?}"
        );
    }
    let cap = capture();
    assert_eq!((cap.resume_count, cap.suspend_count), (1, 0), "no restart");
    drop(rejoin(processor, controller));
}

/// **The host never claims `kVstSmpteValid`**: a plugin asking `audioMasterGetTime` with every
/// flag requested, the SMPTE one included, is answered a snapshot without it, because the
/// transport the host holds carries no SMPTE offset or frame rate to fill `smpteOffset` and
/// `smpteFrameRate` from. A host that set the flag with zeroed fields would tell the plugin
/// it is at SMPTE 0 in 24 fps.
#[test]
fn smpte_is_not_claimed_when_a_plugin_asks_for_it() {
    const SMPTE_VALID: i32 = 1 << 14;
    const PLAYING: i32 = 1 << 1;
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    let transport = tutti_vst2_host::TransportInfo::new(tutti_plugin_types::SampleRate::SR_48K)
        .with_playing(true);
    let ctx = ProcessContext {
        transport: Some(&transport),
        ..ProcessContext::new()
    };

    render_f32(&mut processor, &ctx);

    let cap = capture();
    assert!(cap.time_info_present);
    assert_ne!(cap.time_flags & PLAYING, 0, "the snapshot is the live one");
    assert_eq!(cap.time_flags & SMPTE_VALID, 0, "no SMPTE is claimed");
    drop(rejoin(processor, controller));
}

/// **A rejoin keeps what the audio side remembers**: the transport the plugin
/// last read stays the predecessor of the next block's, so a start after a
/// reconfigure is still an edge (`kVstTransportChanged`) against the stop
/// before it.
#[test]
fn a_rejoin_keeps_the_transport_the_plugin_reads() {
    const TRANSPORT_CHANGED: i32 = 1;
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    let stopped = tutti_vst2_host::TransportInfo::new(tutti_plugin_types::SampleRate::SR_48K);
    let playing = tutti_vst2_host::TransportInfo::new(tutti_plugin_types::SampleRate::SR_48K)
        .with_playing(true);
    let ctx = |transport| ProcessContext {
        transport: Some(transport),
        ..ProcessContext::new()
    };
    render_f32(&mut processor, &ctx(&stopped));
    assert!(capture().time_info_present);
    assert_eq!(
        capture().time_flags & TRANSPORT_CHANGED,
        0,
        "the first block has no predecessor"
    );

    let loaded = rejoin(processor, controller);
    let (mut processor, controller) = loaded
        .activate(main_thread(), prepare())
        .expect("activates again");
    render_f32(&mut processor, &ctx(&playing));
    let cap = capture();
    assert!(cap.time_info_present, "the plugin still reads the writer");
    assert_ne!(
        cap.time_flags & TRANSPORT_CHANGED,
        0,
        "the start is an edge against the stop before the rejoin"
    );
    drop(rejoin(processor, controller));
}

// --- the shell: `Lifecycle` over the plugin --------------------------------

struct Driven {
    lifecycle: Lifecycle<Vst2Loaded>,
    state: tutti_plugin_types::State,
    hand: Hand<Vst2Processor>,
    notices: Vec<Notice>,
}

impl Driven {
    fn activated(probe: &Probe) -> Self {
        let mut driven = Driven {
            lifecycle: Lifecycle::new(probe.load()),
            state: Lifecycle::<Vst2Loaded>::initial_state(),
            hand: Hand::local(),
            notices: Vec::new(),
        };
        driven.drive(Event::Op(HostOp::Activate(prepare())));
        assert_eq!(driven.state.stage(), Stage::Active);
        driven.notices.clear();
        driven
    }

    fn drive(&mut self, event: Event) {
        let state = std::mem::replace(&mut self.state, Lifecycle::<Vst2Loaded>::initial_state());
        let notices = &mut self.notices;
        self.state = self
            .lifecycle
            .drive(state, event, &mut self.hand, main_thread(), |n| {
                notices.push(n)
            });
    }

    /// What the shell's idle pass does: take the plugin's requests, feed them,
    /// then tick.
    fn pass(&mut self) {
        let mut asked = Vec::new();
        if let Lifecycle::Active { controller, .. } = &mut self.lifecycle {
            controller.take_requests(main_thread(), &mut asked);
        }
        for request in asked {
            self.drive(Event::Request(request));
        }
        self.drive(Event::Tick);
    }
}

/// **A plugin's `audioMasterIOChanged` restarts it, and the pins it now declares are
/// rendered**: the plugin raises its input count after activation (the S7 case), the core
/// deactivates and activates it again (`effMainsChanged` 0 then 1), re-reads the layout and
/// the latency, tells the host, and the new Processor renders the plugin's output instead of
/// the silence a buffer sized for the old pins would get.
#[test]
fn an_io_changed_callback_restarts_the_plugin_and_renders_its_new_pins() {
    let probe = Probe::acquire();
    let mut driven = Driven::activated(&probe);
    // The probe settles its latency at each resume, so the figure it announces is the one
    // the restart reads.
    set_late_latency(2048);
    clear_trace();
    assert_eq!(fire_io_changed(2048, 4), 1, "the host handles it");
    driven.pass();

    let mains: Vec<i32> = trace()
        .iter()
        .filter(|(op, _)| *op == MAINS_CHANGED)
        .map(|(_, value)| *value)
        .collect();
    assert_eq!(mains, [0, 1], "suspended, then resumed");
    assert!(
        driven
            .notices
            .iter()
            .any(|n| matches!(n, Notice::LatencyChanged(s) if *s == Samples::new(2048))),
        "{:?}",
        driven.notices
    );
    assert!(
        driven
            .notices
            .iter()
            .any(|n| matches!(n, Notice::IoChanged)),
        "{:?}",
        driven.notices
    );
    assert_eq!(driven.state.stage(), Stage::Active);
    let wiring = driven
        .lifecycle
        .control()
        .expect("a plugin")
        .wiring(main_thread());
    assert_eq!(
        wiring.inputs[0].channels.count(),
        4,
        "the layout was read again"
    );
    let processor = driven.lifecycle.processor().expect("the processor is back");
    let out = render_f32(processor, &ProcessContext::new());
    assert!(
        out[0].iter().all(|&v| v == 1.0) && out[1].iter().all(|&v| v == 101.0),
        "a plugin that grew its pins rendered silence: {:?}",
        &out[0][..4]
    );
}

/// **A reset asks the plugin to stop, and the next block starts it again**: `effStopProcess`
/// from the realtime side, no `effMainsChanged` (a main-thread opcode), and `effStartProcess`
/// with the next block. The opcodes are the header's (`aeffectx.h`, numbers 71 and 72 as the
/// trace records them), and the plugin sees them in that order.
#[test]
fn a_reset_stops_the_plugin_and_the_next_block_starts_it() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    render_f32(&mut processor, &ProcessContext::new());
    clear_trace();

    Processor::reset(&mut processor);
    assert_eq!(count_of(STOP_PROCESS), 1, "reset stopped the plugin");
    assert_eq!(count_of(START_PROCESS), 0, "and did not start it");
    assert_eq!(count_of(MAINS_CHANGED), 0, "without a suspend");

    render_f32(&mut processor, &ProcessContext::new());
    assert_eq!(count_of(START_PROCESS), 1, "the next block started it");
    drop(rejoin(processor, controller));
}

/// **A state carries the program, and a restore selects it before the chunk**: `effSetProgram`
/// (`aeffect.h`, opcode 2) then `effSetChunk` with `[index]` 1, the program form. The plugin
/// records what the host sent before acting on it. A restore into a fresh instance (which
/// starts at program 0) lands in program 1.
#[test]
fn a_state_carries_the_program_and_a_restore_selects_it_first() {
    const SET_PROGRAM: i32 = 2;
    const SET_CHUNK: i32 = 24;
    let probe = Probe::acquire();
    let mut live = probe.load();
    assert!(live.set_program(main_thread(), 1));
    let saved = Control::save_state(
        &mut live,
        main_thread(),
        tutti_plugin_types::StateUse::Project,
    )
    .expect("saves");
    drop(live);

    let mut fresh = probe.load();
    assert_eq!(fresh.current_program(main_thread()), 0, "a fresh instance");
    clear_trace();
    Control::load_state(
        &mut fresh,
        main_thread(),
        &saved,
        tutti_plugin_types::StateUse::Project,
    )
    .expect("restores");
    let sent: Vec<(i32, i32)> = trace()
        .into_iter()
        .filter(|(op, _)| matches!(*op, SET_PROGRAM | SET_CHUNK))
        .collect();
    assert_eq!(
        sent,
        [(SET_PROGRAM, 1), (SET_CHUNK, 1)],
        "the program, then the program chunk"
    );
    assert_eq!(fresh.current_program(main_thread()), 1);
}

/// **A plugin's `audioMasterSizeWindow` reaches the editor's resize slot**: the host answers 1
/// (`aeffectx.h`: "1 if supported"), and the size, `[index]` wide and `[value]` high, comes
/// out of the controller's `poll_editor_resize_request` once.
#[test]
fn a_size_window_request_reaches_the_resize_slot() {
    let probe = Probe::acquire();
    let (processor, mut controller) = probe.activate();
    let answered = with_symbol("tutti_vst2_probe_request_size_window", |p| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(i32, i32) -> isize = unsafe { std::mem::transmute(p) };
        f(900, 640)
    });
    assert_eq!(answered, 1, "supported");
    assert_eq!(
        EditorWindow::poll_editor_resize_request(&mut controller, main_thread())
            .map(|s| s.to_physical::<u32>(1.0)),
        Some(dpi::PhysicalSize::new(900, 640))
    );
    assert_eq!(
        EditorWindow::poll_editor_resize_request(&mut controller, main_thread()),
        None,
        "taken once"
    );
    drop(rejoin(processor, controller));
}

/// **A plugin's `audioMasterUpdateDisplay` raised during the activation is told
/// to the host**: the parameter values and titles are read again.
#[test]
fn a_display_update_raised_during_activation_reaches_the_host() {
    let probe = Probe::acquire();
    set_flag("tutti_vst2_probe_set_fire_update_display", true);
    let mut driven = Driven::activated(&probe);
    driven.pass();
    assert!(
        driven
            .notices
            .iter()
            .any(|n| matches!(n, Notice::ParamTitlesChanged)),
        "{:?}",
        driven.notices
    );
    assert!(
        driven
            .notices
            .iter()
            .any(|n| matches!(n, Notice::ParamValuesChanged)),
        "{:?}",
        driven.notices
    );
}

/// **A rate change is a rejoin and an activation, and the figure the plugin
/// recomputes is read live afterwards**: the Controller's latency follows the
/// plugin's, and the host is told.
#[test]
fn a_rate_change_that_moves_the_latency_is_told() {
    let probe = Probe::acquire();
    set_late_latency(0);
    let mut driven = Driven::activated(&probe);
    set_late_latency(1537);
    let wanted = prepare().with_rate(SampleRate::SR_96K);
    driven.drive(Event::Op(HostOp::Reconfigure(wanted)));
    assert_eq!(driven.state.stage(), Stage::Active);
    assert!(
        driven
            .notices
            .iter()
            .any(|n| matches!(n, Notice::LatencyChanged(s) if *s == Samples::new(1537))),
        "{:?}",
        driven.notices
    );
    assert_eq!(capture().sample_rate, 96_000.0);
}

/// The three types are `Send` for a server that builds them on one thread and
/// runs them on another, and none is `Sync`: the plugin is reached from one
/// thread at a time per half.
#[test]
fn the_types_are_send() {
    fn send<T: Send>() {}
    send::<Vst2Loaded>();
    send::<Vst2Processor>();
    send::<Vst2Controller>();
}
