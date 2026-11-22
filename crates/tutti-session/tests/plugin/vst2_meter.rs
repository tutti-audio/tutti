//! **An in-process VST2 plugin is given the project meter**, as an
//! out-of-process one is: `Session::apply` captures its meter cell on
//! the way in and installs the project's (the click settings') through it,
//! and the plugin's `audioMasterGetTime` answers in the project's 7/8, read
//! back out of the reference VST2 probe.
//!

use crate::loading::{LoadPlugin, Spec};
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_session::MainOutput;

use tutti_music::{BeatsPerBar, MeterChange, MeterMap, NoteValue, TimeSignature};
use tutti_plugin_types::PluginRef;
use tutti_session::Session;
use tutti_units::{Beat, Bpm, SampleRate};
use tutti_vst2_test_plugin::ProcessCapture;

const RATE: SampleRate = SampleRate::SR_48K;

fn vst2_probe() -> std::path::PathBuf {
    tutti_fixtures::vst2_probe().to_path_buf()
}

/// What `audioMasterGetTime` answered on the probe's last `process`: read from
/// the image the host loaded (the linked rlib's statics are another image's).
fn read_capture() -> ProcessCapture {
    let path = vst2_probe();
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(&path) }
        .unwrap_or_else(|e| panic!("re-open the reference plugin at {path:?}: {e}"));
    let mut cap = ProcessCapture::empty();
    {
        // SAFETY: the probe exports this as
        // `unsafe extern "C" fn(*mut ProcessCapture) -> bool`.
        let f: libloading::Symbol<unsafe extern "C" fn(*mut ProcessCapture) -> bool> =
            unsafe { lib.get(b"tutti_vst2_probe_capture\0") }.expect("the probe's capture");
        // SAFETY: as above; `cap` outlives the call.
        unsafe { f(&mut cap) };
    }
    // Never unloaded: dropping the handle could unmap the image the host
    // still runs.
    std::mem::forget(lib);
    cap
}

/// **The session hands an in-process VST2 the project's meter.** The project
/// is in 7/8 from beat 0; one frame is rendered at beat 3.5 (bar 2 of 7/8
/// starts there) and the plugin's time info says 7/8, with the bar starting at
/// beat 3.5.
#[test]
fn an_in_process_vst2_plugin_is_given_the_project_meter() {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(RATE)
        .expect("a detached session builds");
    s.set(
        s.transport_handle()
            .meter()
            .to(MeterMap::new([MeterChange::new(
                Beat::new(0.0),
                TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH),
            )])),
    )
    .unwrap();
    let main = tutti_fixtures::main_thread();
    let node = s
        .load_plugin_blocking(main, Spec::new(PluginRef::file(vst2_probe())))
        .expect("the in-process VST2 loads");
    s.graph().connect(node, MainOutput).unwrap();
    // The meter written above and the wiring take effect at an update.
    let _ = s.update_on(main);

    let hosted = s.plugin_view(node).expect("hosted");
    assert!(
        hosted.diagnostics().follows_project_meter(),
        "the session gave the in-process node its meter cell"
    );
    assert_eq!(
        hosted.diagnostics().server_pid(),
        None,
        "an in-process plugin has no server"
    );

    // One frame at beat 3.5: bar 2 of 7/8 starts there.
    let mut out = [0.0f32; 2];
    s.render_frame_at(
        &tutti_graph::TransportPosition::new(true, Bpm::of(120.0), Beat::new(3.5), None),
        InterleavedMut::one_frame(&mut out).unwrap(),
    );
    let cap = read_capture();
    assert!(cap.time_info_present, "the plugin was answered");
    assert_eq!(
        (cap.time_sig_numerator, cap.time_sig_denominator),
        (7, 8),
        "the project's meter, not 4/4"
    );
    assert!((cap.time_bar_start_pos - 3.5).abs() < 1e-9);
}
