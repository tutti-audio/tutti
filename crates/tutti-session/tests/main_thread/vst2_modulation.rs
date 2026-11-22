//! Modulation lanes on an in-process VST2 plugin's parameters (doc 014 §8.2,
//! §12.5): a plugin target is a lane on the plugin's event input, its knob
//! its own, started at the plugin's value and moved by the plugin's GUI.
//!
//! The plugin is the reference VST2 probe, in process.

use crate::loading::{LoadPlugin, Spec};
use crate::param_reads::ParamReads as _;
use std::time::Duration;
use tutti_buffers::PlanesMut;
use tutti_platform::Instant;
use tutti_session::MainOutput;

use tutti_buffers::ChannelLayout;
use tutti_graph::NodeKey;
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
use tutti_plugin_types::PluginRef;
use tutti_session::{
    DetachedSession, EventInputPort, Feeder, PHASES, ParamRef, PhaseCx, RouteSpec, Session,
    SessionEvent,
};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{Depth, SampleRate};

const RATE: SampleRate = SampleRate::SR_48K;

fn vst2_probe() -> std::path::PathBuf {
    tutti_fixtures::vst2_probe().to_path_buf()
}

/// A detached stereo session at 48 kHz with the probe loaded in process, on
/// the outputs, its parameter values read.
fn session_with_probe() -> (DetachedSession, NodeKey) {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(RATE)
        .expect("a detached session builds");
    let node = s
        .load_plugin_blocking(
            tutti_fixtures::main_thread(),
            Spec::new(PluginRef::file(vst2_probe())),
        )
        .expect("the in-process VST2 loads");
    s.graph().connect(node, MainOutput).unwrap();
    let _ = s.update();
    assert!(
        s.plugin_view(node).is_ok_and(|p| p.params().is_ready()),
        "an in-process plugin's values are read as it opens"
    );
    (s, node.erase())
}

/// One update at `now`, every phase, as a frame of a host.
fn update(s: &mut DetachedSession, now: Instant) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, PhaseCx::at(now), &mut out);
    }
    out
}

/// Updates until `target` has a lane, and its knob.
fn knob_of(s: &mut DetachedSession, target: ParamRef) -> f64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let now = Instant::now();
    loop {
        assert!(
            !s.mod_state()
                .problems()
                .any(|p| matches!(p, tutti_session::Problem::Refused { .. })),
            "the declarations apply"
        );
        if let Some((_, lane)) = s.mod_state().lane(target.key()) {
            return lane.value().expect("a plugin lane has a knob");
        }
        assert!(Instant::now() < deadline, "no lane on {target:?}");
        update(s, now);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Calls the one-argument switch `name` the probe exports, in the image the
/// host loaded (never unloaded: the host still runs it).
fn probe_switch<T>(name: &[u8], arg: T) {
    let path = vst2_probe();
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(&path) }.expect("re-open the probe");
    {
        // SAFETY: the probe exports these as `extern "C" fn(T)`.
        let f: libloading::Symbol<extern "C" fn(T)> =
            unsafe { lib.get(name) }.expect("the probe's switch");
        f(arg);
    }
    std::mem::forget(lib);
}

/// **An edit in a plugin's GUI becomes its lane's knob** (doc 014 §8.2, one
/// writer per knob). The reference VST2 plugin, in process, reports
/// `audioMasterAutomate(0, 0.5)` from inside `process` (as a GUI drag does);
/// the host's drain (`editor_idle`) hands it to the parameter callback, and
/// the next update writes it into the lane's knob on parameter 0, leaving
/// parameter 1's knob where the plugin started it.
fn a_plugin_gui_edit_becomes_the_knob() {
    let (mut s, node) = session_with_probe();
    let (p0, p1) = (
        ParamRef::plugin(node, ParamId::new(0)),
        ParamRef::plugin(node, ParamId::new(1)),
    );
    {
        let mut m = s.mods_mut();
        m.range_raw(p0, 0.0, 1.0).unwrap();
        m.range_raw(p1, 0.0, 1.0).unwrap();
        let zero = m.macro_(0.0);
        m.route(RouteSpec::new(zero, p0, Depth::of(1.0))).unwrap();
        m.route(RouteSpec::new(zero, p1, Depth::of(1.0))).unwrap();
    }
    // The probe's parameter `i` starts at `i / 16`.
    assert_eq!(knob_of(&mut s, p0), 0.0);
    assert_eq!(knob_of(&mut s, p1), 1.0 / 16.0);

    probe_switch::<u32>(b"tutti_vst2_probe_set_automate_burst\0", 1);
    let mut out = [0.0f32; 2];
    s.render_frame_at(
        &tutti_graph::TransportPosition::new(
            false,
            tutti_units::Bpm::of(120.0),
            tutti_units::Beat::new(0.0),
            None,
        ),
        tutti_buffers::InterleavedMut::one_frame(&mut out).unwrap(),
    );
    probe_switch::<u32>(b"tutti_vst2_probe_set_automate_burst\0", 0);
    s.plugin_view(node)
        .expect("the plugin's handle")
        .handle()
        .editor_idle(tutti_fixtures::main_thread());
    update(&mut s, Instant::now());

    let state = s.mod_state();
    assert_eq!(state.lane(p0.key()).expect("a lane").1.value(), Some(0.5));
    assert_eq!(
        state.lane(p1.key()).expect("a lane").1.value(),
        Some(1.0 / 16.0)
    );
    assert_eq!(
        s.plugin_view(node)
            .expect("hosted")
            .params()
            .get(ParamId::new(0))
            .expect("declared")
            .normalized(),
        Normalized::of(0.5),
        "and the mirror"
    );
}

/// A node with one event output and nothing else: a source of event edges
/// (the clip a MIDI feed names).
#[derive(Clone)]
struct EventSource1;

impl Node for EventSource1 {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A plugin's MIDI wiring changing keeps its parameter lanes.** The
/// reference VST2 plugin (in process) has a lane on parameter 0 (a macro's
/// `+0.25` on a knob of 0), on its one event input. A MIDI source is then
/// declared on that input (`Feeder::Declared`) and taken away again: each
/// rewrite of the port's declared sources leaves the lane's edge where it was
/// (`Graph::set_sources` keeps the edges it did not make), so
/// `sources` still lists the lane and the plugin still reads `0.25`.
fn a_plugins_midi_rewiring_keeps_its_lanes() {
    let (mut s, node) = session_with_probe();
    let p0 = ParamRef::plugin(node, ParamId::new(0));
    {
        let mut m = s.mods_mut();
        m.range_raw(p0, 0.0, 1.0).unwrap();
        let offset = m.macro_(1.0);
        m.route(RouteSpec::new(offset, p0, Depth::of(0.25)))
            .unwrap();
    }
    knob_of(&mut s, p0);
    let lane = s.mod_state().lane(p0.key()).expect("a lane").0;
    let clip = s.graph().add(tutti_graph::ForkByClone(EventSource1));
    let _ = s.update();
    let mut side = s.graph().take_audio_side();
    let value = |s: &DetachedSession| {
        s.plugin_view(node)
            .expect("the handle")
            .handle()
            .parameter_value(ParamId::new(0))
            .expect("a value")
    };
    let mut out = vec![vec![0.0; 1_024]; 2];
    let mut check = |s: &mut DetachedSession, want: &[NodeKey], what: &str| {
        let mut want = want.to_vec();
        want.sort();
        for _ in 0..2 {
            update(s, Instant::now());
        }
        side.render(128, PlanesMut::from_vecs(&mut out).unwrap());
        let mut sources: Vec<_> = s
            .graph_ref()
            .sources(EventInputPort::new(node, 0))
            .unwrap()
            .into_iter()
            .map(|e| e.node())
            .collect();
        sources.sort();
        assert_eq!(sources, want, "{what}: the port's sources");
        assert_eq!(
            value(s),
            Normalized::of(0.25),
            "{what}: the lane reaches the plugin"
        );
    };
    check(&mut s, &[lane], "before");
    s.graph()
        .set_feed(node, Feeder::Declared, vec![clip.events_out()])
        .unwrap();
    check(&mut s, &[lane, clip.erase()], "a MIDI source added");
    s.graph()
        .set_feed(node, Feeder::Declared, Vec::new())
        .unwrap();
    check(&mut s, &[lane], "the MIDI source removed");
}

/// **A plugin echoing the lane's writes does not ratchet its knob.** The
/// reference VST2 plugin (in process) with its echo on reports every value
/// `setParameter` is given back through `audioMasterAutomate`, from inside the
/// call. Its parameter 0 has a lane: a knob of 0 and a macro's `+0.25`, so
/// the lane writes 0.25. Taken as a GUI edit, that echo would become the knob
/// (0.25), the lane would then write 0.5, and so on to the clamp; the VST2
/// host drops a report made inside its own `setParameter` call
/// (`tutti-vst2-host`'s `host_write`), so after four rounds of render, drain
/// and update the knob is still 0 and the plugin still reads 0.25.
fn a_plugin_echoing_the_lane_keeps_its_knob() {
    let (mut s, node) = session_with_probe();
    let p0 = ParamRef::plugin(node, ParamId::new(0));
    {
        let mut m = s.mods_mut();
        m.range_raw(p0, 0.0, 1.0).unwrap();
        let offset = m.macro_(1.0);
        m.route(RouteSpec::new(offset, p0, Depth::of(0.25)))
            .unwrap();
    }
    assert_eq!(knob_of(&mut s, p0), 0.0);
    probe_switch::<bool>(b"tutti_vst2_probe_set_echo_set_parameter\0", true);

    let mut side = s.graph().take_audio_side();
    let mut out = vec![vec![0.0; 1_024]; 2];
    for round in 0..4 {
        side.render(128, PlanesMut::from_vecs(&mut out).unwrap());
        let handle = s
            .plugin_view(node)
            .expect("the plugin's handle")
            .handle()
            .clone();
        handle.editor_idle(tutti_fixtures::main_thread());
        update(&mut s, Instant::now());
        assert_eq!(
            s.mod_state().lane(p0.key()).expect("a lane").1.value(),
            Some(0.0),
            "round {round}: the knob"
        );
        assert_eq!(
            handle.parameter_value(ParamId::new(0)),
            Some(Normalized::of(0.25)),
            "round {round}: the plugin reads the knob plus the offset"
        );
    }
    probe_switch::<bool>(b"tutti_vst2_probe_set_echo_set_parameter\0", false);
}

/// The cases of this module, run by `main`.
pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["vst2_modulation";
a_plugin_gui_edit_becomes_the_knob,
a_plugins_midi_rewiring_keeps_its_lanes,
a_plugin_echoing_the_lane_keeps_its_knob,
];
