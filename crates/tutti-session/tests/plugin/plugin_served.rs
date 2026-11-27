//! A served plugin through the session: the reference CLAP bundle (two plugins)
//! in a real `plugin-server`, resolved through a catalog that scanned it: its
//! death, its recovery, its state and the swap of the plugin at a node. The
//! load itself is `tests/plugin/plugin_load.rs`, and the in-process half is
//! `tests/main_thread/plugin_in_process_main.rs`.
//!
//! Build the server and the probe first: `cargo build -p tutti-plugin-server
//! -p tutti-plugin-probe`.

use std::time::Duration;

use tutti_buffers::PlanesMut;
use tutti_platform::Instant;

use crate::loading::Perform as _;
use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_graph::{CommitError, Cx, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_plugin_db::Catalog;
use tutti_plugin_types::StateError;
use tutti_plugin_types::StateUse;
use tutti_plugin_types::{PluginFailure, TerminationReason};
use tutti_plugin_types::{PluginId, PluginRef};
use tutti_session::{
    DetachedSession, LoadPreset, LoadedPlugin, OtherPeer, Peer, PeerState, PluginCause,
    PluginHealth, PluginInstance, Presence, RecoveryPolicy, ReplaceRefused, Resource, RestoreState,
    SaveState, Session, SessionEvent, SetRateError, StateStartError, SwapPluginError,
};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{SampleRate, Samples, Tail};

const BLOCK: usize = 512;

use crate::loading::{Block as _, Hosted as _};
use crate::support::clap_probe;

fn first_id() -> PluginId {
    PluginId::Clap(
        tutti_clap_test_plugin::PLUGIN_ID
            .to_str()
            .expect("utf-8")
            .into(),
    )
}

fn second_id() -> PluginId {
    PluginId::Clap(
        tutti_clap_test_plugin::SECOND_PLUGIN_ID
            .to_str()
            .expect("utf-8")
            .into(),
    )
}

/// A catalog that scanned the reference bundle (both of its plugins). It is
/// probed first thing in a test, before the plugin's variables are set.
fn catalog() -> (Catalog, tempfile::TempDir) {
    let (catalog, _id, dir) = crate::loading::catalog_of(&PluginRef::file(clap_probe()));
    (catalog, dir)
}

/// The file that holds the port layout the plugin's next server reports.
fn layout_file() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("plugin-served-layout.{}", std::process::id()))
}

/// Sets what the plugin's servers read from the environment: the server's path
/// and the file of the port layout. Once, when the first session is built (the
/// test's catalog is probed before that, with none of the plugin's variables
/// set, and its writer is joined): the only threads then are Rust's, which read
/// the environment through `std::env`, and no plugin code runs in this process,
/// since the plugin is served. Later changes are of the file, not the
/// environment.
fn servers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        layout("0");
        tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        tutti_fixtures::set_env("TUTTI_CLAP_PROBE_PORT_LAYOUT_FILE", layout_file());
    });
}

/// What the plugin's next server reports as its port layout: the probe's
/// `PortLayoutMode` as a number (`0` stereo, `1` stereo and an aux bus, `3` two
/// mono buses).
///
/// The file is replaced whole (a temporary beside it, then a rename), so a
/// server that starts meanwhile reads the old layout or the new one, never a
/// truncated file.
fn layout(mode: &str) {
    let file = layout_file();
    let mut temporary = file.clone().into_os_string();
    temporary.push(".new");
    let temporary = std::path::PathBuf::from(temporary);
    std::fs::write(&temporary, mode).expect("the layout is written");
    std::fs::rename(&temporary, &file).expect("the layout file is replaced");
}

/// An offline session with `recovery` as its policy (`None`: recovery off).
fn session_with(recovery: Option<RecoveryPolicy>) -> Session {
    servers();
    let builder = Session::builder().max_block(MaxBlock::of(Samples::new(BLOCK)));
    match recovery {
        Some(policy) => builder.plugin_recovery(policy),
        None => builder.no_plugin_recovery(),
    }
    .build()
    .expect("an offline session builds")
}

fn session(recovery: bool) -> Session {
    session_with(recovery.then(RecoveryPolicy::default))
}

/// One update and a block rendered: what moves a plugin's health.
fn pump(s: &mut Session) -> Vec<SessionEvent> {
    let events = s.update();
    let mut out = vec![0.0f32; BLOCK * 2];
    s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
    std::thread::yield_now();
    events
}

/// Updates (no render: the session has no output) until `done`.
fn settle_until<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &mut Session<K, C>,
    what: &str,
    mut done: impl FnMut(&mut Session<K, C>) -> bool,
) {
    tutti_fixtures::wait_until(what, tutti_fixtures::HANG_GUARD, || {
        if done(s) {
            return true;
        }
        let _ = s.update();
        false
    });
}

/// Pumps until `done`.
fn pump_until(s: &mut Session, what: &str, mut done: impl FnMut(&mut Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(s) {
        assert!(Instant::now() < deadline, "{what} never happened");
        let _ = pump(s);
    }
}

/// **The server's death with recovery off is `Lost` on the handle, and a new
/// plugin swaps in.** The node stays at its key, silent (recovery is off), and
/// the dead plugin is the node's, so it is not swapped in again; a new plugin
/// loaded and swapped in takes the stand-in's place and its own tail back.
#[test]
fn a_dead_server_with_recovery_off_is_lost_and_a_new_plugin_swaps_in() {
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    let (before, node) = in_a_node(&mut s, &catalog, &first_id());
    assert!(matches!(before.presence(), Presence::Live));

    let latency = s
        .graph_ref()
        .node_latency(node)
        .expect("the node is in the graph");
    let tail = s
        .graph_ref()
        .node_tail(node)
        .expect("the node is in the graph");
    assert!(
        matches!(tail, Tail::Finite(_)),
        "the live plugin declares a tail: {tail:?}"
    );
    let dead_pid = s
        .plugin_view(node)
        .unwrap()
        .diagnostics()
        .server_pid()
        .expect("served");
    s.plugin_view(node)
        .unwrap()
        .diagnostics()
        .terminate(TerminationReason::Hung {
            blocks: 16,
            held: Duration::from_secs(5),
        });
    pump_until(&mut s, "the death", |_| !before.presence().is_live());
    assert!(
        matches!(
            before.presence(),
            Presence::Lost(PluginCause::Died(PluginFailure::Terminated {
                reason: TerminationReason::Hung { .. }
            }))
        ),
        "the handle says how the plugin died"
    );
    // The node stays at its key, silent, in the plugin's place: its latency is
    // the plugin's last, and its plugin is released.
    pump_until(&mut s, "the death to be declared", |s| {
        matches!(
            s.plugin_view(node).map(|view| view.health()),
            Ok(PluginHealth::Died { .. })
        )
    });
    // The stand-in takes the plugin's place: it has no tail (the dead plugin's
    // node still declared one), which is what tells the swap was made.
    pump_until(&mut s, "the stand-in", |s| {
        s.graph_ref().node_tail(node) == Ok(Tail::None)
    });
    assert!(
        matches!(
            s.plugin_view(node).map(|view| view.health()),
            Ok(PluginHealth::Died { .. })
        ),
        "the node reads Died after the swap"
    );
    assert_eq!(
        s.graph_ref().node_latency(node),
        Ok(latency),
        "the node stays at its key, with the plugin's last latency"
    );
    assert!(
        matches!(before.presence(), Presence::Lost(PluginCause::Died(_))),
        "the node does not revive the plugin"
    );
    let main = tutti_fixtures::main_thread();

    // A new plugin loaded and swapped in takes the stand-in's place, and its
    // own tail back.
    let opened = load(&s, &catalog, &first_id());
    let after = opened.instance();
    assert!(matches!(after.presence(), Presence::Live));
    s.swap_plugin(node, opened, main)
        .expect("a live plugin swaps in at the node");
    pump_until(&mut s, "the node's new plugin", |s| {
        s.plugin_view(node)
            .is_ok_and(|view| view.diagnostics().server_pid() != Some(dead_pid))
    });
    assert_eq!(s.graph_ref().node_tail(node), Ok(tail));
    assert!(matches!(after.presence(), Presence::Live));
    assert!(
        matches!(before.presence(), Presence::Closed),
        "the dead plugin was replaced"
    );
}

/// **With recovery on, a dead server reads live while the session reloads it
/// in place**, and `Lost(GaveUp)` once recovery gives up.
#[test]
fn recovery_keeps_a_plugin_live_until_it_gives_up() {
    let (catalog, _dir) = catalog();
    let kill = |s: &Session, node| {
        s.plugin_view(node)
            .unwrap()
            .diagnostics()
            .terminate(TerminationReason::Hung {
                blocks: 16,
                held: Duration::from_secs(5),
            });
    };

    let mut s = session(true);
    let (handle, node) = in_a_node(&mut s, &catalog, &first_id());
    kill(&s, node);
    pump_until(&mut s, "the reload to start", |s| {
        matches!(
            s.plugin_view(node).map(|p| p.health()),
            Ok(PluginHealth::Reloading { .. })
        )
    });
    assert!(
        matches!(handle.presence(), Presence::Live),
        "recovering reads live"
    );

    let mut s = session_with(Some(RecoveryPolicy {
        max_failures: 0,
        ..RecoveryPolicy::default()
    }));
    let (handle, node) = in_a_node(&mut s, &catalog, &first_id());
    kill(&s, node);
    pump_until(&mut s, "the give-up", |_| !handle.presence().is_live());
    assert!(matches!(
        handle.presence(),
        Presence::Lost(PluginCause::GaveUp(_))
    ));
}

/// **A served plugin arrives with its parameters read.** The load future awaits the
/// read the server's replies feed, so the node's mirror is ready the moment the session
/// adds it, with no `update` between: an app never sees a loaded plugin whose
/// parameters are not read.
#[test]
fn a_served_plugin_arrives_with_its_parameters_read() {
    const CUTOFF: ParamId = ParamId::new(101);
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    let (_, node) = in_a_node(&mut s, &catalog, &first_id());
    let view = s.plugin_view(node).unwrap();
    assert!(view.params().is_ready(), "read before the first update");
    assert!(view.params().get(CUTOFF).is_ok(), "and declared");
}

/// **A reload composed from a node's info and state is awaited off the main
/// thread and swapped in on it.** The load of the handle's id and hosting, from
/// the state saved of the node, is `Send`: a spawned thread awaits it, and the
/// plugin swapped in at the node's key reads the saved value, not the default.
#[test]
fn a_reload_awaited_on_a_spawned_thread_is_swapped_in_with_its_state() {
    const CUTOFF: ParamId = ParamId::new(101);
    let main = tutti_fixtures::main_thread();
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    let (handle, node) = in_a_node(&mut s, &catalog, &first_id());
    let set = Normalized::of(0.9);
    let target = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(CUTOFF)
        .expect("the plugin declares the parameter")
        .target();
    s.set(target.to(set)).unwrap();
    let state = s
        .save_state(&node, StateUse::Project)
        .unwrap()
        .perform(main)
        .expect("a state");

    let info = handle.info();
    let mut load = match s
        .load_plugin_as(info.id(), &catalog, info.hosting())
        .unwrap()
    {
        tutti_session::LoadPlugin::OutOfProcess(load) => load,
        other => panic!("{other:?}"),
    };
    load.initial_state(tutti_session::Saved::new(state))
        .expect("its own plugin's state");
    let revived = std::thread::spawn(move || load.block())
        .join()
        .expect("the load's thread ends")
        .expect("the plugin loads");
    let old_pid = pid_of(&s, node);
    s.swap_plugin(node, revived, main).expect("it swaps in");
    pump_until(&mut s, "the node's new plugin", |s| {
        pid_of(s, node) != old_pid
            && s.plugin_view(node)
                .is_ok_and(|view| view.params().get(CUTOFF).is_ok())
    });
    let read = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(CUTOFF)
        .unwrap()
        .normalized();
    assert!(
        (read.get() - set.get()).abs() < 1e-6,
        "the state's value, not the default: {read:?}"
    );
}

/// **A state saved is the state of the plugin at the run, and the refusals are
/// named.** A value set on a served plugin and saved through `save_state` is the
/// value another load starts from; a node that is gone and a state of another
/// plugin are refused at the call, and nothing is sent to the plugin.
#[test]
fn a_saved_state_holds_what_the_plugin_holds_and_refuses_by_name() {
    const CUTOFF: ParamId = ParamId::new(101);
    let main = tutti_fixtures::main_thread();
    let (catalog, _dir) = catalog();
    let mut s = session(true);
    let (first, a) = in_a_node(&mut s, &catalog, &first_id());
    let (_, gone) = in_a_node(&mut s, &catalog, &first_id());
    s.remove(gone).unwrap();

    assert_eq!(
        s.save_state(&gone, StateUse::Project)
            .expect_err("the node is gone")
            .node(),
        gone.erase()
    );
    let foreign = {
        let entry = catalog
            .entries()
            .find(|entry| entry.id == Some(&second_id()))
            .expect("the second plugin is listed");
        PeerState::of_plugin(entry.plugin_ref(), Vec::new())
    };
    assert!(matches!(
        s.restore_state(&gone, tutti_session::Saved::new(foreign.clone())),
        Err(StateStartError::NoNode(_))
    ));
    assert!(matches!(
        s.restore_state(&a, tutti_session::Saved::new(foreign)),
        Err(StateStartError::OtherPeer(OtherPeer { .. }))
    ));

    let set = Normalized::of(0.9);
    let target = s
        .plugin_view(a)
        .unwrap()
        .params()
        .get(CUTOFF)
        .expect("the plugin declares the parameter")
        .target();

    s.set(target.to(set)).unwrap();
    let state = s
        .save_state(&a, StateUse::Project)
        .expect("a node holds the plugin")
        .perform(main)
        .expect("a state");
    assert_eq!(
        state.media_type().as_str(),
        "application/x-clap-state",
        "a saved CLAP state is typed as CLAP's"
    );

    // The probe stamps the state context after its 4-byte magic: 3 for a
    // project save, 2 for a duplicate.
    const CONTEXT_BYTE: usize = 4;
    assert_eq!(
        state.bytes()[CONTEXT_BYTE],
        3,
        "a project save is FOR_PROJECT"
    );
    let duplicate = s
        .save_state(&a, StateUse::Duplicate)
        .expect("a node holds the plugin")
        .perform(main)
        .expect("a duplicate state");
    assert_eq!(
        duplicate.bytes()[CONTEXT_BYTE],
        2,
        "the use reaches the plugin: a duplicate save is FOR_DUPLICATE"
    );

    let mut load = s.load_out_of_process(&first_id(), &catalog);
    load.initial_state(tutti_session::Saved::new(state))
        .expect("its own plugin's state");
    let second = load.block().expect("the plugin loads");
    let second_handle = second.instance();
    let b = s.apply(second).unwrap();
    let read = s
        .plugin_view(b)
        .unwrap()
        .params()
        .get(CUTOFF)
        .unwrap()
        .normalized();
    assert!(
        (read.get() - set.get()).abs() < 1e-6,
        "the saved value, not the default: {read:?}"
    );
    // Two loads of one id are two plugins, in two servers.
    let pid = |node| s.plugin_view(node).unwrap().diagnostics().server_pid();
    assert_ne!(pid(a), pid(b), "two plugins, two servers");
    assert_ne!(first.id(), second_handle.id());
}

/// **A restored state applied after crash recovery replaced the node's plugin is
/// refused `Stale`**: the server that took the state is gone, and the plugin
/// now at the node is a new instance.
#[test]
fn a_restore_applied_after_a_recovery_replaced_the_plugin_is_stale() {
    let (catalog, _dir) = catalog();
    let main = tutti_fixtures::main_thread();
    let mut s = session(true);
    let (_, node) = in_a_node(&mut s, &catalog, &first_id());
    let state = s
        .save_state(&node, StateUse::Project)
        .unwrap()
        .perform(main)
        .expect("a state");
    let restored = s
        .restore_state(&node, tutti_session::Saved::new(state))
        .unwrap()
        .perform(main)
        .expect("the plugin takes it");
    let before = pid_of(&s, node);
    s.plugin_view(node)
        .unwrap()
        .diagnostics()
        .terminate(TerminationReason::Hung {
            blocks: 16,
            held: Duration::from_secs(5),
        });
    pump_until(&mut s, "the recovered plugin", |s| {
        pid_of(s, node).is_some() && pid_of(s, node) != before
    });
    assert_eq!(
        s.apply(restored).unwrap_err(),
        tutti_session::PluginApplyError::Stale
    );
}

/// **A save of a plugin whose server was terminated ends as the crash, not a
/// state:** the run is `StateError::PluginCrashed`, and no empty state is saved
/// as the real thing.
#[test]
fn a_save_of_a_dead_plugin_is_the_plugins_refusal() {
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    let (_, node) = in_a_node(&mut s, &catalog, &first_id());
    s.plugin_view(node)
        .unwrap()
        .diagnostics()
        .terminate(TerminationReason::Hung {
            blocks: 16,
            held: Duration::from_secs(5),
        });
    let saved = s
        .save_state(&node, StateUse::Project)
        .expect("the node is still there")
        .perform(tutti_fixtures::main_thread());
    assert!(
        matches!(saved, Err(StateError::PluginCrashed)),
        "the plugin's refusal: {saved:?}"
    );
}

/// **A plugin with other ports is refused by a swap, named, and the node keeps
/// the plugin it has**: swapping would leave the node's edges on ports that are
/// gone and refuse every later commit, so the swap is not made and the refused
/// plugin stays loaded.
#[test]
fn a_swap_to_a_plugin_with_other_ports_is_refused() {
    let (catalog, _dir) = catalog();
    let mut s = session(true);
    let (_, node) = in_a_node(&mut s, &catalog, &first_id());
    let first = tutti_clap_test_plugin::PLUGIN_ID.to_str().unwrap();

    // The next server starts with the probe's aux-bus layout.
    layout("1");
    let other = load(&s, &catalog, &second_id());
    let handle = other.instance();
    let refused = s.swap_plugin(node, other, tutti_fixtures::main_thread());
    let Err(SwapPluginError::Ports { node: returned, .. }) = refused else {
        panic!("other ports are refused: {refused:?}");
    };
    assert_eq!(
        crate::loading::clap_id(s.plugin_view(node).unwrap().plugin()),
        first
    );
    assert!(
        matches!(handle.presence(), Presence::Live),
        "the refused plugin is still loaded"
    );
    let _ = pump(&mut s);
    assert!(
        s.update()
            .iter()
            .all(|event| !matches!(event, SessionEvent::CommitRefused(_))),
        "the graph still commits"
    );
    // The node comes back whole: it has a node of its own to add.
    let own = s.apply(returned).expect("the returned node adds");
    assert_ne!(own, node);
    assert!(matches!(handle.presence(), Presence::Live));

    // A refusal leaves the node able to take a plugin that fits.
    let before = pid_of(&s, node);
    layout("0");
    let fitting = load(&s, &catalog, &second_id());
    s.swap_plugin(node, fitting, tutti_fixtures::main_thread())
        .expect("a plugin that fits swaps in");
    assert_eq!(
        crate::loading::clap_id(s.plugin_view(node).unwrap().plugin()),
        tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap()
    );
    assert_ne!(pid_of(&s, node), before, "in a server of its own");
}

/// **A swap replaces the plugin at the node, at the same key.** The node is the
/// second plugin afterwards, in a server of its own.
#[test]
fn a_swap_replaces_the_plugin_at_the_node() {
    let (catalog, _dir) = catalog();
    let mut s = session(true);
    let (outgoing, node) = in_a_node(&mut s, &catalog, &first_id());
    assert!(matches!(outgoing.presence(), Presence::Live));
    let id_of_node =
        |s: &Session| crate::loading::clap_id(s.plugin_view(node).unwrap().plugin()).to_owned();
    assert_eq!(
        id_of_node(&s),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().unwrap()
    );
    let first_pid = pid_of(&s, node);

    let second = load(&s, &catalog, &second_id());
    let incoming = second.instance();
    s.swap_plugin(node, second, tutti_fixtures::main_thread())
        .expect("a plugin with the node's ports swaps in");

    let want = tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap();
    assert_eq!(id_of_node(&s), want, "the node is the second plugin");
    assert_ne!(pid_of(&s, node), first_pid, "in a server of its own");
    assert!(matches!(incoming.presence(), Presence::Live));
    assert!(
        matches!(outgoing.presence(), Presence::Closed),
        "the swapped-out plugin's handle has no plugin behind it"
    );
}

/// A node that cannot run above 90 kHz: its `prepare` panics there, which
/// poisons the graph's editor.
struct Grenade;

impl Node for Grenade {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, prepare: &Prepare) {
        assert!(prepare.sample_rate().get() < 90_000.0, "no such rate");
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A session with no output, whose audio side a test can take (a re-prepare
/// then waits for it).
fn detached() -> DetachedSession {
    detached_with(Some(RecoveryPolicy::default()))
}

/// [`detached`] with `recovery` as its policy (`None`: recovery off).
fn detached_with(recovery: Option<RecoveryPolicy>) -> DetachedSession {
    servers();
    let builder = Session::builder().max_block(MaxBlock::of(Samples::new(BLOCK)));
    match recovery {
        Some(policy) => builder.plugin_recovery(policy),
        None => builder.no_plugin_recovery(),
    }
    .build_detached(SampleRate::SR_48K)
    .expect("a detached session builds")
}

/// One session with the first plugin loaded and in a node.
fn with_node(catalog: &Catalog) -> (Session, tutti_session::Key<tutti_session::AnyPlugin>) {
    with_node_in(session(true), catalog)
}

fn with_node_in<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    mut s: Session<K, C>,
    catalog: &Catalog,
) -> (Session<K, C>, tutti_session::Key<tutti_session::AnyPlugin>) {
    let (_, node) = in_a_node(&mut s, catalog, &first_id());
    (s, node)
}

/// `id` loaded on the main thread: the node, not yet added.
fn load<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &Session<K, C>,
    catalog: &Catalog,
    id: &PluginId,
) -> LoadedPlugin {
    s.load_out_of_process(id, catalog)
        .block()
        .expect("the plugin loads")
}

/// `id` loaded and in a node: the handle, and the node's key.
fn in_a_node<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &mut Session<K, C>,
    catalog: &Catalog,
    id: &PluginId,
) -> (PluginInstance, tutti_session::Key<tutti_session::AnyPlugin>) {
    let opened = load(s, catalog, id);
    let plugin = opened.instance();
    let node = s.apply(opened).expect("the plugin enters");
    (plugin, node)
}

fn pid_of<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &Session<K, C>,
    node: tutti_session::Key<tutti_session::AnyPlugin>,
) -> Option<u32> {
    s.plugin_view(node).unwrap().diagnostics().server_pid()
}

/// **Equal channel totals with another split of buses are other ports**: two
/// mono buses where the node has one stereo bus. The totals match; the buses
/// the node's edges name do not.
#[test]
fn a_swap_to_the_same_totals_in_other_buses_is_refused() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node(&catalog);
    layout("3");
    let other = load(&s, &catalog, &second_id());
    let refused = s.swap_plugin(node, other, tutti_fixtures::main_thread());
    let Err(SwapPluginError::Ports { got, was, .. }) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(
        (got.inputs.clone(), was.inputs.clone()),
        (
            vec![ChannelLayout::MONO, ChannelLayout::MONO],
            vec![ChannelLayout::STEREO]
        )
    );
    assert_eq!(
        crate::loading::clap_id(s.plugin_view(node).unwrap().plugin()),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().unwrap()
    );
}

/// What a graph that re-prepares does to a swap: it waits, with the executor
/// taken, until the audio side has run and an update has collected.
fn repreparing<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &mut Session<K, C>,
) -> tutti_session::AudioSide {
    let side = s.graph().take_audio_side();
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    assert!(s.graph_ref().testing().is_repreparing(), "mid re-prepare");
    side
}

/// **A swap is `Busy` while the graph is re-preparing, and the plugin it
/// refused swaps in once the graph resumes.** The node keeps its plugin
/// meanwhile.
#[test]
fn a_swap_is_busy_while_the_graph_re_prepares_and_succeeds_after() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node_in(detached(), &catalog);
    let main = tutti_fixtures::main_thread();
    let first = pid_of(&s, node);
    let mut side = repreparing(&mut s);

    let second = load(&s, &catalog, &second_id());
    let Err(SwapPluginError::Busy(second)) = s.swap_plugin(node, second, main) else {
        panic!("a re-preparing graph is busy");
    };
    assert_eq!(
        pid_of(&s, node),
        first,
        "the graph is re-preparing: the node keeps its plugin"
    );

    let mut out = vec![vec![0.0; BLOCK]; 2];
    settle_until(&mut s, "the re-prepare to resume", |s| {
        side.render(BLOCK, PlanesMut::from_vecs(&mut out).unwrap());
        !s.graph_ref().testing().is_repreparing()
    });
    s.swap_plugin(node, second, main)
        .expect("the node the busy graph refused swaps in once it resumes");
    assert_eq!(
        crate::loading::clap_id(s.plugin_view(node).unwrap().plugin()),
        tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap(),
    );
    assert_ne!(pid_of(&s, node), first);
}

/// **A swap into a node that is gone is refused by name**, and the node it
/// would have swapped in is handed back for a next node.
#[test]
fn a_swap_into_a_removed_node_is_refused_and_hands_the_node_back() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node(&catalog);
    let main = tutti_fixtures::main_thread();
    let second = load(&s, &catalog, &second_id());

    s.remove(node).unwrap();
    let Err(SwapPluginError::NoSuchNode(_, second)) = s.swap_plugin(node, second, main) else {
        panic!("a removed node is refused by name");
    };
    let next = s
        .apply(second)
        .expect("the refused plugin is the next node's");
    assert_eq!(
        crate::loading::clap_id(s.plugin_view(next).unwrap().plugin()),
        tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap()
    );
}

/// **A graph that refuses the swap fails it with the refusal**: a poisoned
/// graph is `Graph(Poisoned)`, and the node keeps its plugin.
#[test]
fn a_graph_that_refuses_the_swap_is_named_and_the_node_keeps_its_plugin() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node(&catalog);
    s.graph().add(Unforkable(Grenade));
    // The session's executor is local, so both halves of the re-prepare run
    // here and the grenade's panic has poisoned the editor when this returns,
    // with the poisoning as the refusal.
    let poisoned = s.graph().set_sample_rate(SampleRate::SR_96K);
    assert!(
        matches!(
            poisoned,
            Err(SetRateError::Commit(CommitError::Poisoned { .. }))
        ),
        "{poisoned:?}"
    );
    let second = load(&s, &catalog, &second_id());
    let handle = second.instance();
    let refused = s.swap_plugin(node, second, tutti_fixtures::main_thread());
    let Err(SwapPluginError::Graph(refused)) = refused else {
        panic!("{refused:?}");
    };
    assert!(
        matches!(*refused, ReplaceRefused::Poisoned(_)),
        "{refused:?}"
    );
    assert_eq!(
        crate::loading::clap_id(s.plugin_view(node).unwrap().plugin()),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().unwrap()
    );
    assert!(
        matches!(handle.presence(), Presence::Closed),
        "the refused plugin was consumed"
    );
}

/// **A plugin that dies while the graph re-prepares is silenced at the next
/// phase that can**: the dead plugin's node keeps its tail until the swap is
/// made (the graph refuses it mid re-prepare and asks again), and has none
/// once the re-prepare resumes.
#[test]
fn a_plugin_that_dies_during_a_re_prepare_is_silenced_when_it_resumes() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node_in(detached_with(None), &catalog);
    let tail = s
        .graph_ref()
        .node_tail(node)
        .expect("the node is in the graph");
    assert!(matches!(tail, Tail::Finite(_)), "{tail:?}");
    let mut side = repreparing(&mut s);

    s.plugin_view(node)
        .unwrap()
        .diagnostics()
        .terminate(TerminationReason::Hung {
            blocks: 16,
            held: Duration::from_secs(5),
        });
    settle_until(&mut s, "the death to be declared", |s| {
        matches!(
            s.plugin_view(node).map(|view| view.health()),
            Ok(PluginHealth::Died { .. })
        )
    });
    for _ in 0..5 {
        let _ = s.update();
    }
    assert_eq!(
        s.graph_ref().node_tail(node),
        Ok(tail),
        "the graph is re-preparing: the swap waits"
    );
    assert!(s.graph_ref().contains(node));

    let mut out = vec![vec![0.0; BLOCK]; 2];
    settle_until(&mut s, "the stand-in", |s| {
        side.render(BLOCK, PlanesMut::from_vecs(&mut out).unwrap());
        s.graph_ref().node_tail(node) == Ok(Tail::None)
    });
}

/// The case that names the server by its process id.
#[cfg(target_os = "linux")]
mod process {
    use super::*;

    /// **A served plugin's record is not parked:** removing the node drops it
    /// at once, with nothing in the graveyard, and the server ends under
    /// `Session::update` alone, which has no main thread to empty a yard.
    #[test]
    fn a_removed_served_plugin_is_dropped_with_no_graveyard_entry() {
        let (catalog, _dir) = catalog();
        let mut s = session(true);
        let (_, node) = in_a_node(&mut s, &catalog, &first_id());
        let pid = s
            .plugin_view(node)
            .unwrap()
            .diagnostics()
            .server_pid()
            .expect("a served plugin has its server");
        let alive = || std::path::Path::new(&format!("/proc/{pid}")).exists();
        assert!(alive());

        s.remove(node).unwrap();
        assert_eq!(s.retired_plugins(), 0, "nothing is parked");
        pump_until(&mut s, "the server's end", |s| {
            let _ = s.update();
            !alive()
        });
    }
}

/// **A reloaded plugin's node is cut to the same input bound, and reports it.**
/// A block of 300 notes is cut by the graph to the node's declared input bound
/// (`MIDI_STACK_BOUND`): the rest is reported as `EventsDropped` at the node, and
/// only that. The server is terminated and recovery reloads the plugin at the same
/// key; the reload reports no cut of its own, and a second block of 300 is cut by
/// the same bound. The cut is the graph's, not the plugin's, so `losses()` stays
/// empty throughout: each chunk is answered before the next thing happens to its
/// server, since a chunk the server is killed under, or falls behind on, is a real
/// loss of its 256 events and is counted as one.
#[test]
fn a_reloaded_plugins_node_is_cut_to_the_same_input_bound() {
    use tutti_fixtures::midi;
    use tutti_graph::EventCapacity;
    use tutti_session::{MainOutput, Stamp};
    use tutti_ump::MidiChannel;
    use tutti_units::Losses;

    let cut = 300 - u64::from(tutti_plugin_types::MIDI_STACK_BOUND.get());
    let dropped_at = |events: Vec<SessionEvent>, node: tutti_graph::NodeKey| -> u64 {
        events
            .into_iter()
            .filter_map(|e| match e {
                SessionEvent::EventsDropped {
                    node: n,
                    port: None,
                    count,
                    ..
                } if n == node => Some(count),
                _ => None,
            })
            .sum()
    };
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node(&catalog);
    s.graph().connect(node, MainOutput).unwrap();
    let (inlet, mut writer) = s.graph().open_event_inlet(EventCapacity::of(512));
    s.graph()
        .connect(inlet.event_output(0), node.event_input(0))
        .unwrap();
    let notes = || (0..300).map(|i| midi::note_on(MidiChannel::FIRST, (i % 128) as u8, 0xFFFF));
    let losses = |s: &Session| s.plugin_view(node).unwrap().losses();
    let answered = |s: &Session| {
        tutti_fixtures::wait_until(
            "the chunk in flight to be answered",
            tutti_fixtures::HANG_GUARD,
            || {
                s.plugin_view(node)
                    .unwrap()
                    .diagnostics()
                    .chunk_in_flight_answered()
            },
        );
    };
    let _ = pump(&mut s);
    assert_eq!(losses(&s), Losses::NONE, "nothing sent yet");

    // The cut is charged in the block that rendered it, reported by the next update.
    writer.push_all(notes().map(|n| (Stamp::NextBlock, n)));
    let _ = pump(&mut s);
    answered(&s);
    assert_eq!(dropped_at(pump(&mut s), node.into()), cut);
    assert_eq!(
        losses(&s),
        Losses::NONE,
        "the graph's cut, not the plugin's"
    );

    // The server has answered every chunk it was sent, so its death takes
    // none of the node's events with it.
    answered(&s);
    let first = pid_of(&s, node).expect("served");
    s.plugin_view(node)
        .unwrap()
        .diagnostics()
        .terminate(TerminationReason::Hung {
            blocks: 16,
            held: Duration::from_secs(5),
        });
    pump_until(&mut s, "the reload to land", |s| {
        pid_of(s, node).is_some_and(|pid| pid != first)
            && matches!(s.plugin_view(node).unwrap().health(), PluginHealth::Running)
    });
    assert_eq!(
        dropped_at(pump(&mut s), node.into()),
        0,
        "the reload reports no cut of its own"
    );

    answered(&s);
    writer.push_all(notes().map(|n| (Stamp::NextBlock, n)));
    let _ = pump(&mut s);
    answered(&s);
    assert_eq!(
        dropped_at(pump(&mut s), node.into()),
        cut,
        "the reloaded plugin's node is cut to the same bound"
    );
    answered(&s);
    assert_eq!(losses(&s), Losses::NONE);
}

fn block_on_save(save: SaveState) -> PeerState {
    let SaveState::OutOfProcess(save) = save else {
        panic!("a served plugin's save is awaited");
    };
    tutti_fixtures::block_on(save)
        .expect("a state")
        .into_state()
}

/// **A served save after a write no block has applied states the write in
/// `pending`, and a restore of a save with writes pending replays them through
/// the session's ledger once the state has loaded.** No block runs for the
/// first save (the node has no output), so the write is the host's alone; the
/// restore is of a state that does not hold a value and names it pending, and
/// the plugin holds that value once a block has run.
#[test]
fn a_served_save_states_a_pending_write_and_a_restore_replays_it() {
    const CUTOFF: ParamId = ParamId::new(101);
    let own = |saved: &tutti_session::Saved| {
        tutti_clap_test_plugin::saved_normalized(saved.state().bytes(), CUTOFF.get())
            .expect("the state holds the parameter")
    };
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    let (_, node) = in_a_node(&mut s, &catalog, &first_id());
    let save = |s: &Session| {
        let SaveState::OutOfProcess(save) = s.save_state(&node, StateUse::Project).unwrap() else {
            panic!("a served plugin's save is awaited");
        };
        tutti_fixtures::block_on(save).expect("a state")
    };
    let target = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(CUTOFF)
        .expect("the plugin declares the parameter")
        .target();

    assert!(save(&s).pending().is_empty(), "nothing was written");
    s.set(target.to(Normalized::of(0.9))).unwrap();
    assert_eq!(
        save(&s).pending(),
        [(CUTOFF, Normalized::of(0.9))],
        "no block has applied the write"
    );

    // A state that holds the plugin's own value, with another write pending.
    s.graph().connect(node, tutti_session::MainOutput).unwrap();
    for _ in 0..4 {
        let _ = pump(&mut s);
    }
    let held = save(&s);
    assert!((own(&held) - 0.9).abs() < 1e-4, "blocks applied the write");
    assert!(held.pending().is_empty(), "so nothing is pending");
    let stated =
        tutti_session::Saved::with_pending(held.into_state(), vec![(CUTOFF, Normalized::of(0.2))]);
    let RestoreState::OutOfProcess(restore) = s.restore_state(&node, stated).unwrap() else {
        panic!("a served plugin's restore is awaited");
    };
    let restored = tutti_fixtures::block_on(restore).expect("the plugin takes the state");
    s.apply(restored).expect("the node hosts the plugin");
    pump_until(&mut s, "the replayed write reaches the plugin", |s| {
        for _ in 0..2 {
            let _ = pump(s);
        }
        (own(&save(s)) - 0.2).abs() < 1e-4
    });
}

/// **A save awaited on a spawned thread returns the plugin's state**, which a
/// new plugin loaded from it holds: no main thread ran the round trip.
#[test]
fn a_save_awaited_on_a_spawned_thread_returns_the_plugins_state() {
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    let (_, a) = in_a_node(&mut s, &catalog, &first_id());
    let set = Normalized::of(0.9);
    let target = s
        .plugin_view(a)
        .unwrap()
        .params()
        .get(ParamId::new(101))
        .expect("the plugin declares Cutoff")
        .target();
    s.set(target.to(set)).unwrap();

    let SaveState::OutOfProcess(save) = s.save_state(&a, StateUse::Project).unwrap() else {
        panic!("a served plugin's save is awaited");
    };
    let state = std::thread::spawn(move || tutti_fixtures::block_on(save))
        .join()
        .expect("the awaiting thread ends")
        .expect("a state");

    let mut load = s.load_out_of_process(&first_id(), &catalog);
    load.initial_state(state).expect("its own plugin's state");
    let b = s.apply(load.block().expect("the plugin loads")).unwrap();
    let read = s
        .plugin_view(b)
        .unwrap()
        .params()
        .get(ParamId::new(101))
        .unwrap()
        .normalized();
    assert!(
        (read.get() - set.get()).abs() < 1e-6,
        "the saved value, not the default: {read:?}"
    );

    let preset = s.load_preset_file(&a, "/x.tutti-preset").unwrap();
    assert!(
        matches!(preset, LoadPreset::OutOfProcess(_)),
        "a served plugin's preset load is awaited: {preset:?}"
    );
}

/// **A restore awaited off the main thread has its effect once the main thread
/// applies it**: the plugin holds 0.3 when its state is saved, is moved to 0.9,
/// and the saved state goes back from a spawned thread: the mirror reads 0.3
/// again after `apply`.
#[test]
fn a_restore_awaited_off_main_takes_effect_when_applied() {
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    let (_, node) = in_a_node(&mut s, &catalog, &first_id());
    // Rendered, so a host write reaches the plugin with a block.
    s.graph().connect(node, tutti_session::MainOutput).unwrap();
    let cutoff = |s: &Session| {
        s.plugin_view(node)
            .unwrap()
            .params()
            .get(ParamId::new(101))
            .unwrap()
            .normalized()
            .get()
    };
    let target = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(ParamId::new(101))
        .unwrap()
        .target();
    s.set(target.to(Normalized::of(0.3))).unwrap();
    let saved = block_on_save(s.save_state(&node, StateUse::Project).unwrap());
    s.set(target.to(Normalized::of(0.9))).unwrap();
    // The write reaches the plugin with a block: a state saved now holds it.
    for _ in 0..4 {
        let _ = pump(&mut s);
    }
    let held = block_on_save(s.save_state(&node, StateUse::Project).unwrap());
    assert_ne!(held.bytes(), saved.bytes(), "the plugin holds 0.9, not 0.3");

    let RestoreState::OutOfProcess(restore) = s
        .restore_state(&node, tutti_session::Saved::new(saved))
        .unwrap()
    else {
        panic!("a served plugin's restore is awaited");
    };
    let restored = std::thread::spawn(move || tutti_fixtures::block_on(restore))
        .join()
        .expect("the awaiting thread ends")
        .expect("the plugin takes the state");
    s.apply(restored).expect("the node hosts the plugin");
    pump_until(&mut s, "the mirror reads the restored value", |s| {
        (cutoff(s) - 0.3).abs() < 1e-6
    });
}

/// **A restore or a save dropped before its first poll asks the plugin
/// nothing**, and no `RestoredState` exists for it: the plugin keeps its value.
#[test]
fn a_restore_dropped_before_it_is_awaited_sends_nothing() {
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    let (_, node) = in_a_node(&mut s, &catalog, &first_id());
    let default = block_on_save(s.save_state(&node, StateUse::Project).unwrap());
    let target = s
        .plugin_view(node)
        .unwrap()
        .params()
        .get(ParamId::new(101))
        .unwrap()
        .target();
    s.set(target.to(Normalized::of(0.9))).unwrap();
    drop(
        s.restore_state(&node, tutti_session::Saved::new(default))
            .unwrap(),
    );
    let after = block_on_save(s.save_state(&node, StateUse::Project).unwrap());
    let mut loaded = s.load_out_of_process(&first_id(), &catalog);
    loaded
        .initial_state(tutti_session::Saved::new(after))
        .unwrap();
    let c = s.apply(loaded.block().unwrap()).unwrap();
    let held = s
        .plugin_view(c)
        .unwrap()
        .params()
        .get(ParamId::new(101))
        .unwrap()
        .normalized();
    assert!(
        (held.get() - 0.9).abs() < 1e-6,
        "a dropped restore never reached the plugin: {held:?}"
    );
}

/// **A server that dies in a restore drops the sender, and the awaited restore
/// ends as the dead plugin**, never a hang: the lane ends with the request
/// unanswered.
#[test]
fn a_restore_the_server_dies_in_ends_as_the_dead_plugin() {
    let (catalog, _dir) = catalog();
    tutti_fixtures::set_env("TUTTI_CLAP_PROBE_CRASH_ON_STATE_LOAD", "1");
    let mut s = session(false);
    let (_, node) = in_a_node(&mut s, &catalog, &first_id());
    let state = block_on_save(s.save_state(&node, StateUse::Project).unwrap());
    let RestoreState::OutOfProcess(restore) = s
        .restore_state(&node, tutti_session::Saved::new(state))
        .unwrap()
    else {
        panic!("a served plugin's restore is awaited");
    };
    let ended = std::thread::spawn(move || tutti_fixtures::block_on(restore))
        .join()
        .expect("the awaiting thread ends");
    assert!(
        matches!(ended, Err(StateError::PluginCrashed)),
        "the plugin died in the restore: {:?}",
        ended.map(|_| ())
    );
}
