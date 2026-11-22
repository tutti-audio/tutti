//! A plugin hosted in process, through the session: the swap of the plugin at a
//! node, a state restore, and a plugin that changes its ports. The reference CLAP
//! plugin (`Hosting::InProcess`) resolved through a catalog that probed it. The
//! load and the drop of what only the main thread may drop are
//! `plugin_load_main.rs`; the served half is `tests/plugin/plugin_served.rs`.
//!
//! Build the probe first: `cargo build -p tutti-plugin-probe`.

use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use tutti_buffers::PlanesMut;
use tutti_platform::Instant;

use tutti_clap_test_plugin::{ClapProbe, ClapSwitch, Site, ThreadAnswer};

use crate::loading::Perform as _;
use crate::loading::{Hosted as _, LoadFrom as _};
use tutti_buffers::MaxBlock;
use tutti_plugin_db::PluginDb;
use tutti_plugin_db::{Catalog, CatalogConfig, Hosting};
use tutti_plugin_types::{Locator, PluginId};
use tutti_session::{PluginChange, Session, SessionEvent};
use tutti_units::{SampleRate, Samples};

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
    PluginId::Clap(
        tutti_clap_test_plugin::SECOND_PLUGIN_ID
            .to_str()
            .expect("utf-8")
            .into(),
    )
}

fn id() -> PluginId {
    PluginId::Clap(
        tutti_clap_test_plugin::PLUGIN_ID
            .to_str()
            .expect("utf-8")
            .into(),
    )
}

/// A catalog that probed the reference plugin.
fn catalog() -> (Catalog, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let config =
        CatalogConfig::new(dir.path()).probe_locator(Locator::at(tutti_fixtures::plugin_probe()));
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));
    let task = catalog
        .register(clap_probe(), tutti_fixtures::executor().downgrade())
        .expect("a plugin file");
    let scanned = tutti_fixtures::block_on(task).expect("the executor lives");
    let result = catalog
        .apply(scanned)
        .expect("the output of this catalog's own scan");
    assert!(result.failures().is_empty(), "{:?}", result.failures());
    (catalog, dir)
}

fn session() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("an offline session builds")
}

/// **A swap replaces an in-process node's plugin too**: the swap runs on the
/// main thread, which its proof says, and the node is the second plugin after
/// it, the first closing on the main thread.
fn a_swap_replaces_an_in_process_nodes_plugin() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let mut s = session();
    let main = tutti_fixtures::main_thread();
    let (_, node) = s
        .load_from(main, &catalog, &id(), Hosting::InProcess, None)
        .unwrap();
    let second = tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap();
    assert_ne!(
        crate::loading::clap_id(s.plugin_view(node).unwrap().plugin()),
        second
    );

    let next = s.load_in_process(&second_id(), &catalog).run(main).unwrap();
    s.swap_plugin(node, next, main).unwrap();
    assert_eq!(
        crate::loading::clap_id(s.plugin_view(node).unwrap().plugin()),
        second
    );
    // The outgoing plugin retires to the graveyard and closes in an update here.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !deactivation().observed {
        assert!(Instant::now() < deadline, "the first plugin never closed");
        let _ = s.update();
        thread::yield_now();
    }
    assert!(deactivation().is_main, "{:?}", deactivation());
    s.close(main);
}

/// **A swap from an in-process plugin to a served one takes the served kind's
/// recovery.** The node was hosted in process, which is never reloaded; once a
/// served plugin is swapped in and its server dies, the session reloads it, as
/// it does a served plugin it loaded itself.
fn a_served_plugin_swapped_over_an_in_process_one_is_reloaded_when_it_dies() {
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let (catalog, _dir) = catalog();
    let mut s = session();
    let main = tutti_fixtures::main_thread();
    let (_, node) = s
        .load_from(main, &catalog, &id(), Hosting::InProcess, None)
        .unwrap();
    let served = {
        use crate::loading::Block as _;
        s.load_out_of_process(&second_id(), &catalog)
            .block()
            .expect("the served plugin loads")
    };
    s.swap_plugin(node, served, main).unwrap();
    s.plugin_view(node).unwrap().diagnostics().terminate(
        tutti_plugin_types::TerminationReason::Hung {
            blocks: 16,
            held: Duration::from_secs(5),
        },
    );
    // Only a hang guards this loop: the session reports what it does about
    // the death, a reload or the end of the node's plugin.
    let guard = Instant::now() + Duration::from_secs(60);
    let change = 'seen: loop {
        for event in s.update() {
            if let SessionEvent::Plugin(event) = event
                && matches!(
                    event.change,
                    PluginChange::Reloading { .. } | PluginChange::Died { .. }
                )
            {
                break 'seen event.change;
            }
        }
        assert!(Instant::now() < guard, "the death was never seen");
        thread::yield_now();
    };
    assert!(
        matches!(change, PluginChange::Reloading { .. }),
        "a served plugin is reloaded: {change:?}"
    );
    s.close(main);
}

/// **An in-process plugin with other ports is refused by a swap and the node
/// keeps its plugin**, as a served one does; the refused plugin stays loaded,
/// and the graph still commits.
fn an_in_process_swap_to_other_ports_is_refused() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    let main = tutti_fixtures::main_thread();
    let (_, node) = s
        .load_from(main, &catalog, &id(), Hosting::InProcess, None)
        .unwrap();
    let before = crate::loading::clap_id(s.plugin_view(node).unwrap().plugin()).to_owned();

    // The next plugin opened has the probe's aux-bus layout.
    probe().set(ClapSwitch::PortLayout, 1);
    probe().thread_reset();
    let other = s.load_in_process(&second_id(), &catalog).run(main).unwrap();
    let refused = s.swap_plugin(node, other, main);
    let Err(tutti_session::SwapPluginError::Ports { node: other, .. }) = refused else {
        panic!("other ports are refused: {refused:?}");
    };
    assert_eq!(
        crate::loading::clap_id(s.plugin_view(node).unwrap().plugin()),
        before
    );
    assert!(
        !deactivation().observed,
        "the refused plugin is still loaded, and open"
    );
    assert!(
        s.update()
            .iter()
            .all(|event| !matches!(event, SessionEvent::CommitRefused(_))),
        "the graph still commits"
    );
    drop(other);
    let closed = deactivation();
    assert!(closed.observed && closed.is_main, "{closed:?}");
    s.close(main);
}

/// A value saved through `save_state`, then moved, is the value the plugin and
/// its mirror hold after `restore_state` is run on the main thread and applied.
fn restore_round_trip(hosting: Hosting) {
    const CUTOFF: tutti_units::ParamId = tutti_units::ParamId::new(101);
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let (catalog, _dir) = catalog();
    let mut s = session();
    let (_, node) = s
        .load_from(
            tutti_fixtures::main_thread(),
            &catalog,
            &id(),
            hosting,
            None,
        )
        .unwrap();
    // Wired to the output, so blocks run the plugin: an in-process plugin takes
    // a host write at the head of its next block.
    s.graph().connect(node, tutti_session::MainOutput).unwrap();
    let blocks = |s: &mut Session| {
        for _ in 0..4 {
            let _ = s.update();
            let mut out = vec![0.0f32; 512 * 2];
            s.render(
                &mut tutti_buffers::InterleavedMut::new(
                    &mut out,
                    tutti_buffers::ChannelLayout::STEREO,
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
        let target = s
            .plugin_view(node)
            .unwrap()
            .params()
            .get(CUTOFF)
            .expect("the plugin declares the parameter")
            .target();

        s.set(target.to(tutti_units::Normalized::of(v))).unwrap();
    };

    set(&s, 0.9);
    blocks(&mut s);
    let main = tutti_fixtures::main_thread();
    // The arm of each value is the node's hosting.
    let in_process = matches!(hosting, Hosting::InProcess);
    let save = s
        .save_state(&node, tutti_plugin_types::StateUse::Project)
        .expect("a node holds the plugin");
    assert_eq!(
        matches!(save, tutti_session::SaveState::InProcess(_)),
        in_process,
        "{save:?}"
    );
    let state = save.perform(main).expect("a state");

    set(&s, 0.1);
    blocks(&mut s);
    let restore = s.restore_state(&node, state).expect("its own plugin's");
    assert_eq!(
        matches!(restore, tutti_session::RestoreState::InProcess(_)),
        in_process,
        "{restore:?}"
    );
    let restored = restore.perform(main).expect("the plugin takes it");
    s.apply(restored).expect("the node still hosts the plugin");
    // The mirror is read again by the next updates.
    let deadline = Instant::now() + Duration::from_secs(30);
    while (knob(&s) - 0.9).abs() > 1e-6 {
        assert!(
            Instant::now() < deadline,
            "the saved value never came back: {}",
            knob(&s)
        );
        blocks(&mut s);
        thread::yield_now();
    }
    s.close(tutti_fixtures::main_thread());
}

/// **A restore applies the saved state to an in-process plugin**: its
/// writes reach the plugin at the head of a block, so the saved state holds the
/// value the blocks delivered, and the restore moves it back.
fn a_restore_applies_the_saved_state_to_an_in_process_plugin() {
    restore_round_trip(Hosting::InProcess);
}

/// **A restore applies the saved state to a served plugin**.
fn a_restore_applies_the_saved_state_to_a_served_plugin() {
    restore_round_trip(Hosting::OutOfProcess);
}

/// An in-process plugin in a node, in `layout`'s ports, rendering its
/// closed-form tags (`TagOnly`: output channel `c` of port `p` holds
/// `probe_tag(p, c)`), its stereo input fed by two constants and its first two
/// outputs on the main output.
struct Rebound {
    s: Session,
    node: tutti_session::Key<tutti_session::AnyPlugin>,
    a: tutti_session::Key<tutti_graph::ForkByClone<tutti_nodes::testing::Const>>,
    b: tutti_session::Key<tutti_graph::ForkByClone<tutti_nodes::testing::Const>>,
}

fn rebound(layout: u32) -> Rebound {
    use tutti_graph::ForkByClone;
    use tutti_nodes::testing::Const;
    use tutti_session::MainOutput;
    probe().set(ClapSwitch::PortLayout, layout);
    probe().set(ClapSwitch::RenderMode, 2);
    let (catalog, _dir) = catalog();
    let mut s = session();
    let (_, node) = s
        .load_from(
            tutti_fixtures::main_thread(),
            &catalog,
            &id(),
            Hosting::InProcess,
            None,
        )
        .unwrap();
    let g = s.graph();
    let a = g.add(ForkByClone(Const::mono(0.25)));
    let b = g.add(ForkByClone(Const::mono(0.5)));
    for (from, to) in [
        (tutti_session::OutputPort::new(a, 0), node.input(0)),
        (tutti_session::OutputPort::new(b, 0), node.input(1)),
        (node.output(0), MainOutput::channel(0)),
        (node.output(1), MainOutput::channel(1)),
    ] {
        g.connect(from, to).unwrap().unfaded();
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
                    tutti_buffers::ChannelLayout::STEREO,
                )
                .unwrap(),
            );
            if done(&mut self.s) {
                return events;
            }
            assert!(Instant::now() < deadline, "{what} never happened");
            thread::yield_now();
        }
    }

    /// The main output's channel `ch` of one rendered block.
    fn channel(&mut self, ch: usize) -> Vec<f32> {
        let mut block = vec![f32::NAN; 512 * 2];
        self.s.render(
            &mut tutti_buffers::InterleavedMut::new(
                &mut block,
                tutti_buffers::ChannelLayout::STEREO,
            )
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

fn unwired_of(
    events: &[SessionEvent],
) -> Vec<(tutti_graph::NodeKey, Vec<tutti_session::ChannelEdge>)> {
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
/// move), and its tags reach the main output through the new node.
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
        thread::yield_now();
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
/// its outputs, still wired to the main output, carry the plugin's tags.
fn a_plugin_that_narrows_its_ports_reports_the_wiring_it_lost() {
    use tutti_session::{ChannelEdge, OutputPort};
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
        ChannelEdge::new(OutputPort::new(a, 0), node.input(0)),
        ChannelEdge::new(OutputPort::new(b, 0), node.input(1)),
    ];
    want.sort();
    assert_eq!(edges, want, "exactly the edges into the inputs it lost");
    assert_eq!(r.s.graph_ref().node_outputs(node).unwrap(), 2);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !r.renders() {
        assert!(Instant::now() < deadline, "the new node never rendered");
        let _ = r.s.update();
        thread::yield_now();
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
/// swap keeps the mode of the key.
fn a_muted_node_stays_muted_through_a_port_change() {
    use tutti_session::{ModeTarget, NodeMode};
    let mut r = rebound(0);
    let node = r.node;
    r.s.set(ModeTarget::of(node).to(NodeMode::Muted))
        .expect("a node can be muted");
    r.change_ports(1);
    let _ = r.until("the wider node", |s| {
        s.graph_ref().node_outputs(node).is_ok_and(|n| n == 3)
    });
    assert_eq!(r.s.get(ModeTarget::of(node)).unwrap(), NodeMode::Muted);
    r.s.close(tutti_fixtures::main_thread());
}

/// **A bypassed node's plugin changes its ports while the bypass lasts:** the
/// parked node answers the restart's recall and the commit's relend, so the
/// wider node is at the key, still bypassed, and holds the Processor (the
/// rebuild stops settling).
fn a_bypassed_plugin_node_is_re_inserted_and_takes_its_processor() {
    use tutti_session::{ModeTarget, NodeMode};
    let mut r = rebound(0);
    let node = r.node;
    r.until("a block", |s| s.graph_ref().node_outputs(node).is_ok());
    r.s.set(ModeTarget::of(node).to(NodeMode::Bypassed))
        .expect("a node can be bypassed");
    for _ in 0..8 {
        let _ = r.channel(0);
    }
    r.change_ports(1);
    r.until("the Processor to reach the new node", |s| {
        s.graph_ref().node_outputs(node).is_ok_and(|n| n == 3)
            && s.graph_ref().testing().rebuild_settling(node) == Some(false)
    });
    assert_eq!(r.s.get(ModeTarget::of(node)).unwrap(), NodeMode::Bypassed);
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
        .max_block(MaxBlock::of(Samples::new(512)))
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    let (_, node) = s
        .load_from(
            tutti_fixtures::main_thread(),
            &catalog,
            &id(),
            Hosting::InProcess,
            None,
        )
        .unwrap();
    let mut side = s.graph().take_audio_side();
    let mut out = vec![vec![0.0; 512]; 2];
    // The plugin has its Processor in the graph before the re-prepare.
    let _ = s.update();
    side.render(512, PlanesMut::from_vecs(&mut out).unwrap());
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    assert!(s.graph_ref().testing().is_repreparing(), "mid re-prepare");
    probe().set(ClapSwitch::PortLayout, 1);
    probe().set(ClapSwitch::RequestPortsRescan, 0x24);
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while s.graph_ref().node_outputs(node).is_ok_and(|n| n != 3) {
        assert!(Instant::now() < deadline, "the wider node never came");
        side.render(512, PlanesMut::from_vecs(&mut out).unwrap());
        events.extend(s.update());
        thread::yield_now();
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
    r.s.remove(node).expect("the node is there");
    let events = r.until("the node to be gone", |s| s.plugin_view(node).is_err());
    assert!(
        !events.iter().any(|event| matches!(
            event,
            SessionEvent::Plugin(tutti_session::PluginEvent { node: n, .. })
                if *n == node.erase()
        )),
        "nothing is said of a removed node: {events:?}"
    );
    // The removed record waits in the graveyard for the main-thread update.
    // Then nothing keeps the plugin open.
    let _ = r.until("the plugin to close", |s| {
        let _ = s.update_on(tutti_fixtures::main_thread());
        deactivation().observed
    });
    let closed = deactivation();
    assert!(
        closed.observed && closed.is_main,
        "the plugin was closed, on the main thread: no rebinder kept it open: {closed:?}"
    );
    r.s.close(tutti_fixtures::main_thread());
}

/// The file whose appearance, holding a layout number, makes a running served
/// plugin take that layout and ask its host to rescan its ports.
fn port_change_file() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "plugin-in-process-port-change.{}",
        std::process::id()
    ))
}

/// Updates and renders a block at a time until `done` (given the session and the
/// last frame of the block) holds, failing after 30 s. Returns whether an update
/// reported a refused port change.
fn render_until(
    s: &mut Session,
    what: &str,
    done: &mut dyn FnMut(&Session, [f32; 2]) -> bool,
) -> bool {
    use tutti_session::{PluginChange, PluginEvent};
    let mut refused = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        refused |= s.update().iter().any(|event| {
            matches!(
                event,
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::PortsRefused { .. },
                    ..
                })
            )
        });
        let mut out = vec![0.0f32; 512 * 2];
        s.render(
            &mut tutti_buffers::InterleavedMut::new(&mut out, tutti_buffers::ChannelLayout::STEREO)
                .unwrap(),
        );
        if done(s, [out[out.len() - 2], out[out.len() - 1]]) {
            return refused;
        }
        assert!(Instant::now() < deadline, "{what} never happened");
        thread::yield_now();
    }
}

/// **A served plugin that grows its ports gets a wider node, at the same key,
/// and renders through it.** The stereo plugin announces a stereo and a mono
/// bus while it runs: the server tells the host, the lifecycle restarts the
/// plugin into a new slab, pipeline and node, the node declares three outputs,
/// nothing is refused, and the tags of its first two outputs still play.
fn a_served_plugin_that_grows_its_ports_gets_a_wider_node() {
    // The server inherits these: it renders each output channel's tag, and
    // changes its ports when the file appears.
    // Removed when the case ends, so the next case probes with none set.
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_CLAP_PROBE_RENDER_MODE", "2");
    env.set("TUTTI_CLAP_PROBE_PORT_CHANGE_FILE", port_change_file());
    env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let (catalog, _dir) = catalog();
    let mut s = session();
    let (_, node) = s
        .load_from(
            tutti_fixtures::main_thread(),
            &catalog,
            &id(),
            Hosting::OutOfProcess,
            None,
        )
        .unwrap();
    for (from, to) in [
        (node.output(0), tutti_session::MainOutput::channel(0)),
        (node.output(1), tutti_session::MainOutput::channel(1)),
    ] {
        s.graph().connect(from, to).unwrap().unfaded();
    }
    let mut refused = false;
    refused |= render_until(&mut s, "the stereo tags", &mut |_, t| t == [1.0, 2.0]);
    assert_eq!(s.graph_ref().node_outputs(node).unwrap(), 2);
    std::fs::write(port_change_file(), "1").expect("the change is asked");
    refused |= render_until(&mut s, "the wider node", &mut |s, _| {
        s.graph_ref().node_outputs(node).is_ok_and(|n| n == 3)
    });
    refused |= render_until(&mut s, "the tags through the new node", &mut |s, t| {
        t == [1.0, 2.0] && s.graph_ref().node_outputs(node).is_ok_and(|n| n == 3)
    });
    // Another change, to two mono buses: every layer (slab, pipeline, node)
    // follows the later layout, so the second output plays the second bus's tag.
    std::fs::write(port_change_file(), "3").expect("the change is asked");
    refused |= render_until(&mut s, "the narrower node", &mut |s, _| {
        s.graph_ref().node_outputs(node).is_ok_and(|n| n == 2)
    });
    refused |= render_until(&mut s, "the tags of two mono buses", &mut |s, t| {
        t == [1.0, 1001.0] && s.graph_ref().node_outputs(node).is_ok_and(|n| n == 2)
    });
    assert!(!refused, "a change the slab can follow is not refused");
    s.close(tutti_fixtures::main_thread());
}

/// **A served plugin inserted with `Graph::add_plugin`, no load
/// behind it, that grows its ports gets a wider node at the same key and
/// renders through it**: the session ticks the node's rebinder, so the
/// plugin restarts into a new slab and the node follows. The tags of its first
/// two outputs play before and after.
fn an_inserted_served_plugin_that_grows_its_ports_gets_a_wider_node() {
    // Removed when the case ends, so the next case probes with none set.
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    env.set("TUTTI_CLAP_PROBE_RENDER_MODE", "2");
    env.set("TUTTI_CLAP_PROBE_PORT_CHANGE_FILE", port_change_file());
    let mut s = session();
    let client = tutti_plugin::PluginClient::new(
        tutti_plugin_ipc::IpcConfig::at(
            tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source"),
        ),
        tutti_fixtures::executor(),
        &tutti_fixtures::detached_work(),
        &tutti_plugin_db::Loadable::uncatalogued(tutti_plugin_types::PluginRef::file(clap_probe())),
        SampleRate::SR_48K,
    )
    .expect("the reference plugin loads");
    let node = s.graph().add_plugin(Box::new(client));
    for (from, to) in [
        (node.output(0), tutti_session::MainOutput::channel(0)),
        (node.output(1), tutti_session::MainOutput::channel(1)),
    ] {
        s.graph().connect(from, to).unwrap().unfaded();
    }
    let mut refused = render_until(&mut s, "the stereo tags", &mut |_, t| t == [1.0, 2.0]);
    assert_eq!(s.graph_ref().node_outputs(node).unwrap(), 2);
    std::fs::write(port_change_file(), "1").expect("the change is asked");
    refused |= render_until(&mut s, "the wider node", &mut |s, _| {
        s.graph_ref().node_outputs(node).is_ok_and(|n| n == 3)
    });
    refused |= render_until(&mut s, "the tags through the new node", &mut |s, t| {
        t == [1.0, 2.0] && s.graph_ref().node_outputs(node).is_ok_and(|n| n == 3)
    });
    assert!(!refused, "a change the slab can follow is not refused");
    s.close(tutti_fixtures::main_thread());
}

pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["plugin_in_process_main";
a_swap_replaces_an_in_process_nodes_plugin,
a_served_plugin_swapped_over_an_in_process_one_is_reloaded_when_it_dies,
an_in_process_swap_to_other_ports_is_refused,
a_restore_applies_the_saved_state_to_an_in_process_plugin,
a_restore_applies_the_saved_state_to_a_served_plugin,
a_plugin_that_widens_its_ports_is_re_inserted_wider,
a_plugin_that_narrows_its_ports_reports_the_wiring_it_lost,
a_plugin_that_grows_wider_than_a_block_is_refused_by_name,
a_muted_node_stays_muted_through_a_port_change,
a_bypassed_plugin_node_is_re_inserted_and_takes_its_processor,
a_port_change_during_a_sample_rate_change_ends_with_the_new_ports,
a_node_removed_while_its_rebuild_settles_leaves_nothing_behind,
a_served_plugin_that_grows_its_ports_gets_a_wider_node,
an_inserted_served_plugin_that_grows_its_ports_gets_a_wider_node,
];
