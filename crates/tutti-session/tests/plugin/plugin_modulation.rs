//! Modulation lanes on a hosted plugin's parameters, through the session
//! (doc 014 §8.2, §12.5): a plugin target is a lane on the plugin's event
//! input, its knob its own and started at the plugin's value, and a lane
//! outlives the plugin behind it being replaced (a crash's recovery, a
//! crossfade) and an export is the live render.
//!
//! The reference plugin is the CLAP probe through a real `plugin-server`.
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`):
//! without it every test here fails, naming that command. They must fail
//! rather than skip.

use crate::param_reads::ParamReads as _;
use crate::pooled::{LandOn as _, LoadPlugin, Spec};
use crate::support;
use tutti_buffers::{InterleavedMut, PlanesMut};
use tutti_session::Root;
use tutti_session::{PluginChange, PluginEvent};

use std::time::Duration;

use tutti_platform::Instant;

use support::clap_probe;
use tutti_buffers::ChannelLayout;
use tutti_graph::NodeKey;
use tutti_modulation::Lfo;
use tutti_plugin::PluginClient;
use tutti_plugin::catalog::{Loadable, PluginRef};
use tutti_session::{
    Crossfade, DetachedSession, EventInPort, EventOutPort, PHASES, ParamRef, PhaseCx, RouteSpec,
    Session, SessionEvent,
};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{Depth, Hz, SampleRate, Seconds};

const RATE: SampleRate = SampleRate::SR_48K;
/// The plugin's pipeline chunk in a detached session: its `MaxBlock`
/// (`LIVE_MAX_BLOCK`).
const CHUNK: usize = 1_024;
/// The probe's gain parameter, `-60..=12` dB; its default (0 dB) is 60/72 of
/// the range.
const GAIN: u32 = 77;

/// SAFETY: nextest runs each test in its own process, and every variable is
/// set before the first server is spawned, from the test's own thread.
fn set_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    unsafe { std::env::set_var(key, value) };
}

/// A detached stereo session at 48 kHz, the probe's server in the
/// environment.
fn detached() -> DetachedSession {
    set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    // The executor is polled: a job runs in the `Collect` that polls it, and lands
    // there.
    Session::builder()
        .executor(tutti_platform::Executor::new())
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(RATE)
        .expect("a detached session builds")
}

/// Loads the probe into `s` and puts it on the outputs.
fn add_probe(s: &mut DetachedSession) -> tutti_session::Key<tutti_session::AnyPlugin> {
    add_probe_keyed(s).1
}

/// As [`add_probe`], with the pool key of the plugin's entry.
fn add_probe_keyed(s: &mut DetachedSession) -> (u64, tutti_session::Key<tutti_session::AnyPlugin>) {
    let (key, node) = s
        .load_plugin_keyed(
            tutti_fixtures::main_thread(),
            Spec::new(PluginRef::file(clap_probe())),
        )
        .expect("the reference CLAP plugin loads");
    s.graph().connect(node, Root).unwrap();
    (key, node)
}

/// A virtual clock for the session's phases, and the plugin changes they
/// reported: a parameter read or a reload is a job on the session's polled
/// executor, run and landed in the next frame's collect.
struct Drive {
    now: Instant,
    told: Vec<&'static str>,
}

impl Drive {
    fn new() -> Self {
        Self {
            now: Instant::now(),
            told: Vec::new(),
        }
    }

    /// One frame: every phase at the clock 20 ms on.
    fn update(&mut self, s: &mut DetachedSession) {
        self.now += Duration::from_millis(20);
        let mut out = Vec::new();
        for &phase in PHASES {
            s.run_phase(phase, PhaseCx::at(self.now), &mut out);
        }
        self.told_of(&out);
    }

    fn told_of(&mut self, events: &[SessionEvent]) {
        for event in events {
            match event {
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::Reloading { .. },
                    ..
                }) => self.told.push("reloading"),
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::Recovered { .. },
                    ..
                }) => self.told.push("recovered"),
                _ => {}
            }
        }
    }

    /// [`update`](Self::update) and a plugin chunk's worth of frames rendered
    /// from the graph (the pipeline sends the server one block per chunk).
    /// Whether any was non-zero.
    fn frame(&mut self, s: &mut DetachedSession) -> bool {
        self.update(s);
        let mut heard = false;
        for _ in 0..CHUNK {
            let mut out = [0.0f32; 2];
            s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
            heard |= out.iter().any(|&x| x != 0.0);
        }
        std::thread::sleep(Duration::from_millis(5));
        heard
    }
}

/// Updates until the plugin at `node` has its parameter values read.
fn wait_for_values(drive: &mut Drive, s: &mut DetachedSession, node: impl Into<NodeKey> + Copy) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !s.plugin_view(node).is_ok_and(|p| p.params().is_ready()) {
        assert!(
            Instant::now() < deadline,
            "the plugin's parameter read never landed"
        );
        drive.update(s);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The node of `target`'s lane, waiting for it while the declarations apply.
fn lane_of(drive: &mut Drive, s: &mut DetachedSession, target: ParamRef) -> NodeKey {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            !s.mod_state()
                .problems()
                .any(|p| matches!(p, tutti_session::Problem::Refused { .. })),
            "the declarations apply"
        );
        if let Some((key, _)) = s.mod_state().lane(target.key()) {
            return key;
        }
        assert!(Instant::now() < deadline, "no lane on {target:?}");
        drive.update(s);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn gain_value(s: &DetachedSession, node: impl Into<NodeKey> + Copy) -> Option<Normalized> {
    s.plugin_view(node)
        .expect("hosted")
        .handle()
        .parameter_value(ParamId::new(GAIN))
}

/// **A modulation lane on the plugin survives the replacement.** The lane
/// (a macro of -0.25 on the gain parameter, id 77) is a node of its own
/// feeding the plugin's event input. The recovery's replacement takes the
/// plugin's node, so the declarations still resolve; the session rebuilds the
/// lane for the new unit (the node's generation moved), as it does for a
/// plugin swapped by hand, and the lane's value reaches the replacement,
/// which loaded at its default (no snapshot was due): the gain reads default
/// minus 0.25 again.
///
/// The reference plugin aborts on its 8th block in the first server only
/// (`TUTTI_CLAP_PROBE_ONCE_FILE`); the second server is the replacement.
#[test]
fn a_modulation_lane_on_the_plugin_survives_its_replacement() {
    // This process's own marker: a concurrent run of this test over the same
    // target directory must not remove it between the two servers.
    let once = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("session-recovery-lane.once.{}", std::process::id()));
    let _ = std::fs::remove_file(&once);
    // The probe of the plugin's file runs before the fault variable is set.
    crate::pooled::prime(&PluginRef::file(clap_probe()));
    set_env("TUTTI_CLAP_PROBE_RENDER_MODE", "1");
    set_env("TUTTI_CLAP_PROBE_ONCE_FILE", &once);
    set_env("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "8");
    set_env("TUTTI_CLAP_PROBE_APPLY_GAIN", "1");
    let mut s = detached();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);

    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Depth::of(-0.25)))
            .unwrap();
    }
    lane_of(&mut drive, &mut s, gain);
    // The default is 60/72 of the range; the lane moves it down a quarter.
    let lowered = Normalized::clamped(60.0 / 72.0 - 0.25);
    let near = |v: Option<Normalized>| v.is_some_and(|v| (v.get() - lowered.get()).abs() < 1e-3);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node)) {
        assert!(
            Instant::now() < deadline,
            "the lane never reached the plugin: {:?}",
            gain_value(&s, node)
        );
        drive.frame(&mut s);
    }

    // Through the crash (block 8) and the recovery.
    let deadline = Instant::now() + Duration::from_secs(30);
    while drive.told != ["reloading", "recovered"] {
        assert!(Instant::now() < deadline, "told {:?}", drive.told);
        drive.frame(&mut s);
    }
    // The session rebuilds a lane whose target unit was replaced (the node's
    // generation moved), from the same declarations. (The rebuilt lane may
    // reuse the old lane's key, so the proof is what it does: the new plugin
    // loaded at its default, and the gain is lowered again only by a lane
    // that reaches it.)
    let rebuilt = lane_of(&mut drive, &mut s, gain);
    assert!(s.graph_ref().contains(rebuilt), "the lane is in the graph");
    assert_eq!(
        s.graph_ref().sources(EventInPort::new(node, 0)).unwrap(),
        vec![EventOutPort::new(rebuilt, 0)],
        "and feeds the replacement's event input"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node)) {
        assert!(
            Instant::now() < deadline,
            "the lane does not reach the replacement: {:?}",
            gain_value(&s, node)
        );
        drive.frame(&mut s);
    }
}

/// **A crossfaded plugin keeps its gain's lane, and the lane follows the
/// plugin out.** A lane on the probe's gain feeds its event input. The
/// incoming plugin's parameter mirror is read by a job (a job on the
/// session's spawner, landed by a later update), and until it
/// lands a lane on the plugin would wait; then the lane feeds whichever plugin
/// the node holds now, the incoming one (a crossfade keeps the key and its
/// edges). When the node goes, the lane's target no longer resolves and the
/// lane leaves the graph with it.
#[test]
fn a_crossfaded_plugin_keeps_its_lane_and_a_removed_one_drops_it() {
    let mut s = detached();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let offset = m.macro_(0.0);
        m.route(RouteSpec::new(offset, gain, Depth::of(1.0)))
            .unwrap();
    }
    let outgoing_lane = lane_of(&mut drive, &mut s, gain);
    assert_eq!(
        s.graph_ref().sources(EventInPort::new(node, 0)).unwrap(),
        vec![EventOutPort::new(outgoing_lane, 0)],
        "precondition: the lane feeds the outgoing plugin"
    );

    let incoming = PluginClient::new(
        tutti_plugin::IpcConfig::at(
            tutti_plugin::unique_socket_path().expect("the OS has a random source"),
        ),
        &tutti_fixtures::detached_work(),
        &Loadable::uncatalogued(PluginRef::file(clap_probe())),
        RATE,
    )
    .expect("load a second instance of the reference plugin");
    let landed = s
        .graph()
        .crossfade_plugin(
            node,
            Box::new(incoming),
            Seconds::new(0.005),
            tutti_graph::FadeLaw::EqualAmplitude,
        )
        .expect("the crossfade is accepted");
    assert!(matches!(landed, Crossfade::Landed(())), "nothing waits");
    // Rebuilt by the crossfade: not ready until the job is run and landed.
    assert!(
        !s.plugin_view(node).is_ok_and(|p| p.params().is_ready()),
        "the incoming plugin's mirror starts unread"
    );
    wait_for_values(&mut drive, &mut s, node);

    let lane = lane_of(&mut drive, &mut s, gain);
    drive.update(&mut s);
    assert_eq!(
        s.graph_ref().sources(EventInPort::new(node, 0)).unwrap(),
        vec![EventOutPort::new(lane, 0)],
        "the gain's lane feeds the incoming plugin"
    );
    assert!(s.graph_ref().contains(lane), "while its plugin is");

    s.remove(node).unwrap();
    drive.update(&mut s);
    drive.update(&mut s);
    assert!(
        !s.graph_ref().contains(lane),
        "the lane leaves with its plugin"
    );
}

/// Sets the probe's gain to `value` through the session's params.
fn set_gain(s: &DetachedSession, node: impl Into<NodeKey> + Copy, value: f64) {
    let target = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(ParamId::new(GAIN))
        .unwrap()
        .target();
    s.set(target.to(Normalized::of(value))).unwrap();
}

/// How far a plugin's value may be from the one a lane plays: the plugin
/// takes it as a float over the wire.
const TOLERANCE: f64 = 1e-3;

/// Whether `v` is within [`TOLERANCE`] of `want`.
fn near(v: Option<Normalized>, want: f64) -> bool {
    v.is_some_and(|v| (v.get() - want).abs() < TOLERANCE)
}

/// **A set under a lane writes the lane's knob.** The gain's lane is a macro
/// of -0.25 over the knob, which starts at the plugin's 60/72. A set of 0.5
/// is the lane's knob at once, the read-back is that knob (the control, not
/// the played value), and the plugin hears the knob plus the route (0.25).
#[test]
fn a_set_under_a_lane_writes_the_knob() {
    let mut s = detached();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Depth::of(-0.25)))
            .unwrap();
    }
    lane_of(&mut drive, &mut s, gain);
    // The lane drives the plugin before the set.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node), 60.0 / 72.0 - 0.25) {
        assert!(
            Instant::now() < deadline,
            "the lane never reached the plugin"
        );
        drive.frame(&mut s);
    }

    set_gain(&s, node, 0.5);
    let knob = s.mod_state().lane(gain.key()).and_then(|(_, l)| l.value());
    assert_eq!(knob, Some(0.5), "the knob is the set value, with no update");
    let read = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(ParamId::new(GAIN))
        .unwrap()
        .normalized();
    assert_eq!(read, Normalized::of(0.5), "the read-back is the knob");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node), 0.25) {
        assert!(
            Instant::now() < deadline,
            "the plugin hears {:?}, not the knob plus the route",
            gain_value(&s, node)
        );
        drive.frame(&mut s);
    }
}

/// **A set in the update that added the lane writes its knob.** The lane
/// exists as the update ends, so a set made then is the base: the plugin
/// hears it plus the route, not the raw value.
#[test]
fn a_set_right_after_the_lane_is_added_writes_the_knob() {
    let mut s = detached();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Depth::of(-0.25)))
            .unwrap();
    }
    lane_of(&mut drive, &mut s, gain);

    set_gain(&s, node, 0.5);
    let knob = s.mod_state().lane(gain.key()).and_then(|(_, l)| l.value());
    assert_eq!(knob, Some(0.5), "the knob is the set value");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node), 0.25) {
        assert!(
            Instant::now() < deadline,
            "the plugin hears {:?}, not the knob plus the route",
            gain_value(&s, node)
        );
        drive.frame(&mut s);
    }
}

/// **Once its lane is gone a parameter takes a set as the plugin's own.** The
/// lane's knob is not written, and the plugin takes the value raw.
#[test]
fn a_set_after_the_lane_is_removed_goes_to_the_plugin() {
    let mut s = detached();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    let route = {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Depth::of(-0.25)))
            .unwrap()
    };
    lane_of(&mut drive, &mut s, gain);
    drive.update(&mut s);
    {
        let mut m = s.mods_mut();
        m.remove_route(route).unwrap();
        m.clear_range(gain);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.mod_state().lane(gain.key()).is_some() {
        assert!(Instant::now() < deadline, "the lane never left");
        drive.update(&mut s);
    }

    // The update that dropped the lane also dropped the parameter's knob.
    set_gain(&s, node, 0.5);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node), 0.5) {
        assert!(
            Instant::now() < deadline,
            "the plugin hears {:?}, not the raw set",
            gain_value(&s, node)
        );
        drive.frame(&mut s);
    }
}

/// **A read of the plugin does not put the played value in the mirror.** The
/// gain is set to 0.5 under a lane that plays 0.25 (a macro of -0.25 over the
/// knob). A preset load makes the session read the plugin's values again; the
/// read finds the played 0.25, and the parameter still reads the knob.
#[test]
fn a_read_landing_under_a_lane_keeps_the_knob_in_the_mirror() {
    // The plugin's file is probed before its variables are set; the reference
    // plugin answers `clap.preset-load` only when asked.
    crate::pooled::prime(&PluginRef::file(clap_probe()));
    set_env("TUTTI_CLAP_PROBE_PRESET_LOAD", "1");
    let mut s = detached();
    let mut drive = Drive::new();
    let (entry, node) = add_probe_keyed(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Depth::of(-0.25)))
            .unwrap();
    }
    lane_of(&mut drive, &mut s, gain);
    set_gain(&s, node, 0.5);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node), 0.25) {
        assert!(Instant::now() < deadline, "the lane never played the knob");
        drive.frame(&mut s);
    }

    // The preset sets only the first parameter (Cutoff), so the read it
    // causes has landed once the mirror's Cutoff moved.
    let cutoff = |s: &DetachedSession| {
        let plugin = s.plugin_view(node).unwrap();
        let params = plugin.params();
        let first = params.iter().next().expect("a parameter").id();
        params.get(first).unwrap().normalized()
    };
    let before = cutoff(&s);
    s.load_preset_file(&entry, "/nowhere/bright.tutti-preset")
        .expect("the entry has a node")
        .land_on(&mut s)
        .expect("the plugin takes the preset");
    let deadline = Instant::now() + Duration::from_secs(10);
    while cutoff(&s) == before {
        assert!(Instant::now() < deadline, "the read never landed");
        drive.update(&mut s);
        std::thread::sleep(Duration::from_millis(5));
    }
    // The knob reported as the read landed is applied by the next update.
    drive.update(&mut s);

    let read = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(ParamId::new(GAIN))
        .unwrap()
        .normalized();
    assert!(
        near(Some(read), 0.5),
        "the mirror holds {read:?}, not the knob 0.5"
    );
}

/// **Export == live on a plugin's parameter.** The reference CLAP plugin
/// renders a constant tag scaled by its gain (id 77, applied sample-accurately
/// at each parameter event's frame); a 3 Hz sine at depth 0.1 modulates the
/// gain's lane, whose knob starts at the plugin's own value (0 dB: 60/72 of
/// `-60..=12`). Live renders rolling 1024-frame blocks from the play start,
/// each once the server answered the chunk before it
/// (`PluginControls::chunk_in_flight_answered`), and fails by name if one was
/// played unanswered (`missed_chunks`); the export forks the graph (the
/// plugin by state transfer, the lane by clone) and renders the same span at
/// its 1024-frame chunk. Equal bit for bit.
#[test]
fn an_export_is_the_live_render_on_a_clap_gain() {
    use std::sync::Arc;

    use tutti_buffers::ChannelLayout;
    use tutti_session::{Bounce, BounceLatency, BounceTail, ExportClock, Range};
    use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig};
    use tutti_units::{Beat, Bpm};

    const BLOCK: usize = 1_024;
    const BLOCKS: usize = 24;

    // The plugin's file is probed before its variables are set.
    crate::pooled::prime(&PluginRef::file(clap_probe()));
    set_env("TUTTI_CLAP_PROBE_RENDER_MODE", "2");
    set_env("TUTTI_CLAP_PROBE_APPLY_GAIN", "1");
    let mut s = detached();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);

    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let lfo = m.lfo(Lfo::sine().rate(Hz::new(3.0)));
        m.route(RouteSpec::new(lfo, gain, Depth::of(0.1))).unwrap();
    }
    lane_of(&mut drive, &mut s, gain);
    let knob = s
        .mod_state()
        .lane(gain.key())
        .and_then(|(_, lane)| lane.value())
        .expect("a plugin lane has a knob");
    assert!(
        (knob - 60.0 / 72.0).abs() < 1e-4,
        "the knob starts at the plugin's 0 dB: {knob}"
    );
    for _ in 0..3 {
        drive.update(&mut s);
    }

    let total = BLOCK * BLOCKS;
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat::new(0.0),
        tempo_map: Arc::new(tutti_tempo::TempoMap::constant(Bpm::of(120.0)).expect("a tempo")),
        sample_rate: RATE,
        loop_range: None,
    }));
    let config = Bounce::project()
        .master_buffers()
        .channels(ChannelLayout::STEREO)
        .range(Range::Seconds(
            Seconds::new(0.0),
            Seconds::new(total as f64 / RATE.get()),
        ))
        .sample_rate(RATE)
        .latency(BounceLatency::Keep)
        .tail(BounceTail::None);

    // Live: rolling from beat 0 in device-sized blocks, a frame between each
    // as a device's callbacks would leave. The plugin's pipeline plays each
    // chunk's answer one block later; a block played before its answer
    // landed is silence, which would read as a mismatch below. So before each
    // block, wait for the chunk in flight to be answered (a slow server, a
    // loaded machine), and after it, require that none was played
    // unanswered: a late block is waited for, a missed one fails here, by
    // name.
    let missed_before = diagnostics_of(&s, node.into()).missed_chunks();
    let mut side = s.graph().take_audio_side();
    let mut live = [Vec::new(), Vec::new()];
    let mut out = vec![vec![0.0; BLOCK]; 2];
    for b in 0..BLOCKS {
        let beat = (b * BLOCK) as f64 * 2.0 / RATE.get();
        side.set_transport(tutti_graph::TransportPosition::new(
            true,
            Bpm::of(120.0),
            Beat::new(beat),
            None,
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !diagnostics_of(&s, node.into()).chunk_in_flight_answered() {
            assert!(
                Instant::now() < deadline,
                "live block {b}: the chunk before it was never answered (10 s)"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        side.render(BLOCK, PlanesMut::from_vecs(&mut out).unwrap());
        assert_eq!(
            diagnostics_of(&s, node.into()).missed_chunks(),
            missed_before,
            "live block {b} late or missed: the plugin's chunk was played unanswered"
        );
        for (l, o) in live.iter_mut().zip(&out) {
            l.extend_from_slice(o);
        }
        drive.update(&mut s);
    }

    let request = config.clock(ExportClock::timeline(timeline));
    let export = s.bounce(request).expect("the export renders").into_master();
    for (c, (l, e)) in live.iter().zip(export.planes()).enumerate() {
        assert!(e.len() >= total, "channel {c}: {} frames", e.len());
        let moving = l.iter().filter(|&&v| v != 0.0).collect::<Vec<_>>();
        assert!(
            moving.windows(2).any(|w| w[0] != w[1]),
            "channel {c}: the gain moves"
        );
        for f in 0..total {
            assert_eq!(
                l[f].to_bits(),
                e[f].to_bits(),
                "channel {c}, frame {f}: live {} vs export {}",
                l[f],
                e[f]
            );
        }
    }
}

fn diagnostics_of(s: &DetachedSession, node: NodeKey) -> tutti_session::Diagnostics<'_> {
    s.plugin_view(node).expect("hosted").diagnostics()
}
