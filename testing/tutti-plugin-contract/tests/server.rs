//! The mode and the transcript through the plugin-server, a separate process:
//! the mode goes in as state, the transcript comes out as state, a parameter
//! reads as a parameter. The server picks the format from the path's
//! extension, so VST3 goes by its `.vst3` bundle.
//!
//! Needs `cargo build -p tutti-plugin-server --no-default-features --features
//! clap,vst3` first. VST2 is not here: the server does not serve it (its
//! `AEffect` fuses editor and audio), and its in-process
//! hosting is the contract's own `hostings`.

use std::path::PathBuf;
use tutti_fixtures::main_thread;

use tutti_buffers::FloatWidth;
use tutti_plugin::PluginClient;
use tutti_plugin_contract::spec::{self, Mode, Param};
use tutti_plugin_contract::{
    Active, BLOCK, Clap, Format, RATE, Render, Transcript, Vst3, Vst3Cpp, bundle_of, clap_path,
    gesture_edits, patch_mode, prepare, select, vst3_cpp_module, vst3_module,
};
use tutti_plugin_ipc::{Ipc, IpcConfig, IpcEvent, Launched};
use tutti_plugin_types::{
    Control, Editor, IpcError, IpcResult, LoadStage, MainThreadMarker, ParamEdit, SampleRate,
};
use tutti_units::Normalized;
use tutti_units::ParamId;

fn through_server<F: Format>(path: PathBuf) {
    // SAFETY: set before any thread of this test reads it.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    let client = PluginClient::new(
        IpcConfig::new(),
        tutti_fixtures::executor(),
        &tutti_fixtures::detached_work(),
        &tutti_plugin_db::Loadable::uncatalogued(tutti_plugin_types::PluginRef::file(path)),
        tutti_plugin_types::SampleRate::SR_48K,
    )
    .unwrap_or_else(|e| panic!("{}: the server loads the plugin: {e:?}", F::NAME));
    let handle = client.handle();
    let pid = handle.server_pid().expect("the plugin lives in a server");
    assert_ne!(pid, std::process::id(), "a process other than this one");

    // The tables the plugin declared ride the load reply.
    tutti_plugin_contract::assert_tables::<F>(handle.loaded(), "served");

    let state = handle
        .save_state(tutti_plugin_types::StateUse::Project)
        .expect("saves")
        .bytes;
    let blob = patch_mode(F::extract_blob(&state), Mode::Level);
    assert!(
        handle
            .load_state(main_thread(), &F::embed_blob(&state, &blob))
            .map(|loaded| loaded.expect("loads"))
            .is_ready()
    );

    let saved = handle
        .save_state(tutti_plugin_types::StateUse::Project)
        .expect("saves again")
        .bytes;
    let after = Transcript::parse(&F::extract_blob(&saved));
    assert_eq!(after.mode, Mode::Level.index(), "the mode crossed");
    // The mirror a session reads: its load-time read of the served plugin's values.
    let (mut mirror, read) = tutti_plugin_host::PluginParamValues::start(&handle);
    mirror.land(
        futures_lite::future::block_on(std::future::IntoFuture::into_future(read))
            .expect("the served plugin lists its parameters"),
    );
    let level = mirror
        .get(ParamId::new(Param::Level.id().get()))
        .flatten()
        .map(|v| v.get());
    assert_eq!(level, Some(0.5), "a parameter read across the boundary");

    // The wiring is asked of the server, not the load copy: the declared
    // controller table read across the boundary is the format's own.
    assert_eq!(
        handle.wiring(main_thread()).assignments,
        F::SURFACE.assignments,
        "{}: the declared table, read from the server now",
        F::NAME
    );

    // The app's controller entries and the plugin's own learn are asked of the
    // served plugin, which answers as the format does.
    let main = main_thread();
    assert_eq!(
        handle.set_app_midi_map(main, &[spec::APP_ENTRY]),
        F::SURFACE.app_map,
        "{}: who applies an app entry, across the boundary",
        F::NAME
    );
    let learn = handle.arm_plugin_midi_learn(main, true);
    match F::SURFACE.plugin_learn {
        spec::PluginLearn::Refused => {
            assert_eq!(
                learn,
                Err(tutti_plugin_types::PluginLearnError::Unsupported)
            );
        }
        spec::PluginLearn::MovesAux => assert_eq!(learn, Ok(())),
    }

    // A load the plugin cannot take answers the spec's variant, as in process.
    for id in spec::unlisted_ids(F::SURFACE.preset_ids) {
        assert_eq!(
            handle.load_preset(main, &id),
            std::task::Poll::Ready(Err(F::SURFACE.unlisted_preset)),
            "{}: the answer to the unlisted {id:?}, across the boundary",
            F::NAME
        );
    }

    // The conversion is the format's, asked across the boundary: each figure is
    // the spec's for the format (`spec::Tone`), never a line the client draws.
    let tone = F::SURFACE.tone;
    for (i, &n) in spec::TONE_AT.iter().enumerate() {
        let any = &handle;
        let plain = any.parameter_plain(Param::Tone.id(), Normalized::of(n));
        let want = tone.plain.map(|p| p[i]);
        match (plain, want) {
            (Some(got), Some(want)) => {
                assert!(
                    (got - want).abs() <= want.abs() * 1e-12,
                    "{}: tone plain at {n} across the boundary: got {got}, want {want}",
                    F::NAME
                );
                let back =
                    futures_lite::future::block_on(any.parameter_normalized(Param::Tone.id(), got))
                        .expect("the server answers")
                        .expect("a plain value converts back");
                assert!(
                    (back.get() - n).abs() < 1e-9,
                    "{}: {back:?} is not {n}",
                    F::NAME
                );
            }
            (None, None) => {}
            (got, want) => panic!("{}: tone plain at {n}: got {got:?}, want {want:?}", F::NAME),
        }
    }
}

#[test]
fn clap_mode_and_transcript_cross_the_process_boundary() {
    through_server::<Clap>(clap_path());
}

#[test]
fn vst3_mode_and_transcript_cross_the_process_boundary() {
    through_server::<Vst3>(bundle_of(&vst3_module()));
}

#[test]
fn vst3_cpp_mode_and_transcript_cross_the_process_boundary() {
    through_server::<Vst3Cpp>(bundle_of(&vst3_cpp_module()));
}

/// No editor in these tests: the served plugin is driven through `Ipc` alone.
fn no_editor(
    _: MainThreadMarker,
    _: &tutti_plugin_types::MainWorkHandle,
    plugin: &tutti_plugin_types::PluginRef,
    _: tutti_plugin_types::PluginFormat,
) -> IpcResult<Box<dyn Editor>> {
    Err(IpcError::LoadFailed {
        path: Some(plugin.path().to_path_buf()),
        stage: LoadStage::Opening,
        reason: "these tests load no editor".into(),
    })
}

/// A served plugin, active: its parameter edits are heard through the bridge's
/// listener (`Ipc::forward_events`), as the plugin node hears them.
fn served<F: Format>(
    path: PathBuf,
) -> (Active<Ipc>, std::sync::Arc<tutti_rt::Mutex<Vec<ParamEdit>>>) {
    // SAFETY: set before any thread of this test reads it.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    let Launched { ipc, .. } = Ipc::launch(
        IpcConfig::new(),
        &tutti_plugin_ipc::LoadRequest {
            origin: &tutti_plugin_types::PluginRef::file(path),
            format: F::FORMAT,
            rate: SampleRate::of(RATE),
            restore: None,
        },
        Some(tutti_plugin_ipc::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
        tutti_fixtures::executor(),
    )
    .unwrap_or_else(|e| panic!("{}: the server loads the plugin: {e:?}", F::NAME));
    let heard = std::sync::Arc::new(tutti_rt::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&heard);
    ipc.forward_events(Some(std::sync::Arc::new(move |event| {
        if let IpcEvent::ParameterChanged { edit } = event {
            sink.lock().push(edit);
        }
    })));
    let active = Active::activate(ipc, prepare(RATE, BLOCK, FloatWidth::F32))
        .unwrap_or_else(|(_, why)| panic!("{}: the served plugin activates: {why:?}", F::NAME));
    (active, heard)
}

/// A hang guard for a loop that runs until an event: generous, so a loaded
/// runner never trips it, and failing a loop whose event never comes.
struct HangGuard {
    until: tutti_platform::Instant,
}

impl HangGuard {
    const LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

    fn new() -> Self {
        Self {
            until: tutti_platform::Instant::now() + Self::LIMIT,
        }
    }

    fn check(&self, what: &str) {
        assert!(tutti_platform::Instant::now() < self.until, "{what}");
    }
}

/// A gesture the plugin makes in the server crosses the wire as the spec's four
/// edits, in order, begin to end, each with its value: the same figures the
/// in-process case reads ([`gesture_edits`]), here from the process the plugin
/// lives in.
fn a_served_gesture_arrives_in_order<F: Format>(path: PathBuf) {
    let (mut a, heard) = served::<F>(path);
    select::<F, _>(
        a.controller_mut(),
        if F::SURFACE.selects_editor {
            spec::EDITOR_TIDY
        } else {
            0
        },
        spec::NOTICE_GESTURE_ON_LOAD,
    );
    // CLAP's plugin says it from `process`, so blocks run while the host waits.
    let guard = HangGuard::new();
    while heard.lock().len() < gesture_edits().len() {
        guard.check("the gesture never arrived");
        a.silent(BLOCK);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Anything else the server sent before its answer to a control request has
    // arrived by then: the control connection is one ordered stream.
    a.silent(BLOCK);
    a.controller_mut()
        .save_state(
            tutti_fixtures::main_thread(),
            tutti_plugin_types::StateUse::Project,
        )
        .expect("saves");
    assert_eq!(
        *heard.lock(),
        gesture_edits(),
        "{}: the gesture across the wire",
        F::NAME
    );
}

/// **A host gesture crosses the wire and reaches the plugin in the server as it
/// reaches one in process**: begin, the latest value, end on a helper parameter,
/// nothing for an automatable one, and `NotDefined` at the call for it.
fn a_served_host_gesture_reaches_the_plugin_in_order<F: Format>(path: PathBuf) {
    use tutti_plugin_types::{AnyThread, EditOutcome, ParamRole};
    let (mut a, _heard) = served::<F>(path);
    let helper = spec::HELPER_ID;
    let level = Param::Level.id();
    let c = a.controller();
    assert_eq!(
        AnyThread::edit(c, ParamEdit::Begin(level), ParamRole::Other),
        EditOutcome::NotDefined,
        "{}: an automatable parameter has no host gesture, said at the call",
        F::NAME
    );
    for edit in [
        ParamEdit::Begin(helper),
        ParamEdit::change(helper, Normalized::of(0.25)),
        ParamEdit::change(helper, Normalized::of(0.75)),
        ParamEdit::End(helper),
    ] {
        assert_eq!(
            AnyThread::edit(c, edit, ParamRole::Helper),
            EditOutcome::Queued
        );
    }
    // The control connection is one ordered stream: the server has posted the
    // steps before it answers the save, and drains them on its main loop.
    let guard = HangGuard::new();
    let seen: Vec<_> = loop {
        guard.check("the host gesture never reached the plugin");
        let t = tutti_plugin_contract::transcript::<F, _>(a.controller_mut());
        let seen: Vec<_> = t
            .events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    tutti_plugin_contract::Kind::HostBegin
                        | tutti_plugin_contract::Kind::HostValue
                        | tutti_plugin_contract::Kind::HostEnd
                )
            })
            .map(|e| (e.kind, e.id, e.value))
            .collect();
        if seen
            .last()
            .is_some_and(|&(kind, ..)| kind == tutti_plugin_contract::Kind::HostEnd)
        {
            break seen;
        }
        a.silent(BLOCK);
    };
    // The server drains between the steps it has received, so the values it
    // heard are some of those sent, the last of them the final one; the begin
    // is first and the end last.
    let id = helper.get();
    let (begin, rest) = seen.split_first().expect("a begin");
    let (end, values) = rest.split_last().expect("an end");
    assert_eq!(*begin, (tutti_plugin_contract::Kind::HostBegin, id, 0.0));
    assert_eq!(*end, (tutti_plugin_contract::Kind::HostEnd, id, 0.0));
    assert!(
        values
            .iter()
            .all(|&(kind, i, _)| kind == tutti_plugin_contract::Kind::HostValue && i == id),
        "{}: only the helper's values between the ends: {seen:?}",
        F::NAME
    );
    assert_eq!(
        values.last().map(|v| v.2),
        Some(0.75),
        "{}: the last value sent is the last heard",
        F::NAME
    );
}

#[test]
fn vst3_host_gesture_reaches_the_served_plugin_in_order() {
    a_served_host_gesture_reaches_the_plugin_in_order::<Vst3>(bundle_of(&vst3_module()));
}

#[test]
fn vst3_cpp_host_gesture_reaches_the_served_plugin_in_order() {
    a_served_host_gesture_reaches_the_plugin_in_order::<Vst3Cpp>(bundle_of(&vst3_cpp_module()));
}

#[test]
fn clap_host_gesture_is_not_defined_for_a_served_plugin() {
    use tutti_plugin_types::{AnyThread, EditOutcome, ParamRole};
    let (a, _heard) = served::<Clap>(clap_path());
    assert_eq!(
        AnyThread::edit(
            a.controller(),
            ParamEdit::Begin(Param::Level.id()),
            ParamRole::Other
        ),
        EditOutcome::NotDefined,
        "CLAP defines no host gesture (ext/params.h, scenario II)"
    );
}

/// **What a plugin says while it takes parameters outside a block crosses the
/// wire as what it says in a block**: CLAP's plugin raises its gesture from
/// `params.flush` too, and no block runs here. A write and a read of the plugin
/// send the write as a flush; the gesture it raises arrives in order.
#[test]
fn clap_gesture_from_a_flush_crosses_the_process_boundary_in_order() {
    use tutti_plugin_types::AnyThread;
    let (mut a, heard) = served::<Clap>(clap_path());
    select::<Clap, _>(a.controller_mut(), 0, spec::NOTICE_GESTURE_ON_LOAD);
    AnyThread::set_parameter_value(a.controller(), Param::Level.id(), Normalized::of(0.5))
        .expect("a declared parameter");
    // The read, asked as an answer, sends the write ahead of itself, as a flush.
    drop(AnyThread::parameter_value(
        a.controller(),
        Param::Level.id(),
    ));
    tutti_fixtures::wait_until(
        "the gesture the flush raised to arrive",
        HangGuard::LIMIT,
        || heard.lock().len() >= gesture_edits().len(),
    );
    assert_eq!(*heard.lock(), gesture_edits());
}

/// A parameter value keeps its width across the wire: 0.3, which an `f32`
/// cannot hold, is read back as sent (the wire carried an `f32` before version
/// 31, which returned 0.30000001192092896).
fn a_served_value_keeps_its_f64_width<F: Format>(path: PathBuf) {
    const SENT: f64 = 0.3;
    assert_ne!(
        f64::from(SENT as f32),
        SENT,
        "the value must not fit an f32, or the test cannot tell"
    );
    let (mut a, _) = served::<F>(path);
    let main = tutti_fixtures::main_thread();
    let before = a.controller().parameter(main, Param::Level.id());
    assert_ne!(
        before.map(Normalized::get),
        Some(SENT),
        "{}: a write to see",
        F::NAME
    );
    a.controller_mut()
        .set_parameter(main, Param::Level.id(), Normalized::of(SENT))
        .expect("a declared, writable parameter");
    // A control-side write reaches the plugin at the head of a block after the
    // server's main thread has read it, not in step with a particular block:
    // it travels on the control connection and the blocks on the audio one.
    // So blocks run until the value has moved, and the
    // value it moved to is the one checked.
    let guard = HangGuard::new();
    let mut got = before;
    while got == before {
        guard.check("the value never moved");
        a.silent(BLOCK);
        got = a.controller().parameter(main, Param::Level.id());
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(
        got.map(Normalized::get),
        Some(SENT),
        "{}: the value read back",
        F::NAME
    );
}

/// **A write to the read-only parameter on the realtime route is not applied by
/// the server**: `AnyThread::set_parameter_value` sends it with the next block
/// (the client checks only that the plugin declares it), and the server's own
/// `Control::set_parameter` refuses it. The plugin's record of what the host sent has an event for `level`, the
/// writable write sent the same way, and none for the meter.
fn a_read_only_write_on_the_realtime_route_is_not_applied<F: Format>(path: PathBuf) {
    use tutti_plugin_contract::Kind;
    use tutti_plugin_types::AnyThread;

    let (mut a, _) = served::<F>(path);
    let main = tutti_fixtures::main_thread();
    AnyThread::set_parameter_value(
        a.controller(),
        Param::Meter.id(),
        Normalized::of(spec::WRITE),
    )
    .expect("a declared parameter");
    AnyThread::set_parameter_value(
        a.controller(),
        Param::Level.id(),
        Normalized::of(spec::WRITE),
    )
    .expect("a declared parameter");
    // The writes ride the next block, or a read's flush on the audio
    // connection: blocks run until the writable one has landed.
    let guard = HangGuard::new();
    let mut level = a.controller().parameter(main, Param::Level.id());
    while level.map(Normalized::get) != Some(spec::WRITE) {
        guard.check("the writable write never landed");
        a.silent(BLOCK);
        level = a.controller().parameter(main, Param::Level.id());
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(
        level.map(Normalized::get),
        Some(spec::WRITE),
        "{}: the positive control",
        F::NAME
    );
    // The read can come back before a block has run on the plugin's side, so the
    // record may lack the write yet: one more block, then the record.
    a.silent(BLOCK);
    let state = a
        .controller_mut()
        .save_state(main, tutti_plugin_types::StateUse::Project)
        .expect("saves");
    let t = Transcript::parse(&F::extract_blob(&state));
    assert!(
        t.of(Kind::Param).any(|e| e.id == Param::Level.id().get()),
        "{}: the record shows the writable write",
        F::NAME
    );
    assert!(
        t.of(Kind::Param).all(|e| e.id != Param::Meter.id().get()),
        "{}: the server applied a read-only write: {:?}",
        F::NAME,
        t.events
    );
}

#[test]
fn clap_read_only_write_on_the_realtime_route_is_not_applied() {
    a_read_only_write_on_the_realtime_route_is_not_applied::<Clap>(clap_path());
}

#[test]
fn vst3_read_only_write_on_the_realtime_route_is_not_applied() {
    a_read_only_write_on_the_realtime_route_is_not_applied::<Vst3>(bundle_of(&vst3_module()));
}

#[test]
fn vst3_cpp_read_only_write_on_the_realtime_route_is_not_applied() {
    a_read_only_write_on_the_realtime_route_is_not_applied::<Vst3Cpp>(
        bundle_of(&vst3_cpp_module()),
    );
}

#[test]
fn clap_gesture_crosses_the_process_boundary_in_order() {
    a_served_gesture_arrives_in_order::<Clap>(clap_path());
}

#[test]
fn vst3_gesture_crosses_the_process_boundary_in_order() {
    a_served_gesture_arrives_in_order::<Vst3>(bundle_of(&vst3_module()));
}

#[test]
fn vst3_cpp_gesture_crosses_the_process_boundary_in_order() {
    a_served_gesture_arrives_in_order::<Vst3Cpp>(bundle_of(&vst3_cpp_module()));
}

#[test]
fn clap_value_keeps_its_f64_width_across_the_process_boundary() {
    a_served_value_keeps_its_f64_width::<Clap>(clap_path());
}

#[test]
fn vst3_value_keeps_its_f64_width_across_the_process_boundary() {
    a_served_value_keeps_its_f64_width::<Vst3>(bundle_of(&vst3_module()));
}

#[test]
fn vst3_cpp_value_keeps_its_f64_width_across_the_process_boundary() {
    a_served_value_keeps_its_f64_width::<Vst3Cpp>(bundle_of(&vst3_cpp_module()));
}

/// The shared plugin's `IMidiMapping` (CC 74 on channel 0 to `level`) applied by
/// the plugin node, with the real plugin: a MIDI 2.0 Control Change reaches it as
/// a `level` point of the word over the full 32-bit scale (M2-115), and the plugin
/// holds that level.
mod node_midi_map {
    use super::*;
    use tutti_buffers::{MaxBlock, Planes};
    use tutti_graph::{
        EventCapacity, EventInletNode, EventInletWriter, GraphBuilder, Renderer, Stamp,
    };
    use tutti_plugin::{Bound, PluginClient};
    use tutti_plugin_contract::Kind;
    use tutti_plugin_host::PluginHandle;
    use tutti_plugin_types::{ChannelLayout, StateUse};
    use tutti_units::Samples;

    /// The word of the Control Change sent: a quarter of the 32-bit scale.
    const WORD: u32 = 0x4000_0000;

    fn expected() -> f64 {
        f64::from(WORD) / f64::from(u32::MAX)
    }

    fn cc74() -> tutti_ump::Ump {
        tutti_fixtures::midi::cc(
            tutti_ump::MidiChannel::of(0),
            tutti_ump::CCNumber::BRIGHTNESS,
            WORD,
        )
    }

    /// What the plugin recorded of `level`: the points it was handed and the
    /// level it holds.
    fn level_points<F: Format>(handle: &PluginHandle) -> (Vec<f64>, f64) {
        let state = handle.save_state(StateUse::Project).expect("saves").bytes;
        let t = Transcript::parse(&F::extract_blob(&state));
        let points = t
            .of(Kind::Param)
            .filter(|e| e.id == Param::Level.id().get())
            .map(|e| e.value)
            .collect();
        (points, t.level)
    }

    /// A bound client as the only node of a graph, with an event inlet.
    fn graph(client: PluginClient<Bound>) -> (Renderer, EventInletWriter, usize) {
        let (inputs, outputs) = (client.inputs(), client.outputs());
        let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
        let mut g = GraphBuilder::new(layout(inputs), layout(outputs));
        let (key, _controls) = g.add_with_controls(client);
        g.pipe_input(key).pipe_output(key);
        let (inlet, writer) = EventInletNode::pair(EventCapacity::of(256));
        let from = g.add(inlet);
        g.connect_events(from, 0, key, 0);
        let prepare =
            tutti_graph::Prepare::new(SampleRate::of(RATE), MaxBlock::of(Samples::new(BLOCK)));
        (
            g.renderer(prepare).expect("a one-plugin graph compiles"),
            writer,
            inputs,
        )
    }

    fn render(renderer: &mut Renderer, inputs: usize) {
        let silence = vec![0.0f32; BLOCK];
        let refs: Vec<&[f32]> = (0..inputs).map(|_| silence.as_slice()).collect();
        renderer.render_input(Planes::new(&refs).expect("planes"));
    }

    /// Sends the controller and renders until the plugin has recorded a `level`
    /// point: the served pipeline delivers a block or two late.
    fn drive<F: Format>(client: PluginClient<Bound>, handle: &PluginHandle) -> (Vec<f64>, f64) {
        let (mut renderer, mut writer, inputs) = graph(client);
        writer.push(Stamp::NextBlock, cc74()).expect("room");
        let guard = HangGuard::new();
        loop {
            guard.check("the controller never reached the plugin as a parameter");
            render(&mut renderer, inputs);
            let (points, level) = level_points::<F>(handle);
            if !points.is_empty() {
                return (points, level);
            }
        }
    }

    fn unpumped<F: Format>(path: PathBuf) {
        // SAFETY: set before any thread of this test reads it.
        unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
        let client = PluginClient::new(
            IpcConfig::new(),
            tutti_fixtures::executor(),
            &tutti_fixtures::detached_work(),
            &tutti_plugin_db::Loadable::uncatalogued(tutti_plugin_types::PluginRef::file(path)),
            SampleRate::SR_48K,
        )
        .unwrap_or_else(|e| panic!("{}: the server loads the plugin: {e:?}", F::NAME));
        let handle = client.handle();
        // The live node, then a fork of it: neither is ever pumped, so the
        // table is the load-time wiring's.
        let fork = client
            .fork_instance(tutti_fixtures::tempo())
            .expect("forks");
        let fork_handle = fork.handle();
        let live = client.bind(tutti_fixtures::tempo());
        let (points, level) = drive::<F>(live, &handle);
        assert!(
            points.iter().all(|p| (p - expected()).abs() < 1e-6),
            "{}: served, the points are {points:?}, want {}",
            F::NAME,
            expected()
        );
        assert!(
            (level - expected()).abs() < 1e-6,
            "{}: the plugin holds {level}",
            F::NAME
        );
        let (points, level) = drive::<F>(fork, &fork_handle);
        assert!(
            points.iter().all(|p| (p - expected()).abs() < 1e-6),
            "{}: a fork that is never pumped: {points:?}",
            F::NAME
        );
        assert!(
            (level - expected()).abs() < 1e-6,
            "{}: the fork holds {level}",
            F::NAME
        );
    }

    /// **A served fork is handed the app's entries by its own plugin**: an entry
    /// the app set on the live node (CC 21 on channel 2 to `level`) moves the
    /// fork's `level`, a table only the fork's own answer publishes.
    #[test]
    fn a_served_fork_hands_the_app_entries_to_its_own_plugin() {
        // SAFETY: set before any thread of this test reads it.
        unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
        let client = PluginClient::new(
            IpcConfig::new(),
            tutti_fixtures::executor(),
            &tutti_fixtures::detached_work(),
            &tutti_plugin_db::Loadable::uncatalogued(tutti_plugin_types::PluginRef::file(
                bundle_of(&vst3_module()),
            )),
            SampleRate::SR_48K,
        )
        .expect("the server loads the plugin");
        let handle = client.handle();
        let entry = tutti_plugin_types::MidiAssignment {
            channel: Some(tutti_ump::MidiChannel::of(2)),
            source: tutti_plugin_types::MidiSource::ControlChange(tutti_ump::U7::of(21)),
            param: Param::Level.id(),
        };
        handle.set_app_midi_map(main_thread(), &[entry]);
        let fork = client
            .fork_instance(tutti_fixtures::tempo())
            .expect("forks");
        let fork_handle = fork.handle();
        let (mut renderer, mut writer, inputs) = graph(fork);
        writer
            .push(
                Stamp::NextBlock,
                tutti_fixtures::midi::cc(
                    tutti_ump::MidiChannel::of(2),
                    tutti_ump::CCNumber::new(21).expect("a controller"),
                    WORD,
                ),
            )
            .expect("room");
        let guard = HangGuard::new();
        loop {
            guard.check("the app's entry never moved the fork's level");
            render(&mut renderer, inputs);
            let (points, _) = level_points::<Vst3>(&fork_handle);
            if let Some(point) = points.first() {
                assert!((point - expected()).abs() < 1e-6, "{points:?}");
                break;
            }
        }
    }

    #[test]
    fn a_served_vst3_node_translates_its_declared_controller_from_its_first_block() {
        unpumped::<Vst3>(bundle_of(&vst3_module()));
    }

    #[test]
    fn a_served_vst3_cpp_node_translates_its_declared_controller_from_its_first_block() {
        unpumped::<Vst3Cpp>(bundle_of(&vst3_cpp_module()));
    }

    /// In process, through `load_client`: the same figure.
    #[test]
    fn an_in_process_vst3_node_translates_its_declared_controller() {
        use tutti_graph::contract::Direct;
        let main = main_thread();
        let (client, handle) = tutti_plugin_host::load_client::<tutti_vst3_host::Vst3Loaded, f32>(
            main,
            &tutti_fixtures::pumped_work(),
            &tutti_plugin_types::PluginRef::file(bundle_of(&vst3_module())),
            SampleRate::SR_48K,
        )
        .expect("loads");
        let mut unit = Direct::new(
            client.bind(tutti_fixtures::tempo()),
            SampleRate::SR_48K,
            BLOCK,
        );
        let port = tutti_graph::Node::shape(&*unit.node)
            .event_input(0)
            .expect("an event input");
        unit.events(
            port,
            &[tutti_graph::Event::midi(
                tutti_graph::Offset::of(Samples::new(3), Samples::new(BLOCK)),
                cc74(),
            )],
        );
        unit.block();
        let (points, level) = level_points::<Vst3>(&handle);
        assert_eq!(points.len(), 1, "one point: {points:?}");
        assert!((points[0] - expected()).abs() < 1e-6);
        assert!(
            (level - expected()).abs() < 1e-6,
            "the plugin holds {level}"
        );
    }

    /// In process: a host write to the read-only parameter passes the call (the
    /// slots hold every id the plugin lists), and the Processor refuses it when
    /// the block takes it, so the handle's `refused_edits` reads 1 after one
    /// block. A write refused at the call is not counted.
    #[test]
    fn an_in_process_vst3_node_counts_a_write_to_a_read_only_parameter_refused() {
        use tutti_graph::contract::Direct;
        let (client, handle) = tutti_plugin_host::load_client::<tutti_vst3_host::Vst3Loaded, f32>(
            main_thread(),
            &tutti_fixtures::pumped_work(),
            &tutti_plugin_types::PluginRef::file(bundle_of(&vst3_module())),
            SampleRate::SR_48K,
        )
        .expect("loads");
        let mut unit = Direct::new(
            client.bind(tutti_fixtures::tempo()),
            SampleRate::SR_48K,
            BLOCK,
        );
        assert_eq!(handle.refused_edits(), 0);
        handle
            .send_parameter(
                tutti_plugin_contract::spec::Param::Meter.id(),
                tutti_units::Normalized::of(1.0),
            )
            .expect("the slots hold every listed id");
        unit.block();
        assert_eq!(handle.refused_edits(), 1);
    }
}
