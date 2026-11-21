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
        IpcConfig::at(tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source")),
        tutti_fixtures::executor(),
        &tutti_fixtures::detached_work(),
        &tutti_plugin_db::Loadable::uncatalogued(tutti_plugin_types::PluginRef::file(path)),
        tutti_plugin_types::SampleRate::SR_48K,
    )
    .unwrap_or_else(|e| panic!("{}: the server loads the plugin: {e:?}", F::NAME));
    let handle = client.handle();
    let pid = handle.server_pid().expect("the plugin lives in a server");
    assert_ne!(pid, std::process::id(), "a process other than this one");

    let state = handle
        .save_state(tutti_plugin_types::StateUse::Project)
        .expect("saves");
    let blob = patch_mode(F::extract_blob(&state), Mode::Level);
    handle
        .load_state(main_thread(), &F::embed_blob(&state, &blob))
        .expect("loads");

    let saved = handle
        .save_state(tutti_plugin_types::StateUse::Project)
        .expect("saves again");
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
) -> (
    Active<Ipc>,
    std::sync::Arc<std::sync::Mutex<Vec<ParamEdit>>>,
) {
    // SAFETY: set before any thread of this test reads it.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    let Launched { ipc, .. } = Ipc::launch(
        IpcConfig::at(tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source")),
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
    let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&heard);
    ipc.forward_events(Some(std::sync::Arc::new(move |event| {
        if let IpcEvent::ParameterChanged { edit } = event {
            sink.lock().unwrap().push(edit);
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
    while heard.lock().unwrap().len() < gesture_edits().len() {
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
        *heard.lock().unwrap(),
        gesture_edits(),
        "{}: the gesture across the wire",
        F::NAME
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
    let main = tutti_fixtures::main_thread();
    AnyThread::set_parameter_value(a.controller(), Param::Level.id(), Normalized::of(0.5))
        .expect("a declared parameter");
    // The read sends the write ahead of itself, as a flush.
    let _ = a.controller().parameter(main, Param::Level.id());
    tutti_fixtures::wait_until(
        "the gesture the flush raised to arrive",
        HangGuard::LIMIT,
        || heard.lock().unwrap().len() >= gesture_edits().len(),
    );
    assert_eq!(*heard.lock().unwrap(), gesture_edits());
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
