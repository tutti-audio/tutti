//! A plugin hosted in process, as a pool entry: its main-thread stage, and the
//! drop of what only the main thread may drop. The reference CLAP plugin
//! (`Hosting::InProcess`) resolved through a catalog that probed it. The served
//! half is `tests/plugin/plugin_pool.rs`.
//!
//! Build the probe first: `cargo build -p tutti-plugin-probe`.

use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use tutti_clap_test_plugin::{ClapProbe, ClapSwitch, Site, ThreadAnswer};

use tutti_plugin::catalog::{Catalog, CatalogConfig, Hosting, Locator, NO_SCAN_DIRS, PluginId};
use tutti_plugin_db::{PluginDb, PluginFormat, ScanPoll};
use tutti_session::{
    LoadError, Peer, PoolError, PoolEvent, PoolState, Presence, Session, SessionEvent,
};
use tutti_types::{MaxBlock, SampleRate, Samples};

use crate::support::clap_probe;

fn probe() -> &'static ClapProbe {
    static PROBE: OnceLock<ClapProbe> = OnceLock::new();
    PROBE.get_or_init(|| {
        ClapProbe::open(
            &tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
            &clap_probe(),
        )
    })
}

/// Where the plugin's last `deactivate` ran: the in-process instance closes
/// there when its last owner drops.
fn deactivation() -> ThreadAnswer {
    probe().thread_capture().sites[Site::Deactivate as usize]
}

fn second_id() -> PluginId {
    PluginId::new(
        PluginFormat::Clap,
        tutti_clap_test_plugin::SECOND_PLUGIN_ID
            .to_str()
            .expect("utf-8"),
    )
}

fn id() -> PluginId {
    PluginId::new(
        PluginFormat::Clap,
        tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8"),
    )
}

/// A catalog that probed the reference plugin.
fn catalog() -> (Catalog, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let config = CatalogConfig::new(dir.path(), NO_SCAN_DIRS)
        .probe_locator(Locator::at(tutti_fixtures::plugin_probe()));
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));
    let mut task = catalog.register(clap_probe()).expect("a plugin file");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match catalog.try_apply(&mut task) {
            ScanPoll::Running => {
                assert!(Instant::now() < deadline, "the probe never ended");
                thread::sleep(Duration::from_millis(2));
            }
            ScanPoll::Done(result) => {
                assert!(result.failures.is_empty(), "{:?}", result.failures);
                break;
            }
            ScanPoll::Died => panic!("the probe's worker died"),
        }
    }
    (catalog, dir)
}

fn session() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples(512)))
        .build()
        .expect("an offline session builds")
}

/// Updates, on this (the main) thread, until the key's next pool event.
fn next_event<C: tutti_session::Clock>(s: &mut Session<C>, key: &'static str) -> PoolEvent {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        for event in s.update() {
            if let SessionEvent::Pool { key: k, event } = event
                && k.get::<&'static str>() == Some(&key)
            {
                return event;
            }
        }
        assert!(Instant::now() < deadline, "no event for {key}");
        thread::sleep(Duration::from_millis(2));
    }
}

/// **An in-process plugin's load finishes in an update on the main thread**:
/// the entry is not loaded until one runs, then it is a plugin hosted in this
/// process, live, and its node is hosted here.
fn an_in_process_plugin_lands_in_an_update_on_the_main_thread() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    s.pool(
        s.plugin_entry("verb", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .expect("this is the main thread");
    assert!(s.plugin(&"verb").is_err(), "nothing lands before an update");
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Loaded));

    let handle = s.plugin(&"verb").expect("loaded");
    assert_eq!(handle.info().hosting(), Hosting::InProcess);
    assert!(matches!(handle.presence(), Presence::Live));
    let node = s.add_pooled_plugin(&"verb").expect("the plugin enters");
    assert!(
        s.plugin_view(node)
            .unwrap()
            .diagnostics()
            .server_pid()
            .is_none(),
        "no server: the plugin is in this process"
    );
    s.close(tutti_fixtures::main_thread());
}

/// **A declaration with a main-thread stage is refused where no main thread
/// will run it, whole, and a served one is not.** A thread that is not the
/// main thread, on a session no update has run on the main thread for, gets
/// `NeedsMainThread` naming the key.
fn an_in_process_plugin_needs_the_main_thread_and_is_refused_without_one() {
    let (catalog, _dir) = catalog();
    let s = session();
    let in_process = s
        .plugin_entry("verb", &id(), &catalog)
        .hosting(Hosting::InProcess);
    let served = s
        .plugin_entry("verb", &id(), &catalog)
        .hosting(Hosting::OutOfProcess);
    let (refused, accepted, s) = thread::spawn(move || {
        let mut s = s;
        let refused = s.pool(in_process);
        assert!(
            s.pool_state(&"verb").is_none(),
            "the refusal applies nothing"
        );
        let accepted = s.pool(served);
        (refused, accepted, s)
    })
    .join()
    .unwrap();
    assert_eq!(refused, Err(PoolError::NeedsMainThread("verb")));
    assert_eq!(accepted, Ok(()));
    assert!(matches!(s.pool_state(&"verb"), Some(PoolState::Loading)));
    s.close(tutti_fixtures::main_thread());
}

/// **A released entry's in-process plugin is closed on the main thread**,
/// whichever thread releases it: the release in an update on another thread
/// leaves the plugin open (its `deactivate` has not run), and the main thread's
/// next update closes it, the plugin saying it ran there.
fn a_released_in_process_plugin_is_closed_on_the_main_thread() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let mut s = session();
    s.pool(
        s.plugin_entry("verb", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Loaded));

    let mut s = thread::spawn(move || {
        let mut s = s;
        s.pool::<&'static str>(()).expect("an empty set");
        let events = s.update();
        assert!(
            events.iter().any(|event| matches!(
                event,
                SessionEvent::Pool {
                    event: PoolEvent::Released,
                    ..
                }
            )),
            "the entry is released on this thread"
        );
        s
    })
    .join()
    .unwrap();
    assert!(
        !deactivation().observed,
        "released off the main thread, the plugin is still open"
    );
    let _ = s.update();
    let closed = deactivation();
    assert!(closed.observed, "the main thread's update closed it");
    assert!(closed.is_main, "and the plugin saw the main thread");
    s.close(tutti_fixtures::main_thread());
}

/// **Released on the main thread, the plugin closes there at once.**
fn a_released_in_process_plugin_on_the_main_thread_is_closed_in_place() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let mut s = session();
    s.pool(
        s.plugin_entry("verb", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Loaded));
    assert!(!deactivation().observed);
    s.pool::<&'static str>(()).unwrap();
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Released));
    let closed = deactivation();
    assert!(closed.observed && closed.is_main, "{closed:?}");
    s.close(tutti_fixtures::main_thread());
}

/// The key's pool events of the first update that has any, on this thread.
fn events_of(s: &mut Session, key: &'static str) -> Vec<PoolEvent> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let events: Vec<PoolEvent> = s
            .update()
            .into_iter()
            .filter_map(|event| match event {
                SessionEvent::Pool { key: k, event } if k.get::<&'static str>() == Some(&key) => {
                    Some(event)
                }
                _ => None,
            })
            .collect();
        if !events.is_empty() {
            return events;
        }
        assert!(Instant::now() < deadline, "no event for {key}");
        thread::sleep(Duration::from_millis(2));
    }
}

/// **A changed identity swaps an in-process node too**: the landing runs in an
/// update on the main thread, and the node is the second plugin after it.
fn a_changed_identity_swaps_an_in_process_node() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    s.pool(
        s.plugin_entry("fx", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "fx"), PoolEvent::Loaded));
    let node = s.add_pooled_plugin(&"fx").unwrap();
    let first = s.plugin(&"fx").unwrap();
    let second = tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap();
    assert_ne!(s.plugin_view(node).unwrap().descriptor().id, second);

    s.pool(
        s.plugin_entry("fx", &second_id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "fx"), PoolEvent::Reloaded));
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.plugin_view(node).unwrap().descriptor().id != second {
        assert!(Instant::now() < deadline, "the node never swapped");
        let _ = s.update();
        thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(first.presence(), Presence::Closed));
    assert!(matches!(s.pool_state(&"fx"), Some(PoolState::Loaded)));
    s.close(tutti_fixtures::main_thread());
}

/// **An in-process plugin with other ports fails the entry and the node keeps
/// its plugin**, as a served one does.
fn an_in_process_swap_to_other_ports_fails_the_entry() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    s.pool(
        s.plugin_entry("fx", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "fx"), PoolEvent::Loaded));
    let node = s.add_pooled_plugin(&"fx").unwrap();
    let before = s.plugin_view(node).unwrap().descriptor().id.clone();

    // The next plugin opened has the probe's aux-bus layout.
    probe().set(ClapSwitch::PortLayout, 1);
    probe().thread_reset();
    s.pool(
        s.plugin_entry("fx", &second_id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    let mut events = Vec::new();
    while !events
        .iter()
        .any(|event| matches!(event, PoolEvent::Failed(_)))
    {
        events.extend(events_of(&mut s, "fx"));
    }
    assert!(
        matches!(
            events.as_slice(),
            [
                PoolEvent::Reloaded,
                PoolEvent::Failed(LoadError::PluginPorts { .. })
            ]
        ),
        "{events:?}"
    );
    assert_eq!(s.plugin_view(node).unwrap().descriptor().id, before);
    // The unused plugin was closed where the refusal ran, on the main thread,
    // and the graph still commits.
    let closed = deactivation();
    assert!(closed.observed && closed.is_main, "{closed:?}");
    assert!(
        s.update()
            .iter()
            .all(|event| !matches!(event, SessionEvent::CommitRefused(_))),
        "the graph still commits"
    );
    s.close(tutti_fixtures::main_thread());
}

/// **An in-process swap that waits is made on the main thread only**: with the
/// graph resumed by updates off the main thread, the node keeps its plugin
/// (binding the new one and closing the old are main-thread work), and the
/// main thread's next update swaps.
fn an_in_process_swap_waits_for_the_main_thread() {
    let (catalog, _dir) = catalog();
    let mut s = Session::builder()
        .max_block(MaxBlock::of(Samples(512)))
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    s.pool(
        s.plugin_entry("fx", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "fx"), PoolEvent::Loaded));
    let node = s.add_pooled_plugin(&"fx").unwrap();
    let second = tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap();

    // The graph re-prepares, so the landing's swap waits.
    let mut side = s.graph().take_audio_side();
    s.graph().set_sample_rate(SampleRate::SR_44K1);
    assert!(s.graph_ref().testing().is_repreparing(), "mid re-prepare");
    s.pool(
        s.plugin_entry("fx", &second_id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "fx"), PoolEvent::Reloaded));
    assert_ne!(s.plugin_view(node).unwrap().descriptor().id, second);

    // Off the main thread the re-prepare resumes, and the swap still waits.
    let mut s = thread::spawn(move || {
        let mut s = s;
        let mut out = vec![Vec::new(), Vec::new()];
        let deadline = Instant::now() + Duration::from_secs(10);
        while s.graph_ref().testing().is_repreparing() {
            assert!(Instant::now() < deadline, "the re-prepare never resumed");
            side.render(512, 512, &mut out);
            let _ = s.update();
        }
        let _ = s.update();
        assert_ne!(
            s.plugin_view(node).unwrap().descriptor().id,
            second,
            "an update off the main thread does not swap an in-process plugin"
        );
        s
    })
    .join()
    .unwrap();
    let _ = s.update();
    assert_eq!(s.plugin_view(node).unwrap().descriptor().id, second);
    s.close(tutti_fixtures::main_thread());
}

/// A value saved through `save_state`, then moved, is the value the plugin and
/// its mirror hold after `restore_state` lands in an update on the main thread;
/// the outcome lands in an update, never at the call.
fn restore_round_trip(hosting: Hosting) {
    const CUTOFF: tutti_plugin_types::ParamId = tutti_plugin_types::ParamId::new(101);
    let (catalog, _dir) = catalog();
    let mut s = session();
    s.pool(s.plugin_entry("verb", &id(), &catalog).hosting(hosting))
        .unwrap();
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Loaded));
    let node = s.add_pooled_plugin(&"verb").unwrap();
    // Wired to the output, so blocks run the plugin: an in-process plugin takes
    // a host write at the head of its next block.
    s.graph().connect(node, tutti_session::Root).unwrap();
    let blocks = |s: &mut Session| {
        for _ in 0..4 {
            let _ = s.update();
            let mut out = vec![0.0f32; 512 * 2];
            s.render(
                &mut tutti_buffers::InterleavedMut::new(
                    &mut out,
                    tutti_types::ChannelLayout::STEREO,
                )
                .unwrap(),
            );
        }
    };
    blocks(&mut s);
    let knob = |s: &Session| {
        s.plugin_view(node)
            .unwrap()
            .params()
            .get(CUTOFF)
            .unwrap()
            .normalized()
            .get()
    };
    let set = |s: &Session, v: f64| {
        s.plugin_view(node)
            .unwrap()
            .params()
            .get(CUTOFF)
            .unwrap()
            .set(tutti_plugin_types::Normalized::of(v))
            .unwrap();
    };

    set(&s, 0.9);
    blocks(&mut s);
    let saving = s.save_state(&"verb").expect("a node holds the plugin");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !saving.has_landed() {
        assert!(Instant::now() < deadline, "the save never landed");
        let _ = s.update();
        thread::sleep(Duration::from_millis(2));
    }
    let state = saving.take().expect("landed").expect("a state");

    set(&s, 0.1);
    blocks(&mut s);
    let restoring = s.restore_state(&"verb", state).expect("its own plugin's");
    assert!(!restoring.has_landed(), "it lands in an update");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !restoring.has_landed() {
        assert!(Instant::now() < deadline, "the restore never landed");
        let _ = s.update();
        thread::sleep(Duration::from_millis(2));
    }
    restoring.take().expect("landed").expect("applied");
    // The mirror is read again by a job the next updates land.
    let deadline = Instant::now() + Duration::from_secs(30);
    while (knob(&s) - 0.9).abs() > 1e-6 {
        assert!(
            Instant::now() < deadline,
            "the saved value never came back: {}",
            knob(&s)
        );
        blocks(&mut s);
        thread::sleep(Duration::from_millis(2));
    }
    s.close(tutti_fixtures::main_thread());
}

/// **A restore job applies the saved state to an in-process plugin**: its
/// writes reach the plugin at the head of a block, so the saved state holds the
/// value the blocks delivered, and the restore moves it back.
fn a_restore_job_applies_the_saved_state_to_an_in_process_plugin() {
    restore_round_trip(Hosting::InProcess);
}

/// **A restore job applies the saved state to a served plugin**, in the first
/// update on the main thread.
fn a_restore_job_applies_the_saved_state_to_a_served_plugin() {
    restore_round_trip(Hosting::OutOfProcess);
}

/// **An in-process save the main thread does not run answers `Unavailable`:**
/// the worker waits the off-main deadline for the main thread's work, which no
/// update runs, and the job ends as `StateJobFailed::State(Unavailable)`.
fn an_in_process_save_the_main_thread_does_not_run_is_unavailable() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    s.pool(
        s.plugin_entry("verb", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Loaded));
    let _node = s.add_pooled_plugin(&"verb").unwrap();
    let saving = s.save_state(&"verb").expect("a node holds the plugin");
    // No update on the main thread meanwhile, for more than the off-main
    // deadline: the job's end only lands in an update, so the wait cannot be
    // polled, and the worker has timed out long before the first update.
    thread::sleep(Duration::from_secs(4));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !saving.has_landed() {
        assert!(Instant::now() < deadline, "the save never landed");
        let _ = s.update();
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        matches!(
            saving.take(),
            Some(Err(tutti_session::StateJobFailed::State(
                tutti_plugin::error::StateError::Unavailable
            )))
        ),
        "the main thread did not run it in time"
    );
    s.close(tutti_fixtures::main_thread());
}

/// A pooled in-process plugin at a key, in `layout`'s ports, rendering its
/// closed-form tags (`TagOnly`: output channel `c` of port `p` holds
/// `probe_tag(p, c)`), its stereo input fed by two constants and its first two
/// outputs on the root.
struct Rebound {
    s: Session,
    node: tutti_session::Key<tutti_session::PluginNode>,
    a: tutti_session::Key<tutti_graph::ForkByClone<tutti_nodes::testing::Const>>,
    b: tutti_session::Key<tutti_graph::ForkByClone<tutti_nodes::testing::Const>>,
}

fn rebound(layout: u32) -> Rebound {
    use tutti_graph::ForkByClone;
    use tutti_nodes::testing::Const;
    use tutti_session::Root;
    probe().set(ClapSwitch::PortLayout, layout);
    probe().set(ClapSwitch::RenderMode, 2);
    let (catalog, _dir) = catalog();
    let mut s = session();
    s.pool(
        s.plugin_entry("verb", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Loaded));
    let node = s.add_pooled_plugin(&"verb").unwrap();
    let g = s.graph();
    let (a, _) = g.add(ForkByClone(Const::mono(0.25)));
    let (b, _) = g.add(ForkByClone(Const::mono(0.5)));
    for (from, to) in [
        (tutti_session::OutPort::new(a, 0), node.input(0)),
        (tutti_session::OutPort::new(b, 0), node.input(1)),
        (node.out(0), Root::output(0)),
        (node.out(1), Root::output(1)),
    ] {
        g.connect(from, to).unwrap().hard();
    }
    // Commits the wiring.
    let _ = s.update();
    Rebound { s, node, a, b }
}

impl Rebound {
    /// Updates and renders until `done`, returning every update's events.
    fn until(
        &mut self,
        what: &str,
        mut done: impl FnMut(&mut Session) -> bool,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            events.extend(self.s.update());
            let mut out = vec![0.0f32; 512 * 2];
            self.s.render(
                &mut tutti_buffers::InterleavedMut::new(
                    &mut out,
                    tutti_types::ChannelLayout::STEREO,
                )
                .unwrap(),
            );
            if done(&mut self.s) {
                return events;
            }
            assert!(Instant::now() < deadline, "{what} never happened");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// The root's channel `ch` of one rendered block.
    fn channel(&mut self, ch: usize) -> Vec<f32> {
        let mut block = vec![f32::NAN; 512 * 2];
        self.s.render(
            &mut tutti_buffers::InterleavedMut::new(&mut block, tutti_types::ChannelLayout::STEREO)
                .unwrap(),
        );
        block.iter().skip(ch).step_by(2).copied().collect()
    }

    /// The plugin announces new ports.
    fn change_ports(&self, layout: u32) {
        probe().set(ClapSwitch::PortLayout, layout);
        // `LIST | CHANNEL_COUNT`: what a host restarts the plugin for.
        probe().set(ClapSwitch::RequestPortsRescan, 0x24);
    }

    /// The plugin renders through its node: output 0 holds the tag of port 0,
    /// channel 0, and output 1 that of channel 1.
    fn renders(&mut self) -> bool {
        self.channel(0).iter().all(|&v| v == 1.0) && self.channel(1).iter().all(|&v| v == 2.0)
    }
}

fn unwired_of(events: &[SessionEvent]) -> Vec<(tutti_graph::NodeKey, Vec<tutti_session::Edge>)> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Unwired { node, edges, .. } => Some((*node, edges.clone())),
            _ => None,
        })
        .collect()
}

/// **A plugin that widens its ports gets a node with them, at the same key,
/// and renders through it.** The stereo plugin announces `[2, 1]` buses: the
/// node at its key declares three outputs, nothing is unwired, the plugin runs
/// on (it was told to restart once, by its own change, and not again for the
/// move), and its tags reach the root through the new node.
fn a_plugin_that_widens_its_ports_is_re_inserted_wider() {
    let mut r = rebound(0);
    let node = r.node;
    assert_eq!(r.s.graph_ref().node_outputs(node).unwrap(), 2);
    r.until("a block", |s| s.graph_ref().node_outputs(node).is_ok());
    assert!(r.renders(), "the plugin renders through its first node");

    probe().reset_activation();
    r.change_ports(1);
    let events = r.until("the wider node", |s| {
        s.graph_ref().node_outputs(node).is_ok_and(|n| n == 3)
    });
    assert!(unwired_of(&events).is_empty(), "wider unwires nothing");
    assert_eq!(r.s.graph_ref().node_inputs(node).unwrap(), 3);
    // The plugin restarted once, for its own change of ports; the move of its
    // Processor to the new node restarts nothing.
    let restarts = probe().activation_count();
    assert_eq!(restarts, 1, "one activation, the plugin's own restart");
    // The Processor moves to the new node at a later tick.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !r.renders() {
        assert!(Instant::now() < deadline, "the new node never rendered");
        let _ = r.s.update();
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        probe().activation_count(),
        restarts,
        "the Processor moved with no further activation"
    );
    r.s.close(tutti_fixtures::main_thread());
}

/// **A plugin that narrows its ports gets a node with them, and what it
/// dropped is reported exactly.** The stereo effect announces an instrument
/// layout (no inputs): the two edges into its inputs are `Unwired`, once, and
/// its outputs, still wired to the root, carry the plugin's tags.
fn a_plugin_that_narrows_its_ports_reports_the_wiring_it_lost() {
    use tutti_session::{Edge, OutPort};
    let mut r = rebound(0);
    let node = r.node;
    let (a, b) = (r.a, r.b);
    assert!(r.renders());

    r.change_ports(2);
    let events = r.until("the narrower node", |s| {
        s.graph_ref().node_inputs(node).is_ok_and(|n| n == 0)
    });
    let [(unwired_node, mut edges)] = <[_; 1]>::try_from(unwired_of(&events))
        .unwrap_or_else(|v| panic!("one Unwired for the re-inserted key, got {v:?}"));
    assert_eq!(unwired_node, node.erase());
    edges.sort();
    let mut want = vec![
        Edge::new(OutPort::new(a, 0), node.input(0)),
        Edge::new(OutPort::new(b, 0), node.input(1)),
    ];
    want.sort();
    assert_eq!(edges, want, "exactly the edges into the inputs it lost");
    assert_eq!(r.s.graph_ref().node_outputs(node).unwrap(), 2);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !r.renders() {
        assert!(Instant::now() < deadline, "the new node never rendered");
        let _ = r.s.update();
        thread::sleep(Duration::from_millis(2));
    }
    let events = r.s.update();
    assert!(unwired_of(&events).is_empty(), "reported once");
    r.s.close(tutti_fixtures::main_thread());
}

/// **A plugin whose new ports are wider than a block serves is refused by
/// name, once, and its node stays.** The stereo plugin announces one bus of 17
/// channels: the session reports [`PluginChange::PortsRefused`] with
/// `TooWide`, the node at the key keeps its two outputs, and nothing is
/// reported again.
fn a_plugin_that_grows_wider_than_a_block_is_refused_by_name() {
    use tutti_session::{PluginChange, PluginEvent, PortsRefusal};
    let mut r = rebound(0);
    let node = r.node;
    probe().reset_activation();
    r.change_ports(4);
    let mut refusals = Vec::new();
    let events = r.until("the refusal", |_| true);
    let mut seen = events;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        refusals.extend(seen.drain(..).filter_map(|event| match event {
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::PortsRefused { why },
                ..
            }) => Some(why),
            _ => None,
        }));
        if !refusals.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "no refusal was reported");
        seen = r.until("an update", |_| true);
    }
    assert!(
        matches!(
            refusals.as_slice(),
            [PortsRefusal::Ports(
                tutti_plugin_host::RebindError::TooWide(_)
            )]
        ),
        "{refusals:?}"
    );
    assert_eq!(
        r.s.graph_ref().node_outputs(node).unwrap(),
        2,
        "the node stays"
    );
    for _ in 0..8 {
        let again = r.until("an update", |_| true);
        assert!(
            !again.iter().any(|event| matches!(
                event,
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::PortsRefused { .. },
                    ..
                })
            )),
            "reported once"
        );
    }
    r.s.close(tutti_fixtures::main_thread());
}

/// **A node muted before its plugin changes ports is still muted after:** the
/// swap keeps the mode of the key. (A bypassed node is not run, so it never
/// hands its Processor back and its plugin's restart waits for the bypass to
/// end; a muted node keeps running.)
fn a_muted_node_stays_muted_through_a_port_change() {
    use tutti_session::NodeMode;
    let mut r = rebound(0);
    let node = r.node;
    r.s.graph()
        .set_mode(node, NodeMode::Muted)
        .expect("a node can be muted");
    r.change_ports(1);
    let _ = r.until("the wider node", |s| {
        s.graph_ref().node_outputs(node).is_ok_and(|n| n == 3)
    });
    assert_eq!(r.s.graph_ref().mode(node), Ok(NodeMode::Muted));
    r.s.close(tutti_fixtures::main_thread());
}

/// **A port change announced while the graph re-prepares for another rate
/// ends with the new ports, and is never refused.** The sample-rate change is
/// under way when the plugin announces its ports: no refusal is reported, and
/// the node ends with the new ports. (The retry of a swap the graph refuses as
/// busy is not forced here: the re-prepare may resume before the rebuild
/// runs.)
fn a_port_change_during_a_sample_rate_change_ends_with_the_new_ports() {
    use tutti_session::{PluginChange, PluginEvent};
    probe().set(ClapSwitch::PortLayout, 0);
    probe().set(ClapSwitch::RenderMode, 2);
    let (catalog, _dir) = catalog();
    let mut s = Session::builder()
        .max_block(MaxBlock::of(Samples(512)))
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    s.pool(
        s.plugin_entry("fx", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "fx"), PoolEvent::Loaded));
    let node = s.add_pooled_plugin(&"fx").unwrap();
    let mut side = s.graph().take_audio_side();
    let mut out = vec![Vec::new(), Vec::new()];
    // The plugin has its Processor in the graph before the re-prepare.
    let _ = s.update();
    side.render(512, 512, &mut out);
    s.graph().set_sample_rate(SampleRate::SR_44K1);
    assert!(s.graph_ref().testing().is_repreparing(), "mid re-prepare");
    probe().set(ClapSwitch::PortLayout, 1);
    probe().set(ClapSwitch::RequestPortsRescan, 0x24);
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while s.graph_ref().node_outputs(node).is_ok_and(|n| n != 3) {
        assert!(Instant::now() < deadline, "the wider node never came");
        side.render(512, 512, &mut out);
        events.extend(s.update());
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        !events.iter().any(|event| matches!(
            event,
            SessionEvent::Plugin(PluginEvent {
                change: PluginChange::PortsRefused { .. },
                ..
            })
        )),
        "a graph that re-prepares is waited for, not refused"
    );
    s.close(tutti_fixtures::main_thread());
}

/// **A node removed while its rebuild is settling leaves nothing behind.** The
/// record, its rebinder and the plugin it keeps alive go with the node: the
/// next updates report nothing about the key, and the plugin is closed.
fn a_node_removed_while_its_rebuild_settles_leaves_nothing_behind() {
    let mut r = rebound(0);
    let node = r.node;
    r.change_ports(1);
    let _ = r.until("the wider node", |s| {
        s.graph_ref().node_outputs(node).is_ok_and(|n| n == 3)
    });
    assert_eq!(
        r.s.graph_ref().testing().rebuild_settling(node),
        Some(true),
        "the Processor has not reached the new node yet"
    );
    // The plugin's own restart deactivated it already; what follows is the close.
    probe().thread_reset();
    assert!(!deactivation().observed);
    r.s.graph().remove(node).expect("the node is there");
    let events = r.until("the node to be gone", |s| s.plugin_view(node).is_err());
    assert!(
        !events.iter().any(|event| matches!(
            event,
            SessionEvent::Plugin(tutti_session::PluginEvent { node: n, .. })
                if *n == node.erase()
        )),
        "nothing is said of a removed node: {events:?}"
    );
    // The removed record waits in the graveyard for the main-thread update; the
    // pool entry's hold goes with its release. Then nothing keeps the plugin open.
    let _ = r.s.main(tutti_fixtures::main_thread()).update();
    r.s.pool::<&'static str>(()).unwrap();
    assert!(matches!(next_event(&mut r.s, "verb"), PoolEvent::Released));
    let _ = r.until("the plugin to close", |s| {
        let _ = s.main(tutti_fixtures::main_thread()).update();
        deactivation().observed
    });
    let closed = deactivation();
    assert!(
        closed.observed && closed.is_main,
        "the plugin was closed, on the main thread: no rebinder kept it open: {closed:?}"
    );
    r.s.close(tutti_fixtures::main_thread());
}

pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["plugin_pool_main";
an_in_process_plugin_lands_in_an_update_on_the_main_thread,
an_in_process_plugin_needs_the_main_thread_and_is_refused_without_one,
a_released_in_process_plugin_is_closed_on_the_main_thread,
a_released_in_process_plugin_on_the_main_thread_is_closed_in_place,
a_changed_identity_swaps_an_in_process_node,
an_in_process_swap_to_other_ports_fails_the_entry,
an_in_process_swap_waits_for_the_main_thread,
a_restore_job_applies_the_saved_state_to_an_in_process_plugin,
a_restore_job_applies_the_saved_state_to_a_served_plugin,
an_in_process_save_the_main_thread_does_not_run_is_unavailable,
a_plugin_that_widens_its_ports_is_re_inserted_wider,
a_plugin_that_narrows_its_ports_reports_the_wiring_it_lost,
a_plugin_that_grows_wider_than_a_block_is_refused_by_name,
a_muted_node_stays_muted_through_a_port_change,
a_port_change_during_a_sample_rate_change_ends_with_the_new_ports,
a_node_removed_while_its_rebuild_settles_leaves_nothing_behind,
];
