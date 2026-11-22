//! Hosted plugins and exports in the session (design 015 §4.3, §9.6),
//! end to end over the reference CLAP plugin in a real `plugin-server`.
//!
//! The render mode a plugin was told is read back through its latency: with
//! `TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY=24` the probe reports 24 frames
//! more while it renders offline, and says so through `clap.latency`, which
//! the host carries to the plugin's latency cell (`PluginControls::latency`).
//! The live plugin is the one these read: an export's fork is a second
//! instance, in its own server.
//!
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`):
//! it is not a dev-dependency (a cycle through `tutti-plugin`), and without
//! it every test here fails, naming that command. They must fail rather than
//! skip.

use crate::bounced::bounced;
use crate::loading::{Block as _, LoadFrom as _, LoadPlugin, Spec};
use crate::param_reads::ParamReads as _;
use crate::support;
use tutti_fixtures::block_on;
use tutti_plugin_db::Hosting;
use tutti_rt::LockExt;
use tutti_session::InPort;
use tutti_session::MainOutput;
use tutti_session::{PluginChange, PluginEvent};

use std::sync::atomic::Ordering;

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_buffers::MaxBlock;
use tutti_platform::Instant;

use support::clap_probe;
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::ForkByClone;
use tutti_graph::NodeKey;
use tutti_plugin_types::PluginFailure;
use tutti_plugin_types::PluginRef;
use tutti_plugin_types::RenderMode;
use tutti_session::BufferMainOutput;
use tutti_session::{
    Bounce, BounceError, BounceLatency, BounceTail, ExportClock, Phase, PhaseCx, PluginHealth,
    Range, Session, SessionEvent,
};
use tutti_session::{Peer as _, PeerState};
use tutti_session::{RecoveryFailure, ReloadCause};
use tutti_units::{SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 512;
/// What the probe adds to its latency while it renders offline.
const OFFLINE_EXTRA: usize = 24;

/// Sets an environment variable for the plugin servers this process spawns.
fn set_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: nextest runs each test in its own process, and every variable is
    // set before the first server is spawned, from the test's own thread:
    // nothing reads the environment concurrently.
    unsafe { std::env::set_var(key, value) };
}

/// An offline stereo session at 48 kHz, its servers' environment set:
/// the server's path, the offline extra, and `extra`.
fn session(extra: &[(&str, &str)]) -> Session {
    session_built(extra, |b| b)
}

/// [`session`] with the builder `build` shapes.
fn session_built(
    extra: &[(&str, &str)],
    build: impl FnOnce(tutti_session::SessionBuilder) -> tutti_session::SessionBuilder,
) -> Session {
    // The probe of the plugin's file runs before any fault variable is set.
    crate::loading::prime(&PluginRef::file(clap_probe()));
    set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    set_env(
        "TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY",
        OFFLINE_EXTRA.to_string(),
    );
    for (key, value) in extra {
        set_env(key, value);
    }
    build(Session::builder())
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .build()
        .expect("an offline session builds")
}

/// Loads the probe into `s` (blocking), unwired.
fn add_probe(s: &mut Session) -> NodeKey {
    s.load_plugin_blocking(
        tutti_fixtures::main_thread(),
        Spec::new(PluginRef::file(clap_probe())),
    )
    .expect("the reference CLAP plugin loads")
    .erase()
}

/// The probe's own latency (without the pipeline's chunk), as its node's
/// latency cell holds it.
fn latency(s: &Session, node: NodeKey) -> Samples {
    s.plugin_view(node)
        .expect("hosted")
        .diagnostics()
        .reported_latency()
}

/// One control round trip to the probe's server. The server sends the
/// `LatencyChanged` a `render.set` caused after that call's reply, and the
/// bridge thread reads it off the socket only while it waits for the next
/// reply; live, any later control call (a snapshot, a parameter read) carries
/// it, and here this one does.
fn round_trip(s: &Session, node: NodeKey) {
    let _ = s
        .plugin_view(node)
        .expect("hosted")
        .handle()
        .save_state(tutti_plugin_types::StateUse::Project);
}

/// Waits (up to 10 s) for the probe's latency to read `want`: the
/// `clap.latency` callback reaches the cell from the server's bridge thread.
fn await_latency(s: &Session, node: NodeKey, want: Samples, why: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while {
        round_trip(s, node);
        latency(s, node) != want
    } {
        assert!(
            Instant::now() < deadline,
            "{why}: the probe's latency stayed {:?}, want {want:?}",
            latency(s, node)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Holds for 200 ms that the probe's latency reads `want`: what an
/// announcement that did not happen looks like.
fn stays_latency(s: &Session, node: NodeKey, want: Samples, why: &str) {
    let until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < until {
        round_trip(s, node);
        assert_eq!(latency(s, node), want, "{why}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A render of `seconds` into memory: stereo at `RATE`, trimming nothing and
/// keeping no tail, on a frozen clock.
fn request_for(seconds: f64) -> Bounce<NodeKey, BufferMainOutput> {
    Bounce::new()
        .main_output_buffers()
        .channels(ChannelLayout::STEREO)
        .range(Range::Seconds(
            tutti_units::Seconds::new(0.0),
            tutti_units::Seconds::new(seconds),
        ))
        .sample_rate(RATE)
        .clock(ExportClock::frozen())
        .latency(BounceLatency::Keep)
        .tail(BounceTail::None)
}

/// A short render into memory.
fn request() -> Bounce<NodeKey, BufferMainOutput> {
    request_for(0.05)
}

/// The probe on the outputs, its render mode announced (Realtime) by an
/// update: returns its node and its realtime latency.
fn wired_probe(s: &mut Session) -> (NodeKey, Samples) {
    let node = add_probe(s);
    s.graph().connect(node, MainOutput).unwrap();
    let _ = s.update();
    // Told Realtime by that update (it was never told): the latency it
    // reports realtime.
    let base = latency(s, node);
    assert_eq!(s.render_mode(), RenderMode::Realtime);
    (node, base)
}

/// **An export announces Offline before its first block, and Realtime after
/// it.** `export` runs `Phase::RenderMode` before it returns: the probe
/// renders offline (its latency moves by the extra) while the job is still
/// unrun, so before the fork can render a block. `apply` releases the token and
/// announces nothing: the update after it runs the phase again, back to realtime.
#[test]
fn an_export_announces_offline_before_its_first_block_and_realtime_after() {
    let mut s = session(&[]);
    let (node, base) = wired_probe(&mut s);
    let offline = base + Samples::new(OFFLINE_EXTRA);

    let job = s.bounce(request()).expect("the export starts");
    assert_eq!(s.render_mode(), RenderMode::Offline);
    await_latency(&s, node, offline, "told Offline before the job runs");
    assert!(matches!(s.bounce(request()), Err(BounceError::InFlight)));

    let done = block_on(job);
    let output = s.apply(done).expect("the export renders");
    assert!(
        !output.main_output().planes().is_empty(),
        "the main output is planes"
    );
    let _ = s.update();
    assert_eq!(s.render_mode(), RenderMode::Realtime);
    await_latency(
        &s,
        node,
        base,
        "told Realtime by the update after the apply",
    );
}

/// **A dropped job, or a dropped return, restores Realtime on the next
/// update** (§4.3: the render mode is a level). Neither reaches
/// `apply`; each drops the export's token, and the update after
/// finds none alive. Until that update the session still says Offline:
/// nothing is announced from a drop (it may be a worker's).
#[test]
fn a_dropped_export_job_restores_realtime_on_the_next_update() {
    let mut s = session(&[]);
    let (node, base) = wired_probe(&mut s);
    let offline = base + Samples::new(OFFLINE_EXTRA);

    // The job, dropped unrun (a cancelled task).
    let job = s.bounce(request()).expect("starts");
    await_latency(&s, node, offline, "the job's export is in flight");
    drop(job);
    assert_eq!(
        s.render_mode(),
        RenderMode::Offline,
        "nothing told on a drop"
    );
    let _ = s.update();
    assert_eq!(s.render_mode(), RenderMode::Realtime);
    await_latency(&s, node, base, "the update after a dropped job");

    // Its return, dropped unfinished (a worker that ran it, then went).
    let job = s.bounce(request()).expect("starts again");
    await_latency(&s, node, offline, "the second export is in flight");
    let done = block_on(job);
    assert!(
        matches!(s.bounce(request()), Err(BounceError::InFlight)),
        "the return holds the token"
    );
    drop(done);
    let _ = s.update();
    assert_eq!(s.render_mode(), RenderMode::Realtime);
    await_latency(&s, node, base, "the update after a dropped return");
}

/// **A plugin added mid-export is told Offline** on its first update:
/// `needs_notify`'s `!told` half, end to end. The mode has not changed
/// since the export announced it, so only "never told" reaches the new
/// plugin.
#[test]
fn a_plugin_added_mid_export_is_told_offline() {
    let mut s = session(&[]);
    let dc = s
        .graph()
        .add(ForkByClone(tutti_nodes::testing::Const::mono(0.0)));
    s.graph().connect(dc, MainOutput).unwrap();
    let _ = s.update();
    let job = s.bounce(request()).expect("starts");
    assert_eq!(s.render_mode(), RenderMode::Offline);

    let late = add_probe(&mut s);
    let base = latency(&s, late);
    stays_latency(&s, late, base, "not told before its first update");
    let _ = s.update();
    await_latency(
        &s,
        late,
        base + Samples::new(OFFLINE_EXTRA),
        "a plugin added mid-export, after its first update",
    );

    s.apply(block_on(job)).expect("renders, not cancelled");
    let _ = s.update();
    await_latency(
        &s,
        late,
        base,
        "told Realtime by the update after the apply",
    );
}

/// **With recovery off, a plugin dead for the window stays silent at its node,
/// and is reported once.** The probe
/// aborts on its first block; once the engine latches the death, the
/// debounce counts unhealthy observations at the `now`s the test passes:
/// `Liveness::DEATHS` of them inside the window (50 ms) are still Failing,
/// with the node in place; the first past the window is Dead, the node stays
/// at its key, silent, and `PluginChange::Died` names it, with the engine's cause; no update
/// after reports it again.
#[test]
fn a_plugin_dead_for_the_window_stays_silent_at_its_node_and_is_reported_once() {
    let mut s = session_built(&[("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1")], |b| {
        b.no_plugin_recovery()
    });
    let (node, _) = wired_probe(&mut s);

    // Render until the engine latches the death (the server aborted on its
    // first block; the bridge notices on a later one).
    let mut out = vec![0.0f32; BLOCK * 2];
    let deadline = Instant::now() + Duration::from_secs(10);
    while matches!(s.plugin_view(node).unwrap().health(), PluginHealth::Running) {
        assert!(Instant::now() < deadline, "the probe never died");
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
        std::thread::sleep(Duration::from_millis(5));
    }

    let t0 = Instant::now();
    let plugins_at = |s: &mut Session, ms: u64| {
        let mut out = Vec::new();
        s.run_phase(
            Phase::Plugins,
            PhaseCx::at(t0 + Duration::from_millis(ms)),
            &mut out,
        );
        out.into_iter()
            .filter_map(|e| match e {
                SessionEvent::Plugin(PluginEvent {
                    node,
                    change: PluginChange::Died { cause },
                    ..
                }) => Some((node, cause)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    for ms in [0, 10, 20] {
        assert!(plugins_at(&mut s, ms).is_empty(), "inside the window");
        assert!(
            matches!(
                s.plugin_view(node).unwrap().health(),
                PluginHealth::Died { .. }
            ),
            "{:?}",
            s.plugin_view(node).unwrap().health()
        );
        assert!(s.graph_ref().contains(node), "a failing plugin stays");
    }
    let died = plugins_at(&mut s, 60);
    assert_eq!(died.len(), 1, "one report: {died:?}");
    assert_eq!(died[0].0, node);
    assert!(
        matches!(
            died[0].1,
            PluginFailure::StreamClosed | PluginFailure::ServerExited { .. }
        ),
        "a server that aborted ended its process and closed its stream, and the cause is the first the host saw: {:?}",
        died[0].1
    );
    assert!(
        s.graph_ref().contains(node),
        "the dead plugin's node stays at its key, silent"
    );
    assert!(s.plugin_view(node).is_ok(), "and its record");

    assert!(plugins_at(&mut s, 200).is_empty(), "reported once");
    let again = s
        .update()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::Died { .. },
                    ..
                })
            )
        })
        .count();
    assert_eq!(again, 0, "reported once");
}

/// A node that records the thread it is dropped on: a stand-in for a
/// forked in-process VST2 plugin, whose drop closes its instance and belongs
/// on the main thread. Forked by clone, so the export's copy shares the
/// record.
#[derive(Clone)]
struct DropWitness(Arc<Mutex<Vec<std::thread::ThreadId>>>);

impl Drop for DropWitness {
    fn drop(&mut self) {
        self.0.lock_recover().push(std::thread::current().id());
    }
}

impl tutti_graph::Node for DropWitness {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(&mut self, _: &tutti_graph::Cx<'_>, _: tutti_graph::Io<'_>) -> tutti_graph::Status {
        tutti_graph::Status::Silent
    }
    fn reset(&mut self) {}
}

/// **An export's leftovers drop on the main thread.** The live graph holds a
/// [`DropWitness`] beside the probe, both on the outputs (through a plugin
/// input: the probe's fork comes too), so the export's fork carries a clone
/// of the witness. The job renders on the executor's thread and `apply` runs on
/// this one: every drop of the fork's witness is this thread's.
#[test]
fn export_leftovers_drop_on_the_main_thread() {
    let mut s = session(&[]);
    let drops: Arc<Mutex<Vec<std::thread::ThreadId>>> = Arc::default();
    let witness = s.graph().add(ForkByClone(DropWitness(Arc::clone(&drops))));
    let probe = add_probe(&mut s);
    s.graph()
        .set_sources(
            InPort::new(probe, 0),
            [tutti_session::OutPort::new(witness, 0)],
        )
        .unwrap();
    s.graph()
        .set_sources(
            InPort::new(probe, 1),
            [tutti_session::OutPort::new(witness, 1)],
        )
        .unwrap();
    s.graph().connect(probe, MainOutput).unwrap();
    let _ = s.update();
    drops.lock().unwrap().clear();

    let job = s.bounce(request()).expect("starts");
    let done = block_on(job);
    s.apply(done).expect("renders, not cancelled");

    let main = std::thread::current().id();
    let drops = drops.lock().unwrap().clone();
    assert!(!drops.is_empty(), "the export's copy was never dropped");
    assert!(
        drops.iter().all(|&t| t == main),
        "a copy was dropped off the main thread: {drops:?} (main {main:?})"
    );
}

// --- Recovery ---------------------------------------------------------------

/// The reference plugin's applied `Gain` parameter.
fn gain_param(_s: &Session, _node: NodeKey) -> tutti_units::ParamId {
    tutti_units::ParamId::new(tutti_clap_test_plugin::params_state::GAIN_PARAM_ID)
}

fn param(s: &Session, node: NodeKey, id: tutti_units::ParamId) -> Option<f64> {
    s.plugin_view(node)
        .expect("hosted")
        .handle()
        .parameter_value(id)
        .map(|v| v.get())
}

/// Whether a read-back value is the 0.25 that was set (it passes through the
/// plugin's f32 and dB range).
fn is_quarter(v: Option<f64>) -> bool {
    v.is_some_and(|v| (v - 0.25).abs() < 1e-4)
}

/// Waits until the plugin reads back the 0.25 that was set at `id`. The write rides one
/// block to the server, so one block is rendered and the echo is awaited without
/// rendering: the number of blocks the plugin has processed does not depend on how long
/// the echo takes, which a plugin that aborts on its Nth block relies on. A hang guard
/// bounds the wait.
fn take_value(s: &mut Session, node: NodeKey, id: tutti_units::ParamId) {
    let _ = renders_sound(s);
    let guard = Instant::now() + Duration::from_secs(30);
    while !is_quarter(param(s, node, id)) {
        assert!(Instant::now() < guard, "the probe never took the value");
        std::thread::yield_now();
    }
}

/// A marker path for `TUTTI_CLAP_PROBE_ONCE_FILE`, free now: the first server
/// to start fails, replacements do not. The path is this test process's own:
/// two runs of one test over one target directory (two gates at once) would
/// otherwise share it, and the second run's removal would make the first
/// run's replacement a first server again.
fn once_file(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("{name}.{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path.to_string_lossy().into_owned()
}

/// The reference plugin in `TagPassthrough`: with nothing wired it renders a
/// constant (its tag), so a block of silence is a plugin that is not there.
const PASSTHROUGH: (&str, &str) = ("TUTTI_CLAP_PROBE_RENDER_MODE", "1");

/// The probe applies the parameter writes it is sent (its `Gain`) to its
/// audio, which is also what makes it take them from the host at all.
const APPLY_GAIN: (&str, &str) = ("TUTTI_CLAP_PROBE_APPLY_GAIN", "1");

/// Renders one block and returns whether any sample was non-zero.
fn renders_sound(s: &mut Session) -> bool {
    let mut out = vec![0.0f32; BLOCK * 2];
    s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
    out.iter().any(|&x| x != 0.0)
}

/// A virtual clock for the session's phases, and every event they reported
/// (the plugin tasks' ends included: a task started in a frame runs on a pool thread, and
/// its end lands in the `Phase::Plugins` of a later frame).
struct Drive {
    now: Instant,
    events: Vec<SessionEvent>,
    /// Checked in every frame once a `Reloading` has been reported, after the
    /// plugins' phase started the reload task, which runs on a pool thread meanwhile.
    at_reload: Option<Box<dyn Fn()>>,
    /// Whether a `Reloading` has been reported.
    reload_reported: bool,
}

impl Drive {
    fn new() -> Self {
        Self {
            now: Instant::now(),
            events: Vec::new(),
            at_reload: None,
            reload_reported: false,
        }
    }

    /// One frame: a block rendered (the server gets real time to act), the
    /// plugins' phase at the clock 20 ms on, landing what the tasks sent.
    fn step(&mut self, s: &mut Session) {
        let _ = renders_sound(s);
        std::thread::sleep(Duration::from_millis(5));
        self.now += Duration::from_millis(20);
        let mut out = Vec::new();
        s.run_phase(Phase::Plugins, PhaseCx::at(self.now), &mut out);
        self.reload_reported |= out.iter().any(|e| {
            matches!(
                e,
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::Reloading { .. },
                    ..
                })
            )
        });
        if self.reload_reported
            && let Some(check) = &self.at_reload
        {
            check();
        }
        self.events.extend(out);
        s.run_phase(Phase::Collect, PhaseCx::at(self.now), &mut self.events);
        // The commit that puts a replacement into the executor's plan, and the
        // collect that frees the plan's old unit on this thread.
        s.run_phase(Phase::Commit, PhaseCx::at(self.now), &mut Vec::new());
        s.run_phase(Phase::Collect, PhaseCx::at(self.now), &mut self.events);
    }

    /// Steps until an event satisfies `stop` (10 s of wall time at most).
    fn until(&mut self, s: &mut Session, why: &str, stop: impl Fn(&SessionEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.events.iter().any(&stop) {
            assert!(Instant::now() < deadline, "{why}: {:?}", self.events);
            self.step(s);
        }
    }

    fn count(&self, f: impl Fn(&SessionEvent) -> bool) -> usize {
        self.events.iter().filter(|e| f(e)).count()
    }
}

/// **A crashed plugin comes back at its node with its last state.** The
/// probe's first server aborts on its 60th block (a replacement does not: the
/// once-file); before that a parameter is set and a snapshot captured. The
/// death is reported as `PluginChange::Reloading` with the node kept, and the load
/// is handed out as a task; its landing puts the replacement at the same node
/// with its wiring (the outputs still read it, so the render is sound again
/// with no re-wiring), the parameter back at the snapshot's value, and
/// `PluginChange::Recovered` saying the state was restored, how old, and that the
/// latency moved by the replacement's extra (the plan's total with it: the
/// commit re-plans PDC).
#[test]
fn a_crashed_plugin_comes_back_at_its_node_with_its_last_state() {
    const EXTRA: usize = 48;
    let once = once_file("recovery-crash.once");
    let mut s = session_built(
        &[
            PASSTHROUGH,
            APPLY_GAIN,
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "60"),
            ("TUTTI_CLAP_PROBE_RESTART_LATENCY", &EXTRA.to_string()),
        ],
        |b| b.snapshot_interval(Duration::from_secs(1)),
    );
    let (node, _) = wired_probe(&mut s);
    // The graph's figure for the node: the probe's own and the pipeline's.
    let base = s.graph_ref().node_latency(node).unwrap();
    let id = gain_param(&s, node);

    // A value worth restoring, and a snapshot of it.
    let target = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(id)
        .expect("the plugin declares the parameter")
        .target();

    s.set(target.to(tutti_units::Normalized::of(0.25)))
        .expect("the gain is writable");
    take_value(&mut s, node, id);
    let t0 = Instant::now();
    snapshot_now(&mut s, node);
    let total_before = s.latency();

    // Render into the crash, then through the recovery.
    let mut drive = Drive::new();
    drive.now = t0 + Duration::from_secs(3);
    drive.until(&mut s, "recovered", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Recovered { .. },
                ..
            })
        )
    });
    let kinds: Vec<&str> = drive
        .events
        .iter()
        .map(|e| match e {
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { .. },
                ..
            }) => "reloading",
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Recovered { .. },
                ..
            }) => "recovered",
            _ => "other",
        })
        .filter(|k| *k != "other")
        .collect();
    assert_eq!(kinds, ["reloading", "recovered"], "{:?}", drive.events);
    assert_eq!(
        drive.count(|e| matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Died { .. },
                ..
            })
        )),
        0
    );
    let Some(SessionEvent::Plugin(PluginEvent {
        node: at,
        change:
            PluginChange::Recovered {
                state,
                latency_before,
                latency_after,
                attempts,
            },
        ..
    })) = drive.events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Recovered { .. },
                ..
            })
        )
    })
    else {
        unreachable!()
    };
    assert_eq!(*at, node, "the same node");
    assert!(
        matches!(state, tutti_session::StateRestore::Restored { .. }),
        "{state:?}"
    );
    assert_eq!(*attempts, 1);
    assert_eq!(*latency_before, base);
    assert_eq!(
        *latency_after,
        base + Samples::new(EXTRA),
        "the replacement's latency"
    );
    assert!(s.graph_ref().contains(node));
    assert!(matches!(
        s.plugin_view(node).unwrap().health(),
        PluginHealth::Running
    ));
    // The snapshot is still the node's recovery point: a second failure
    // restores it again.
    assert!(s.plugin_view(node).unwrap().state().last_good().is_some());

    // The state came back.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !is_quarter(param(&s, node, id)) {
        assert!(
            Instant::now() < deadline,
            "the replacement's parameter is {:?}, not the snapshot's 0.25",
            param(&s, node, id)
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // And it stays restored: nothing writes the load-time values over it
    // (a write rides the next blocks to the server, so render some).
    for _ in 0..40 {
        let _ = renders_sound(&mut s);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        is_quarter(param(&s, node, id)),
        "the restored parameter was overwritten: {:?}",
        param(&s, node, id)
    );

    // The commit re-plans PDC for the new latency, and the outputs (never
    // re-wired) read the replacement.
    let _ = s.update();
    let total_after = s.latency();
    assert_eq!(
        total_after.get(),
        total_before.get() + EXTRA,
        "PDC follows the latency"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !renders_sound(&mut s) {
        assert!(
            Instant::now() < deadline,
            "the recovered plugin renders nothing"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// **A plugin that fails every load is given up on, and its node stays silent.**
/// Every server aborts on its first block, so each reload comes up and dies:
/// two reloads (the policy's budget), then `PluginChange::GaveUp` naming the failures,
/// the node and its record kept, and no `PluginChange::Died`.
#[test]
fn a_plugin_that_crashes_every_time_is_given_up_on() {
    let mut s = session_built(&[("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1")], |b| {
        b.plugin_recovery(tutti_session::RecoveryPolicy {
            max_failures: 2,
            backoff: Duration::from_millis(1),
            ..Default::default()
        })
    });
    let (node, _) = wired_probe(&mut s);
    let mut drive = Drive::new();
    drive.until(&mut s, "given up", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::GaveUp { .. },
                ..
            })
        )
    });
    assert_eq!(
        drive.count(|e| matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { .. },
                ..
            })
        )),
        2,
        "{:?}",
        drive.events
    );
    assert_eq!(
        drive.count(|e| matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Died { .. },
                ..
            })
        )),
        0
    );
    let Some(SessionEvent::Plugin(PluginEvent {
        node: at,
        change: PluginChange::GaveUp { why, .. },
        ..
    })) = drive.events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::GaveUp { .. },
                ..
            })
        )
    })
    else {
        unreachable!()
    };
    assert_eq!(*at, node);
    assert_eq!(why.failures, 3);
    assert!(
        matches!(
            &why.last,
            ReloadCause::Died(PluginFailure::StreamClosed | PluginFailure::ServerExited { .. })
        ),
        "a replacement that aborts on its first block ended its process and closed its stream: {:?}",
        why.last
    );
    assert!(s.graph_ref().contains(node), "the node stays at its key");
    assert!(s.plugin_view(node).is_ok(), "and its record");
}

/// **A node that gave up is revived by a reload composed from what its handle
/// and its view read.** The node's servers abort on their 40th block, so it gives
/// up with the gain set to 0.25 held in its record. The load of the handle's id
/// and hosting, started from the withheld state (else the last good), swapped in
/// at the node's key, renders again with that gain.
#[test]
fn a_gave_up_node_is_revived_by_a_load_composed_from_its_state() {
    let (catalog, id, _dir) = crate::loading::catalog_of(&PluginRef::file(clap_probe()));
    let mut s = session_built(
        &[
            PASSTHROUGH,
            APPLY_GAIN,
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "40"),
        ],
        |b| {
            b.snapshot_interval(Duration::from_secs(1)).plugin_recovery(
                tutti_session::RecoveryPolicy {
                    max_failures: 2,
                    backoff: Duration::from_millis(1),
                    ..Default::default()
                },
            )
        },
    );
    let main = tutti_fixtures::main_thread();
    let (handle, node) = s
        .load_from(main, &catalog, &id, Hosting::OutOfProcess, None)
        .expect("the reference plugin loads");
    s.graph().connect(node, MainOutput).unwrap();
    let _ = s.update();
    let gain = gain_param(&s, node.erase());
    let target = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(gain)
        .expect("the plugin declares the parameter")
        .target();
    s.set(target.to(tutti_units::Normalized::of(0.25))).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !is_quarter(param(&s, node.erase(), gain)) {
        assert!(Instant::now() < deadline, "the probe never took the value");
        let _ = s.update();
        std::thread::yield_now();
    }
    snapshot_now(&mut s, node.erase());
    let mut drive = Drive::new();
    drive.until(&mut s, "given up", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::GaveUp { .. },
                ..
            })
        )
    });
    // SAFETY: nextest runs each test in its own process, and the servers that
    // read the variable are spawned from this thread, after this call.
    unsafe { std::env::remove_var("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK") };

    let state = {
        let view = s.plugin_view(node).unwrap();
        view.state()
            .withheld()
            .or(view.state().last_good())
            .cloned()
            .expect("a node that gave up still reads the state worth keeping")
    };
    let info = handle.info();
    let mut load = match s
        .load_plugin_as(info.id(), &catalog, info.hosting())
        .unwrap()
    {
        tutti_session::LoadPlugin::OutOfProcess(load) => load,
        other => panic!("{other:?}"),
    };
    load.initial_state(state).expect("its own plugin's state");
    let revived = load.block().expect("the plugin loads again");
    s.swap_plugin(node, revived, main).expect("it swaps in");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(is_quarter(param(&s, node.erase(), gain)) && renders_sound(&mut s)) {
        assert!(
            Instant::now() < deadline,
            "the revived node never rendered with its state"
        );
        let _ = s.update();
        std::thread::yield_now();
    }
    assert!(matches!(
        s.plugin_view(node).unwrap().health(),
        PluginHealth::Running
    ));
}

#[cfg(target_os = "linux")]
mod reaped_on_drop {
    use super::*;

    /// **Dropping a session with a reload in flight waits for it, so the replacement's
    /// server is reaped.** The first server aborts on its 60th
    /// block; the replacement restores a state its load holds on a fifo until a releaser
    /// thread opens and closes it. The reload task is inside `PluginLoad::run` (it holds the
    /// load and, through its upgrade, the executor only for the launch) when the session
    /// drops: the drop waits for the task, the replacement lands in the inbox, and its
    /// server dies with the inbox. No child of this process outlives the drop.
    #[test]
    fn dropping_a_session_with_a_reload_in_flight_reaps_the_replacement_server() {
        let once = once_file("drop-mid-reload.once");
        let fifo = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("drop-mid-reload.fifo.{}", std::process::id()));
        let _ = std::fs::remove_file(&fifo);
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(made.success());
        let mut s = session_built(
            &[
                PASSTHROUGH,
                ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
                ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "60"),
                (
                    "TUTTI_CLAP_PROBE_STATE_LOAD_HOLD_FIFO",
                    fifo.to_str().expect("a utf-8 path"),
                ),
            ],
            |b| b.snapshot_interval(Duration::from_secs(1)),
        );
        let (node, _) = wired_probe(&mut s);
        snapshot_now(&mut s, node);

        // The death, and the reload handed out: its state restore holds on the fifo.
        let mut drive = Drive::new();
        drive.until(&mut s, "the reload starts", |e| {
            matches!(
                e,
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::Reloading { .. },
                    ..
                })
            )
        });
        let guard = Instant::now() + Duration::from_secs(30);
        while s.plugin_tasks() == 0 {
            assert!(Instant::now() < guard, "the reload was never handed out");
            drive.step(&mut s);
        }
        // Opening the fifo for writing waits for the plugin's read, and closing it releases
        // the restore.
        let releaser = std::thread::spawn({
            let fifo = fifo.clone();
            move || drop(std::fs::OpenOptions::new().write(true).open(fifo))
        });

        drop(s);
        // A session dropped before the plugin opened the fifo leaves the releaser blocked
        // (or not yet started) opening it for writing. Opening it for reading and writing
        // never blocks and pairs with that open, whenever the releaser reaches it; the
        // handle stays open until the releaser ended, so the pairing cannot be missed.
        let pairing = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .expect("the fifo opens");
        releaser.join().expect("the releaser ended");
        drop(pairing);
        assert_eq!(
            children_of_this_process(),
            Vec::<u32>::new(),
            "a plugin-server outlived the session"
        );
        let _ = std::fs::remove_file(&fifo);
    }

    /// The pids of this process's live children, from the kernel.
    fn children_of_this_process() -> Vec<u32> {
        let mut pids = Vec::new();
        for task in std::fs::read_dir("/proc/self/task").expect("the task list") {
            let path = task.expect("a task").path().join("children");
            if let Ok(list) = std::fs::read_to_string(path) {
                pids.extend(
                    list.split_whitespace()
                        .filter_map(|pid| pid.parse::<u32>().ok()),
                );
            }
        }
        pids
    }
}

/// **A reload completes with no host code but `update()`.** The probe's first
/// server aborts on its first block. The host does nothing with tasks: it renders
/// and calls `update()`, on the real clock, under a threaded executor. The death
/// is declared, the reload is started on the executor, runs off this thread,
/// and lands in a later `update()`: the node is
/// `Recovered`, and renders again.
#[test]
fn a_reload_completes_with_no_host_code_but_update() {
    let once = once_file("recovery-update-only.once");
    let mut s = session_built(
        &[
            PASSTHROUGH,
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1"),
        ],
        |b| {
            b.plugin_recovery(tutti_session::RecoveryPolicy {
                backoff: Duration::from_millis(1),
                ..Default::default()
            })
            // The session's own executor runs each task: nothing here touches a task but
            // to start it.
        },
    );
    let (node, _) = wired_probe(&mut s);

    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !events.iter().any(|e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Recovered { .. },
                ..
            })
        )
    }) {
        assert!(
            Instant::now() < deadline,
            "the reload never completed: {events:?}"
        );
        let _ = renders_sound(&mut s);
        events.extend(s.update());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { .. },
                ..
            })
        )),
        "the death was reported before the recovery: {events:?}"
    );
    assert!(matches!(
        s.plugin_view(node).unwrap().health(),
        PluginHealth::Running
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !renders_sound(&mut s) {
        assert!(Instant::now() < deadline, "the replacement renders nothing");
        let _ = s.update();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// **A killed server reads `Died`, then `Reloading`, through `health()` and
/// the events.** The probe aborts on its first block; once the bridge has
/// latched the death, and before the session has written it off (it waits out
/// its debounce), the view reads `Died` with the engine's cause. The update
/// that declares it schedules the reload: the event is `Reloading`, and the view
/// then reads `Reloading` with the same attempt and cause.
#[test]
fn a_killed_server_reads_died_then_reloading() {
    let once = once_file("health-died-reloading.once");
    let mut s = session_built(
        &[
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1"),
        ],
        |b| b.plugin_recovery(tutti_session::RecoveryPolicy::default()),
    );
    let (node, _) = wired_probe(&mut s);
    assert!(
        matches!(s.plugin_view(node).unwrap().health(), PluginHealth::Running),
        "a plugin that has not died is running"
    );

    let mut out = vec![0.0f32; BLOCK * 2];
    let deadline = Instant::now() + Duration::from_secs(10);
    while matches!(s.plugin_view(node).unwrap().health(), PluginHealth::Running) {
        assert!(Instant::now() < deadline, "the probe never died");
        s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
        std::thread::sleep(Duration::from_millis(5));
    }
    let PluginHealth::Died { cause } = s.plugin_view(node).unwrap().health() else {
        panic!("a latched death reads Died before the session writes it off");
    };

    let mut now = Instant::now();
    let reloading = loop {
        assert!(Instant::now() < deadline, "the death was never declared");
        std::thread::sleep(Duration::from_millis(20));
        now += Duration::from_millis(20);
        let mut events = Vec::new();
        s.run_phase(Phase::Plugins, PhaseCx::at(now), &mut events);
        if let Some(found) = events.into_iter().find_map(|e| match e {
            SessionEvent::Plugin(PluginEvent {
                node: n,
                change: PluginChange::Reloading { cause, attempt, .. },
                ..
            }) if n == node => Some((cause, attempt)),
            _ => None,
        }) {
            break found;
        }
    };
    assert_eq!(reloading.1, 1, "the first failure is attempt 1");
    assert!(
        matches!(&reloading.0, ReloadCause::Died(c) if std::mem::discriminant(c) == std::mem::discriminant(&cause)),
        "the reload's cause is the death's: {:?} vs {cause:?}",
        reloading.0
    );
    assert!(
        matches!(
            s.plugin_view(node).unwrap().health(),
            PluginHealth::Reloading {
                attempt: 1,
                cause: ReloadCause::Died(_)
            }
        ),
        "{:?}",
        s.plugin_view(node).unwrap().health()
    );
}

/// A policy that suspects a hang after 16 blocks and 100 ms of the clock the
/// test drives (20 ms a step).
fn quick_hang_policy() -> tutti_session::HangPolicy {
    tutti_session::HangPolicy {
        blocks: 16,
        window: Duration::from_millis(100),
        ..Default::default()
    }
}

/// A reload one millisecond after a death.
fn quick_reload_policy() -> tutti_session::RecoveryPolicy {
    tutti_session::RecoveryPolicy {
        backoff: Duration::from_millis(1),
        ..Default::default()
    }
}

/// **With recovery off, a plugin that stops answering is declared hung and
/// silenced.** The same probe as above parks in `process()` for good; no policy
/// reloads it, yet the hang is detected: the session ends its server, the death
/// reads "stopped answering", and `PluginChange::Died` names it and the node
/// stays at its key, as for any dead plugin with recovery off.
#[test]
fn a_plugin_that_stops_answering_is_declared_hung_with_recovery_off() {
    let once = once_file("recovery-off-hang.once");
    let mut s = session_built(
        &[
            PASSTHROUGH,
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_BLOCK_FROM", "20"),
        ],
        |b| b.plugin_hang(quick_hang_policy()).no_plugin_recovery(),
    );
    let (node, _) = wired_probe(&mut s);

    let mut drive = Drive::new();
    drive.until(&mut s, "died", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Died { .. },
                ..
            })
        )
    });
    let Some(SessionEvent::Plugin(PluginEvent {
        node: died,
        change: PluginChange::Died { cause },
        ..
    })) = drive.events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Died { .. },
                ..
            })
        )
    })
    else {
        unreachable!()
    };
    assert_eq!(*died, node);
    assert!(
        matches!(
            cause,
            PluginFailure::Terminated {
                reason: tutti_plugin_types::TerminationReason::Hung { .. }
            }
        ),
        "{cause:?}"
    );
    assert!(
        s.graph_ref().contains(node),
        "the hung plugin's node stays at its key"
    );
}

/// **A slow plugin that still answers is not declared hung.** Every block
/// takes the probe 80 ms, far over the bridge's budget for a 512-frame block:
/// blocks are abandoned all the time, and every one's reply comes back late.
/// Abandoned blocks pass the policy's 16 many times over, but replies keep
/// arriving between the readings, so the plugin is left alone for four
/// seconds; both totals did rise (else the test proves nothing).
#[test]
fn a_slow_plugin_that_still_answers_is_not_declared_hung() {
    let mut s = session_built(
        &[PASSTHROUGH, ("TUTTI_CLAP_PROBE_STALL_US", "80000")],
        |b| {
            b.plugin_hang(quick_hang_policy())
                .plugin_recovery(quick_reload_policy())
        },
    );
    let (node, _) = wired_probe(&mut s);
    let mut drive = Drive::new();
    // Four seconds of wall time: at 80 ms a block, with the bridge's budget
    // missed on most, far more than 16 blocks are abandoned over it.
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until {
        drive.step(&mut s);
    }
    let counts = s.plugin_view(node).unwrap().diagnostics().block_counts();
    assert!(
        counts.abandoned > 16,
        "{counts:?}: the plugin is not slow enough"
    );
    assert!(counts.replies > 0, "{counts:?}: it never answered");
    assert_eq!(
        drive.count(|e| matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { .. },
                ..
            })
        )),
        0
    );
    // Slow, so it may read `Stalled` while a reading catches blocks unanswered;
    // what it must not be is dead or being replaced.
    assert!(
        matches!(
            s.plugin_view(node).unwrap().health(),
            PluginHealth::Running | PluginHealth::Stalled
        ),
        "{:?}",
        s.plugin_view(node).unwrap().health()
    );
}

/// **A replacement with other ports is a failed attempt, and the graph stays
/// editable.** The replacement server reports an extra aux bus (an updated
/// binary whose buses changed). The node's edges stay as they are, so
/// swapping it in would leave the topology invalid and refuse every later
/// commit while the session called the plugin healthy. It is refused before
/// the swap: the next attempt is scheduled (the same refusal again, until
/// the budget is spent), the reason reaches `PluginChange::GaveUp`, and an edit made
/// meanwhile commits.
#[test]
fn a_replacement_with_other_ports_is_refused_and_the_graph_stays_editable() {
    let once = once_file("recovery-ports.once");
    let mut s = session_built(
        &[
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1"),
            ("TUTTI_CLAP_PROBE_RESTART_PORT_LAYOUT", "1"),
        ],
        |b| {
            b.snapshot_interval(Duration::from_secs(1)).plugin_recovery(
                tutti_session::RecoveryPolicy {
                    max_failures: 2,
                    backoff: Duration::from_millis(1),
                    ..Default::default()
                },
            )
        },
    );
    let (node, _) = wired_probe(&mut s);
    snapshot_now(&mut s, node);
    let kept = s
        .plugin_view(node)
        .unwrap()
        .state()
        .last_good()
        .unwrap()
        .bytes()
        .to_vec();
    let mut drive = Drive::new();
    drive.until(&mut s, "a second attempt", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { attempt: 2, .. },
                ..
            })
        )
    });
    assert_eq!(
        drive.count(|e| matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Recovered { .. },
                ..
            })
        )),
        0,
        "{:?}",
        drive.events
    );
    // An edit made now commits.
    let extra = s.graph().add(ForkByClone(DropWitness(Arc::default())));
    assert!(s.graph_ref().has_uncommitted_edits());
    // The commit alone, on the drive's clock: `update()` would run
    // `Phase::Plugins` on the real one, where a server launch (hundreds of ms on
    // macOS) outlasts the 1 ms reload backoff and hands out a reload task this test
    // would discard.
    s.run_phase(Phase::Commit, PhaseCx::at(drive.now), &mut Vec::new());
    assert!(
        !s.graph_ref().has_uncommitted_edits(),
        "the commit was refused: the topology is invalid"
    );
    assert!(s.graph_ref().contains(extra));

    drive.until(&mut s, "given up", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::GaveUp { .. },
                ..
            })
        )
    });
    let Some(SessionEvent::Plugin(PluginEvent {
        change: PluginChange::GaveUp { why, state, .. },
        ..
    })) = drive.events.iter().find(|e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::GaveUp { .. },
                ..
            })
        )
    })
    else {
        unreachable!()
    };
    assert!(
        matches!(
            why.last,
            ReloadCause::Attempt(RecoveryFailure::ReplacementPorts { got, was }) if got != was
        ),
        "{why}"
    );
    assert_eq!(
        state.as_deref().map(PeerState::bytes),
        Some(kept.as_slice()),
        "the user's last state goes out with the event"
    );
    assert!(s.graph_ref().contains(node), "the node stays at its key");
    let _ = s.update();
    assert!(!s.graph_ref().has_uncommitted_edits());
}

/// Captures a snapshot of `node` at once, as a host's worker would (the
/// session's snapshot cadence is 1 s here, driven by the virtual clock). The first phase
/// starts the cadence, the second, a virtual 2 s on, asks the plugin, and the phases after
/// it land the task's end; a hang guard bounds the wait, and a plugin that died first ends it at
/// once.
fn snapshot_now(s: &mut Session, node: NodeKey) {
    let t0 = Instant::now();
    s.run_phase(Phase::Plugins, PhaseCx::at(t0), &mut Vec::new());
    let cx = PhaseCx::at(t0 + Duration::from_millis(2_000));
    let guard = Instant::now() + Duration::from_secs(30);
    while s.plugin_view(node).unwrap().state().last_good().is_none() {
        assert!(Instant::now() < guard, "the snapshot never landed");
        let mut out = Vec::new();
        s.run_phase(Phase::Plugins, cx, &mut out);
        // A plugin that died before it was asked has nothing to snapshot.
        assert!(
            !out.iter().any(|e| matches!(
                e,
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::Reloading { .. }
                        | PluginChange::Died { .. }
                        | PluginChange::GaveUp { .. },
                    ..
                })
            )),
            "the plugin died before its snapshot: {out:?}"
        );
        std::thread::yield_now();
    }
}

fn recovered_state(drive: &Drive) -> Vec<tutti_session::StateRestore> {
    drive
        .events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Recovered { state, .. },
                ..
            }) => Some(state.clone()),
            _ => None,
        })
        .collect()
}

/// **A snapshot that kills the plugin loading it is not tried again.** The
/// first server dies; every replacement dies when it is handed a state. The
/// first attempt (with the snapshot) fails before the plugin is installed,
/// so it is never reported recovered; the second goes without the state,
/// comes up, and says so: `StateRestore::Withheld`, the snapshot gone from
/// the record.
#[test]
fn a_snapshot_that_kills_the_load_is_withheld_from_the_next_attempt() {
    let once = once_file("recovery-withheld.once");
    let mut s = session_built(
        &[
            PASSTHROUGH,
            APPLY_GAIN,
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "40"),
            ("TUTTI_CLAP_PROBE_CRASH_ON_STATE_LOAD", "1"),
        ],
        |b| {
            b.snapshot_interval(Duration::from_secs(1)).plugin_recovery(
                tutti_session::RecoveryPolicy {
                    backoff: Duration::from_millis(1),
                    ..Default::default()
                },
            )
        },
    );
    let (node, _) = wired_probe(&mut s);
    let id = gain_param(&s, node);
    // A state that is not the plugin's default.
    let target = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(id)
        .expect("the plugin declares the parameter")
        .target();

    s.set(target.to(tutti_units::Normalized::of(0.25)))
        .expect("the gain is writable");
    take_value(&mut s, node, id);
    snapshot_now(&mut s, node);
    let users = s
        .plugin_view(node)
        .unwrap()
        .state()
        .last_good()
        .unwrap()
        .bytes()
        .to_vec();
    let mut drive = Drive::new();
    drive.until(&mut s, "recovered", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Recovered { .. },
                ..
            })
        )
    });
    let states = recovered_state(&drive);
    assert!(
        matches!(
            states.as_slice(),
            [tutti_session::StateRestore::Withheld { .. }]
        ),
        "{states:?} in {:?}",
        drive.events
    );
    assert_eq!(
        drive.count(|e| matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { .. },
                ..
            })
        )),
        2,
        "the attempt with the snapshot failed first"
    );
    assert!(s.plugin_view(node).unwrap().state().last_good().is_none());
    assert!(matches!(
        s.plugin_view(node).unwrap().health(),
        PluginHealth::Running
    ));
    // The user's state is kept for the host, not thrown away...
    assert_eq!(
        s.plugin_view(node)
            .unwrap()
            .state()
            .withheld()
            .map(PeerState::bytes),
        Some(users.as_slice())
    );
    // ...and the snapshots taken since, of a plugin at its defaults, do not
    // replace it.
    snapshot_now(&mut s, node);
    let fresh = s
        .plugin_view(node)
        .unwrap()
        .state()
        .last_good()
        .expect("a new snapshot")
        .bytes();
    assert_ne!(fresh, users.as_slice(), "the plugin is at its defaults");
    assert_eq!(
        s.plugin_view(node)
            .unwrap()
            .state()
            .withheld()
            .map(PeerState::bytes),
        Some(users.as_slice()),
        "a default-state snapshot replaced the withheld one"
    );
}

/// **A plugin that dies soon after restoring a snapshot loses it for the next
/// load.** Every server aborts on its third block. The first replacement
/// takes the snapshot (`Restored`) and dies within the probation, so the
/// next load goes without it (`Withheld`); a death outside the probation
/// would have used the snapshot again.
#[test]
fn a_plugin_that_dies_soon_after_restoring_loses_the_snapshot() {
    let mut s = session_built(&[("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "3")], |b| {
        b.snapshot_interval(Duration::from_secs(1))
            .plugin_recovery(tutti_session::RecoveryPolicy {
                max_failures: 5,
                backoff: Duration::from_millis(1),
                ..Default::default()
            })
    });
    let (node, _) = wired_probe(&mut s);
    snapshot_now(&mut s, node);
    let mut drive = Drive::new();
    drive.until(&mut s, "a recovery without the snapshot", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Recovered {
                    state: tutti_session::StateRestore::Withheld { .. },
                    ..
                },
                ..
            })
        )
    });
    let states = recovered_state(&drive);
    assert!(
        matches!(
            states.as_slice(),
            [
                tutti_session::StateRestore::Restored { .. },
                tutti_session::StateRestore::Withheld { .. }
            ]
        ),
        "{states:?}"
    );
}

/// **An export started while the plugin is being replaced is refused.** The
/// dead plugin cannot hand over its state for the fork, so the gather names
/// the failure; the session and its render mode are left as they were, and an
/// export started after the recovery works.
///
/// It pins what the fork of a dead plugin does today.
#[test]
fn an_export_during_the_recovery_window_is_refused() {
    let once = once_file("recovery-export.once");
    let mut s = session_built(
        &[
            ("TUTTI_CLAP_PROBE_ONCE_FILE", &once),
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "1"),
        ],
        |b| {
            b.plugin_recovery(tutti_session::RecoveryPolicy {
                backoff: Duration::from_secs(3_600),
                ..Default::default()
            })
        },
    );
    let (node, _) = wired_probe(&mut s);
    // Dead and waiting for a backoff that does not end.
    let mut drive = Drive::new();
    drive.until(&mut s, "reloading", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Reloading { .. },
                ..
            })
        )
    });
    assert!(matches!(
        s.plugin_view(node).unwrap().health(),
        PluginHealth::Reloading { .. }
    ));
    let refused = s.bounce(request());
    let refusal = format!("{:?}", refused.as_ref().err());
    assert!(
        refusal.contains("SaveState(PluginCrashed)"),
        "the fork of a dead plugin is refused: {refusal}"
    );
    assert_eq!(s.render_mode(), RenderMode::Realtime);
}

/// Copies stereo through, and cancels the export it renders in when it
/// processes its second block. Forked by clone: the fork shares the handle.
#[derive(Clone)]
struct CancelOnSecondBlock {
    blocks: Arc<std::sync::atomic::AtomicUsize>,
    handle: Arc<std::sync::OnceLock<tutti_session::BounceHandle>>,
}

impl tutti_graph::Node for CancelOnSecondBlock {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::through_unassigned(ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let (ins, mut outs) = io.split();
        for c in 0..2 {
            outs.get(c).copy_from_slice(ins.get(c));
        }
        if self
            .blocks
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
            == 2
        {
            self.handle.get().expect("the handle was set").cancel();
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A cancelled export over a hosted plugin's fork** stops at its second
/// block, reports `Cancelled`, releases the token on `apply`, takes
/// its forked `plugin-server` with it (the fork dies with the rendered graph,
/// on the main thread), and the next export, forking a fresh server, succeeds.
/// The server count is read from `/proc`, so that part is Linux only.
#[test]
fn a_cancelled_export_frees_the_token_and_the_forked_server() {
    use std::sync::atomic::AtomicUsize;
    let mut s = session(&[]);
    let probe = add_probe(&mut s);
    let blocks = Arc::new(AtomicUsize::new(0));
    let handle = Arc::new(std::sync::OnceLock::new());
    let cancel = s.graph().add(ForkByClone(CancelOnSecondBlock {
        blocks: Arc::clone(&blocks),
        handle: Arc::clone(&handle),
    }));
    s.graph()
        .set_sources(
            InPort::new(cancel, 0),
            [tutti_session::OutPort::new(probe, 0)],
        )
        .unwrap();
    s.graph()
        .set_sources(
            InPort::new(cancel, 1),
            [tutti_session::OutPort::new(probe, 1)],
        )
        .unwrap();
    s.graph().connect(cancel, MainOutput).unwrap();
    let _ = s.update();
    let live = servers();

    let job = s.bounce(request_for(5.0)).expect("starts");
    handle.set(job.handle()).ok();
    let rendered = block_on(job);
    assert!(
        blocks.load(std::sync::atomic::Ordering::SeqCst) < 20,
        "the render stopped at the cancel, not at its end of 235 blocks"
    );
    assert!(
        matches!(s.bounce(request()), Err(BounceError::InFlight)),
        "the token lives until the apply"
    );
    if let Some(live) = live {
        assert_eq!(servers(), Some(live + 1), "the export's fork has a server");
    }
    assert!(
        matches!(s.apply(rendered), Err(BounceError::Cancelled)),
        "cancelled"
    );
    if let Some(live) = live {
        let deadline = Instant::now() + Duration::from_secs(10);
        while servers() != Some(live) {
            assert!(Instant::now() < deadline, "the fork's server never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    bounced(&mut s, request()).expect("the next export is not refused, and renders to its end");
}

/// **A replacement that refused the snapshot is not blamed for its death.**
/// Every server aborts on its third block and refuses any state. The first
/// replacement is `Refused` (it never restored anything), and its death is
/// not the snapshot's doing: the snapshot is offered again (`Refused`
/// again), none is withheld.
#[test]
fn a_death_after_a_refused_restore_does_not_withhold_the_snapshot() {
    let mut s = session_built(
        &[
            ("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", "3"),
            ("TUTTI_CLAP_PROBE_REFUSE_STATE_LOAD", "1"),
        ],
        |b| {
            b.snapshot_interval(Duration::from_secs(1)).plugin_recovery(
                tutti_session::RecoveryPolicy {
                    max_failures: 5,
                    backoff: Duration::from_millis(1),
                    ..Default::default()
                },
            )
        },
    );
    let (node, _) = wired_probe(&mut s);
    snapshot_now(&mut s, node);
    let mut drive = Drive::new();
    drive.until(&mut s, "two recoveries", |e| {
        matches!(
            e,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::Recovered { attempts: 2, .. },
                ..
            })
        )
    });
    let states = recovered_state(&drive);
    assert!(
        states
            .iter()
            .all(|s| matches!(s, tutti_session::StateRestore::Refused { .. })),
        "{states:?}"
    );
    assert!(s.plugin_view(node).unwrap().state().withheld().is_none());
    assert!(s.plugin_view(node).unwrap().state().last_good().is_some());
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg_attr(target_os = "linux", path = "plugin/servers_linux.rs")]
#[cfg_attr(not(target_os = "linux"), path = "plugin/servers_other.rs")]
mod servers_of;
use servers_of::servers;
