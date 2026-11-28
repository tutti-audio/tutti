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

use crate::bounced::bounced;
use crate::loading::Perform as _;
use crate::loading::{LoadPlugin, Spec};
use crate::support;
use tutti_buffers::InterleavedMut;
use tutti_fixtures::WavePlanes;
use tutti_session::MainOutput;
use tutti_session::ToMemory;
use tutti_session::{PluginChange, PluginEvent, Replacement};

use std::time::Duration;

use tutti_platform::Instant;

use support::clap_probe;
use tutti_buffers::ChannelLayout;
use tutti_graph::NodeKey;
use tutti_modulation::Lfo;
use tutti_plugin::PluginClient;
use tutti_plugin_db::Loadable;
use tutti_plugin_types::PluginRef;
use tutti_session::{EventOutputPort, PHASES, ParamRef, PhaseCx, RouteSpec, Session, SessionEvent};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{Bipolar, Hz, SampleRate, Seconds};

const RATE: SampleRate = SampleRate::SR_48K;
/// The plugin's pipeline chunk in an offline session: its `MaxBlock`
/// (`LIVE_MAX_BLOCK`).
const CHUNK: usize = 1_024;
/// The probe's gain parameter, `-60..=12` dB; its default (0 dB) is 60/72 of
/// the range.
const GAIN: u32 = 77;

/// An offline stereo session at 48 kHz, the probe's server in the
/// environment.
fn session() -> Session {
    tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .sample_rate(RATE)
        .block_dates(tutti_session::BlockDates::Frames)
        .build()
        .expect("an offline session builds")
}

/// Loads the probe into `s` and puts it on the outputs.
fn add_probe(s: &mut Session) -> tutti_session::Key<tutti_session::AnyPlugin> {
    let node = s
        .load_plugin_blocking(
            tutti_fixtures::main_thread(),
            Spec::new(PluginRef::file(clap_probe())),
        )
        .expect("the reference CLAP plugin loads");
    s.graph().connect(node, MainOutput).unwrap();
    node
}

/// A virtual clock for the session's phases, and the plugin changes they
/// reported: a parameter read or a reload runs on tutti's thread and lands in a later
/// frame's collect.
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
    fn update(&mut self, s: &mut Session) {
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
    fn frame(&mut self, s: &mut Session) -> bool {
        self.update(s);
        let mut heard = false;
        for _ in 0..CHUNK {
            let mut out = [0.0f32; 2];
            s.render(&mut InterleavedMut::one_frame(&mut out).unwrap());
            heard |= out.iter().any(|&x| x != 0.0);
        }
        heard
    }
}

/// Updates until the plugin at `node` has its parameter values read.
fn wait_for_values(drive: &mut Drive, s: &mut Session, node: impl Into<NodeKey> + Copy) {
    tutti_fixtures::wait_until(
        "the plugin's parameter read to land",
        tutti_fixtures::HANG_GUARD,
        || {
            drive.update(s);
            s.plugin_view(node).is_ok_and(|p| p.params().is_ready())
        },
    );
}

/// The node of `target`'s lane, waiting for it while the declarations apply.
fn lane_of(drive: &mut Drive, s: &mut Session, target: ParamRef) -> NodeKey {
    let mut lane = None;
    tutti_fixtures::wait_until(
        &format!("a lane on {target:?}"),
        tutti_fixtures::HANG_GUARD,
        || {
            assert!(
                !s.mod_state().problems().any(|p| !matches!(
                    p,
                    tutti_session::Problem::Unresolved { .. }
                        | tutti_session::Problem::Unfed { .. }
                )),
                "the declarations apply"
            );
            lane = s.mod_state().lane(target.key()).map(|(key, _)| key);
            if lane.is_none() {
                drive.update(s);
            }
            lane.is_some()
        },
    );
    lane.expect("the wait ended on a lane")
}

/// The plugin's own gain, normalized over its declared range: read from the state the
/// plugin saves, not from the host's mirror.
fn gain_value(
    s: &Session,
    node: tutti_session::Key<tutti_session::AnyPlugin>,
) -> Option<Normalized> {
    let state = s
        .save_state(&node, tutti_plugin_types::StateUse::Project)
        .ok()?
        .perform(tutti_fixtures::main_thread())
        .ok()?;
    tutti_clap_test_plugin::saved_normalized(state.bytes(), GAIN).and_then(Normalized::new)
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
    crate::loading::prime(&PluginRef::file(clap_probe()));
    tutti_fixtures::set_env("TUTTI_CLAP_PROBE_RENDER_MODE", "1");
    tutti_fixtures::set_env("TUTTI_CLAP_PROBE_ONCE_FILE", &once);
    tutti_fixtures::set_env("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "8");
    tutti_fixtures::set_env("TUTTI_CLAP_PROBE_APPLY_GAIN", "1");
    let mut s = session();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);

    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Bipolar::of(-0.25)))
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
        s.graph_ref().sources(node.event_input(0)).unwrap(),
        vec![EventOutputPort::new(rebuilt, 0)],
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
    let mut s = session();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let offset = m.macro_(0.0);
        m.route(RouteSpec::new(offset, gain, Bipolar::of(1.0)))
            .unwrap();
    }
    let outgoing_lane = lane_of(&mut drive, &mut s, gain);
    assert_eq!(
        s.graph_ref().sources(node.event_input(0)).unwrap(),
        vec![EventOutputPort::new(outgoing_lane, 0)],
        "precondition: the lane feeds the outgoing plugin"
    );

    let incoming = PluginClient::new(
        tutti_plugin_ipc::IpcConfig::new(),
        tutti_fixtures::executor(),
        &tutti_fixtures::detached_work(),
        &Loadable::uncatalogued(PluginRef::file(clap_probe())),
        RATE,
    )
    .expect("load a second instance of the reference plugin");
    let landed = s
        .graph()
        .replace_plugin(node, Replacement::new(Box::new(incoming)))
        .expect("the replace is accepted");
    assert_eq!(landed, node);
    assert_eq!(
        s.graph_ref().testing().waiting_replacements(),
        0,
        "nothing waits"
    );
    // Rebuilt by the replace: not ready until the job is run and landed.
    assert!(
        !s.plugin_view(node).is_ok_and(|p| p.params().is_ready()),
        "the incoming plugin's mirror starts unread"
    );
    wait_for_values(&mut drive, &mut s, node);

    let lane = lane_of(&mut drive, &mut s, gain);
    drive.update(&mut s);
    assert_eq!(
        s.graph_ref().sources(node.event_input(0)).unwrap(),
        vec![EventOutputPort::new(lane, 0)],
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
fn set_gain(s: &Session, node: impl Into<NodeKey> + Copy, value: f64) {
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
    let mut s = session();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Bipolar::of(-0.25)))
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
    let mut s = session();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Bipolar::of(-0.25)))
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

/// **Narrowing the target's declared range changes what the plugin hears, at
/// the analytic figure.** The gain's knob is 0.4 and a macro of 1 is routed at
/// depth 0.5: over `0..=1` the plugin hears `0.4 + 0.5 * 1` = 0.9; the range
/// redeclared as `0.2..=0.7` (span 0.5; a plugin parameter's range has no
/// typed write, so it is cleared and declared again) it hears `0.4 + 0.5 * 0.5` = 0.65.
#[test]
fn narrowing_a_plugin_targets_range_changes_what_lands_by_the_new_span() {
    let mut s = session();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Bipolar::of(0.5)))
            .unwrap();
    }
    lane_of(&mut drive, &mut s, gain);
    set_gain(&s, node, 0.4);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node), 0.9) {
        assert!(
            Instant::now() < deadline,
            "the plugin hears {:?}, not the knob plus the route over 0..=1",
            gain_value(&s, node)
        );
        drive.frame(&mut s);
    }

    {
        // A plugin parameter's range is redeclared: cleared, then declared again.
        let mut m = s.mods_mut();
        assert!(m.clear_range(gain));
        m.range_raw(gain, 0.2, 0.7).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !near(gain_value(&s, node), 0.65) {
        assert!(
            Instant::now() < deadline,
            "the plugin hears {:?}, not the knob plus the route over the new span",
            gain_value(&s, node)
        );
        drive.frame(&mut s);
    }
}

/// **Once its lane is gone a parameter takes a set as the plugin's own.** The
/// lane's knob is not written, and the plugin takes the value raw.
#[test]
fn a_set_after_the_lane_is_removed_goes_to_the_plugin() {
    let mut s = session();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    let route = {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Bipolar::of(-0.25)))
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
    crate::loading::prime(&PluginRef::file(clap_probe()));
    tutti_fixtures::set_env("TUTTI_CLAP_PROBE_PRESET_LOAD", "1");
    let mut s = session();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);
    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let source = m.macro_(1.0);
        m.route(RouteSpec::new(source, gain, Bipolar::of(-0.25)))
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
    let cutoff = |s: &Session| {
        let plugin = s.plugin_view(node).unwrap();
        let params = plugin.params();
        let first = params.iter().next().expect("a parameter").id();
        params.get(first).unwrap().normalized()
    };
    let before = cutoff(&s);
    let loaded = s
        .load_preset_file(&node, "/nowhere/bright.tutti-preset")
        .expect("the node hosts a plugin")
        .perform(tutti_fixtures::main_thread())
        .expect("the plugin takes the preset");
    s.apply(loaded).expect("the node hosts the plugin");
    tutti_fixtures::wait_until("the read to land", tutti_fixtures::HANG_GUARD, || {
        drive.update(&mut s);
        cutoff(&s) != before
    });
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
/// played unanswered (`ChunkOutcomes::missed`); the export forks the graph (the
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
    crate::loading::prime(&PluginRef::file(clap_probe()));
    tutti_fixtures::set_env("TUTTI_CLAP_PROBE_RENDER_MODE", "2");
    tutti_fixtures::set_env("TUTTI_CLAP_PROBE_APPLY_GAIN", "1");
    let mut s = session();
    let mut drive = Drive::new();
    let node = add_probe(&mut s);
    drive.update(&mut s);
    wait_for_values(&mut drive, &mut s, node);

    let gain = ParamRef::from(node.param(ParamId::new(GAIN)));
    {
        let mut m = s.mods_mut();
        m.range_raw(gain, 0.0, 1.0).unwrap();
        let lfo = m.lfo(Lfo::sine().rate(Hz::new(3.0)));
        m.route(RouteSpec::new(lfo, gain, Bipolar::of(0.1)))
            .unwrap();
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
    let config = Bounce::new()
        .main_output(
            ToMemory::new().channels(tutti_buffers::ChannelSet::default_for(
                ChannelLayout::STEREO,
            )),
        )
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
    let missed_before = diagnostics_of(&s, node.into()).chunks().missed;
    s.play().expect("room");
    let mut live = [Vec::new(), Vec::new()];
    let mut out = vec![0.0f32; BLOCK * 2];
    for b in 0..BLOCKS {
        tutti_fixtures::wait_until(
            &format!("live block {b}: the chunk before it to be answered"),
            tutti_fixtures::HANG_GUARD,
            || diagnostics_of(&s, node.into()).chunk_in_flight_answered(),
        );
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
        assert_eq!(
            diagnostics_of(&s, node.into()).chunks().missed,
            missed_before,
            "live block {b} late or missed: the plugin's chunk was played unanswered"
        );
        for frame in out.chunks_exact(2) {
            live[0].push(frame[0]);
            live[1].push(frame[1]);
        }
        drive.update(&mut s);
    }

    let request = config.clock(ExportClock::timeline(timeline));
    let export = bounced(&mut s, request)
        .expect("the export renders")
        .into_main_output();
    for (c, (l, e)) in live.iter().zip(export.to_planes()).enumerate() {
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

fn diagnostics_of(s: &Session, node: NodeKey) -> tutti_session::Diagnostics<'_> {
    s.plugin_view(node).expect("hosted").diagnostics()
}
