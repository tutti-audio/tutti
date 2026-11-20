//! A plugin as a pool entry, served: the reference CLAP bundle (two plugins) in
//! a real `plugin-server`, resolved through a catalog that scanned it. The
//! in-process half is `tests/main_thread/plugin_pool_main.rs`.
//!
//! Build the server and the probe first: `cargo build -p tutti-plugin-server
//! -p tutti-plugin-probe`.

use std::time::Duration;

use tutti_buffers::PlanesMut;
use tutti_platform::Instant;

use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_plugin_db::{Catalog, Hosting};
use tutti_plugin_types::StateError;
use tutti_plugin_types::StateUse;
use tutti_plugin_types::{PluginFailure, TerminationReason};
use tutti_plugin_types::{PluginId, PluginRef};
use tutti_session::{
    AddPluginError, CrossfadeRefused, DetachedSession, LoadError, OtherPeer, Peer, PeerState,
    PluginCause, PluginHealth, PoolEvent, PoolState, Presence, RecoveryPolicy, Resource,
    RetryError, Session, SessionEvent, StateStartError,
};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{SampleRate, Samples, Tail};

const BLOCK: usize = 512;

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
    let (catalog, _id, dir) = crate::pooled::catalog_of(&PluginRef::file(clap_probe()));
    (catalog, dir)
}

/// The file that holds the port layout the plugin's next server reports.
fn layout_file() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("plugin-pool-layout.{}", std::process::id()))
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
        // SAFETY: nextest runs each test in its own process; see above.
        unsafe {
            std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
            std::env::set_var("TUTTI_CLAP_PROBE_PORT_LAYOUT_FILE", layout_file());
        }
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

/// One update, a block rendered and a short wait: what moves a plugin's health.
fn pump(s: &mut Session) -> Vec<SessionEvent> {
    let events = s.update();
    let mut out = vec![0.0f32; BLOCK * 2];
    s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
    std::thread::sleep(Duration::from_millis(5));
    events
}

/// Updates (no render: the session has no output) until `done`.
fn settle_until<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &mut Session<K, C>,
    what: &str,
    mut done: impl FnMut(&mut Session<K, C>) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(s) {
        assert!(Instant::now() < deadline, "{what} never happened");
        let _ = s.update();
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Pumps until `done`.
fn pump_until(s: &mut Session, what: &str, mut done: impl FnMut(&mut Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(s) {
        assert!(Instant::now() < deadline, "{what} never happened");
        let _ = pump(s);
    }
}

/// Updates until `done` finds what it waits for in an update's pool events.
fn until<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &mut Session<K, C>,
    what: &str,
    mut done: impl FnMut(&&'static str, &PoolEvent) -> bool,
) -> PoolEvent {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        for event in s.update() {
            if let SessionEvent::Pool { key, event } = event
                && let Some(key) = key.get::<&'static str>()
                && done(key, &event)
            {
                return event;
            }
        }
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The key's pool events of the first update that has any.
fn until_all<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &mut Session<K, C>,
    what: &str,
    key: &'static str,
) -> Vec<PoolEvent> {
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
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn loaded<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &mut Session<K, C>,
    key: &'static str,
) {
    let event = until(s, "the load", |k, _| *k == key);
    assert!(matches!(event, PoolEvent::Loaded), "{event:?}");
}

/// **A served plugin loads through the pool.** The entry reads loading, lands
/// as `PoolEvent::Loaded` with no main thread involved, and its handle reads
/// the plugin the id names, served, live. The node over it is the plugin in a
/// server of its own, and one entry has one node.
#[test]
fn a_served_plugin_lands_through_the_pool() {
    let (catalog, _dir) = catalog();
    let mut s = session(true);
    s.pool(s.plugin_entry("verb", &second_id(), &catalog))
        .expect("a served load needs no main thread");
    assert!(matches!(s.pool_state(&"verb"), Some(PoolState::Loading)));
    assert!(s.plugin(&"verb").is_err(), "no handle before it lands");
    loaded(&mut s, "verb");

    let handle = s.plugin(&"verb").expect("loaded");
    assert_eq!(
        crate::pooled::clap_id(handle.info().plugin()),
        tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap(),
        "the plugin the id names, not the file's first"
    );
    assert_eq!(handle.info().hosting(), Hosting::OutOfProcess);
    assert!(matches!(handle.presence(), Presence::Live));

    let node = s.add_pooled_plugin(&"verb").expect("the plugin enters");
    let view = s.plugin_view(node).expect("hosted");
    assert!(
        view.diagnostics().server_pid().is_some(),
        "an out-of-process plugin has its server"
    );
    assert!(matches!(
        s.add_pooled_plugin(&"verb"),
        Err(AddPluginError::Installed("verb"))
    ));
}

/// **An id the catalog does not hold fails the load, named**, and the entry
/// reads failed; it never lands as a default.
#[test]
fn an_id_the_catalog_lacks_fails_its_load() {
    let (catalog, _dir) = catalog();
    let mut s = session(true);
    let absent = PluginId::Clap("com.example.absent".into());
    s.pool(s.plugin_entry("gone", &absent, &catalog)).unwrap();
    let event = until(&mut s, "the failure", |k, _| *k == "gone");
    assert!(
        matches!(event, PoolEvent::Failed(LoadError::Unresolved(_))),
        "{event:?}"
    );
    assert!(matches!(s.pool_state(&"gone"), Some(PoolState::Failed(_))));
}

/// **A state saved from another plugin is refused where the entry is
/// declared**, before any load, as a request's `restoring` refuses it; the
/// plugin's own is taken.
#[test]
fn an_initial_state_of_another_plugin_is_refused() {
    let (catalog, _dir) = catalog();
    let s = session(true);
    let state_of = |id: &PluginId| {
        let entry = catalog
            .entries()
            .find(|entry| entry.id == Some(id))
            .expect("the plugin is listed");
        PeerState::of_plugin(entry.plugin_ref(), Vec::new())
    };
    let declared = s.plugin_entry("a", &second_id(), &catalog);
    assert!(matches!(
        declared.clone().initial_state(state_of(&first_id())),
        Err(OtherPeer { .. })
    ));
    assert!(declared.initial_state(state_of(&second_id())).is_ok());
}

/// **The server's death with recovery off is `Lost`, on the pool's one
/// reading, and `retry` loads the entry again.** The node goes (recovery is
/// off), an entry that is lost refuses a node, the old handle reads closed
/// after the reload, and the reloaded plugin is the next node's.
#[test]
fn a_dead_server_with_recovery_off_is_lost_and_retry_reloads() {
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    s.pool(s.plugin_entry("verb", &first_id(), &catalog))
        .unwrap();
    loaded(&mut s, "verb");
    let node = s.add_pooled_plugin(&"verb").unwrap();
    let before = s.plugin(&"verb").unwrap();
    assert!(matches!(before.presence(), Presence::Live));
    assert!(matches!(
        s.retry(&"verb"),
        Err(RetryError::NotFailed("verb"))
    ));

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
        "the entry says how the plugin died"
    );
    // The node stays at its key, silent, in the plugin's place: its latency is
    // the plugin's last, its plugin is released, and the entry still holds it.
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
        "the node does not revive the entry"
    );
    assert!(matches!(
        s.add_pooled_plugin(&"verb"),
        Err(AddPluginError::Lost("verb"))
    ));

    s.retry(&"verb").expect("a lost entry reloads");
    let event = until(&mut s, "the reload", |k, _| *k == "verb");
    assert!(matches!(event, PoolEvent::Reloaded), "{event:?}");
    assert!(
        matches!(before.presence(), Presence::Closed),
        "its entry was reloaded"
    );
    assert!(matches!(
        s.plugin(&"verb").unwrap().presence(),
        Presence::Live
    ));
    // The reloaded plugin is the node's: it swaps in at the same key, taking
    // the stand-in's place and its own tail back.
    pump_until(&mut s, "the node's new plugin", |s| {
        s.plugin_view(node)
            .is_ok_and(|view| view.diagnostics().server_pid() != Some(dead_pid))
    });
    assert_eq!(s.graph_ref().node_tail(node), Ok(tail));
    assert!(matches!(
        s.add_pooled_plugin(&"verb"),
        Err(AddPluginError::Installed("verb"))
    ));
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
    s.pool(s.plugin_entry("verb", &first_id(), &catalog))
        .unwrap();
    loaded(&mut s, "verb");
    let node = s.add_pooled_plugin(&"verb").unwrap();
    let handle = s.plugin(&"verb").unwrap();
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
    s.pool(s.plugin_entry("verb", &first_id(), &catalog))
        .unwrap();
    loaded(&mut s, "verb");
    let node = s.add_pooled_plugin(&"verb").unwrap();
    let handle = s.plugin(&"verb").unwrap();
    kill(&s, node);
    pump_until(&mut s, "the give-up", |_| !handle.presence().is_live());
    assert!(matches!(
        handle.presence(),
        Presence::Lost(PluginCause::GaveUp(_))
    ));
}

/// **An initial state is restored before the plugin is read, and two keys
/// with one id are two plugins.** A value set on one plugin and saved is the
/// value the other loads with; their servers are two processes.
#[test]
fn an_initial_state_is_restored_and_two_keys_load_two_plugins() {
    const CUTOFF: ParamId = ParamId::new(101);
    let (catalog, _dir) = catalog();
    let mut s = session(true);
    s.pool(s.plugin_entry("a", &first_id(), &catalog)).unwrap();
    loaded(&mut s, "a");
    let a = s.add_pooled_plugin(&"a").unwrap();
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
        .plugin_view(a)
        .unwrap()
        .state()
        .save(tutti_plugin_types::StateUse::Project)
        .expect("a state");

    s.pool((
        s.plugin_entry("a", &first_id(), &catalog),
        s.plugin_entry("b", &first_id(), &catalog)
            .initial_state(state)
            .expect("its own plugin's state"),
    ))
    .unwrap();
    loaded(&mut s, "b");
    let b = s.add_pooled_plugin(&"b").unwrap();
    let read = s
        .plugin_view(b)
        .unwrap()
        .params()
        .get(CUTOFF)
        .unwrap()
        .normalized();
    assert!(
        (read.get() - set.get()).abs() < 1e-6,
        "the state's value, not the default: {read:?}"
    );
    let pid = |node| s.plugin_view(node).unwrap().diagnostics().server_pid();
    assert_ne!(pid(a), pid(b), "two plugins, two servers");
    assert_ne!(s.plugin(&"a").unwrap().id(), s.plugin(&"b").unwrap().id());
}

/// **A state saved is the state of the plugin at the run, and the refusals are
/// named.** A value set on a served plugin and saved through `save_state` is the
/// value another entry loads with; a node that is gone and a state of another
/// plugin are refused at the call, and nothing is sent to the plugin.
#[test]
fn a_saved_state_holds_what_the_plugin_holds_and_refuses_by_name() {
    const CUTOFF: ParamId = ParamId::new(101);
    let main = tutti_fixtures::main_thread();
    let (catalog, _dir) = catalog();
    let mut s = session(true);
    s.pool((
        s.plugin_entry("a", &first_id(), &catalog),
        s.plugin_entry("idle", &first_id(), &catalog),
    ))
    .unwrap();
    // Both load in the same update, so a wait on events would miss the second.
    settle_until(&mut s, "both loads", |s| {
        s.plugin(&"a").is_ok() && s.plugin(&"idle").is_ok()
    });
    let a = s.add_pooled_plugin(&"a").unwrap();
    let gone = s.add_pooled_plugin(&"idle").unwrap();
    s.graph().remove(gone).unwrap();

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
        s.restore_state(&gone, foreign.clone()),
        Err(StateStartError::NoNode(_))
    ));
    assert!(matches!(
        s.restore_state(&a, foreign),
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
        .run(main)
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
        .run(main)
        .expect("a duplicate state");
    assert_eq!(
        duplicate.bytes()[CONTEXT_BYTE],
        2,
        "the use reaches the plugin: a duplicate save is FOR_DUPLICATE"
    );

    s.pool((
        s.plugin_entry("a", &first_id(), &catalog),
        s.plugin_entry("idle", &first_id(), &catalog),
        s.plugin_entry("b", &first_id(), &catalog)
            .initial_state(state)
            .expect("its own plugin's state"),
    ))
    .unwrap();
    loaded(&mut s, "b");
    let b = s.add_pooled_plugin(&"b").unwrap();
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
}

/// **A save of a plugin whose server was terminated ends as the crash, not a
/// state:** the run is `StateError::PluginCrashed`, and no empty state is saved
/// as the real thing.
#[test]
fn a_save_of_a_dead_plugin_is_the_plugins_refusal() {
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    s.pool(s.plugin_entry("a", &first_id(), &catalog)).unwrap();
    loaded(&mut s, "a");
    let node = s.add_pooled_plugin(&"a").unwrap();
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
        .run(tutti_fixtures::main_thread());
    assert!(
        matches!(saved, Err(StateError::PluginCrashed)),
        "the plugin's refusal: {saved:?}"
    );
}

/// **A reloaded plugin with other ports fails the entry, named, and the node
/// keeps the plugin it has**: swapping would leave the node's edges on ports
/// that are gone and refuse every later commit, so the swap is not made and
/// the unused plugin is dropped with the entry's value.
#[test]
fn a_swap_to_a_plugin_with_other_ports_fails_the_entry() {
    let (catalog, _dir) = catalog();
    let mut s = session(true);
    s.pool(s.plugin_entry("fx", &first_id(), &catalog)).unwrap();
    loaded(&mut s, "fx");
    let node = s.add_pooled_plugin(&"fx").unwrap();
    let first = tutti_clap_test_plugin::PLUGIN_ID.to_str().unwrap();

    // The next server starts with the probe's aux-bus layout.
    layout("1");
    s.pool(s.plugin_entry("fx", &second_id(), &catalog))
        .unwrap();
    let events = until_all(&mut s, "the failure", "fx");
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
    assert!(matches!(
        s.pool_state(&"fx"),
        Some(PoolState::Failed(LoadError::PluginPorts { .. }))
    ));
    assert!(s.plugin(&"fx").is_err(), "the unused plugin is gone");
    assert_eq!(
        crate::pooled::clap_id(s.plugin_view(node).unwrap().plugin()),
        first
    );
    let _ = pump(&mut s);
    assert!(
        s.update()
            .iter()
            .all(|event| !matches!(event, SessionEvent::CommitRefused(_))),
        "the graph still commits"
    );
}

/// **A changed identity under the same key loads the new plugin and the node
/// swaps to it once it lands.** The node is the second plugin afterwards, the
/// first handle reads closed, and the entry stays loaded throughout.
#[test]
fn a_changed_identity_swaps_the_node() {
    let (catalog, _dir) = catalog();
    let mut s = session(true);
    s.pool(s.plugin_entry("fx", &first_id(), &catalog)).unwrap();
    loaded(&mut s, "fx");
    let node = s.add_pooled_plugin(&"fx").unwrap();
    let first = s.plugin(&"fx").unwrap();
    let id_of_node =
        |s: &Session| crate::pooled::clap_id(s.plugin_view(node).unwrap().plugin()).to_owned();
    assert_eq!(
        id_of_node(&s),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().unwrap()
    );

    s.pool(s.plugin_entry("fx", &second_id(), &catalog))
        .unwrap();
    assert!(matches!(s.pool_state(&"fx"), Some(PoolState::Reloading)));
    let event = until(&mut s, "the reload", |k, _| *k == "fx");
    assert!(matches!(event, PoolEvent::Reloaded), "{event:?}");

    let second = tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while id_of_node(&s) != second {
        assert!(Instant::now() < deadline, "the node never swapped");
        let _ = s.update();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(first.presence(), Presence::Closed));
    assert_eq!(
        crate::pooled::clap_id(s.plugin(&"fx").unwrap().info().plugin()),
        second
    );
}

/// **A node holds its entry.** The app stops declaring the key and the entry
/// reads held while the node lives; removing the node releases it.
#[test]
fn a_node_holds_its_entry_until_it_is_removed() {
    let (catalog, _dir) = catalog();
    let mut s = session(true);
    s.pool(s.plugin_entry("fx", &first_id(), &catalog)).unwrap();
    loaded(&mut s, "fx");
    let node = s.add_pooled_plugin(&"fx").unwrap();
    let handle = s.plugin(&"fx").unwrap();

    s.pool::<&'static str>(()).unwrap();
    let _ = s.update();
    assert!(matches!(s.pool_state(&"fx"), Some(PoolState::Held)));
    assert!(matches!(handle.presence(), Presence::Live));

    s.remove(node).unwrap();
    let event = until(&mut s, "the release", |k, _| *k == "fx");
    assert!(matches!(event, PoolEvent::Released), "{event:?}");
    assert!(matches!(handle.presence(), Presence::Closed));
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

/// One session with the first plugin loaded and in a node, under key `fx`.
fn with_node(catalog: &Catalog) -> (Session, tutti_session::Key<tutti_session::AnyPlugin>) {
    with_node_in(session(true), catalog)
}

fn with_node_in<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    mut s: Session<K, C>,
    catalog: &Catalog,
) -> (Session<K, C>, tutti_session::Key<tutti_session::AnyPlugin>) {
    s.pool(s.plugin_entry("fx", &first_id(), catalog)).unwrap();
    loaded(&mut s, "fx");
    let node = s.add_pooled_plugin(&"fx").unwrap();
    (s, node)
}

fn pid_of<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &Session<K, C>,
    node: tutti_session::Key<tutti_session::AnyPlugin>,
) -> Option<u32> {
    s.plugin_view(node).unwrap().diagnostics().server_pid()
}

/// Declares `id` (hosted as `hosting`) under `fx`, with the aux-bus layout
/// `mode` for the plugin it loads.
fn declare_with_layout(
    s: &mut Session,
    catalog: &Catalog,
    id: &PluginId,
    hosting: Hosting,
    mode: &str,
) {
    layout(mode);
    s.pool(s.plugin_entry("fx", id, catalog).hosting(hosting))
        .unwrap();
}

/// **Equal channel totals with another split of buses are other ports**: two
/// mono buses where the node has one stereo bus. The totals match; the buses
/// the node's edges name do not.
#[test]
fn a_swap_to_the_same_totals_in_other_buses_fails_the_entry() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node(&catalog);
    declare_with_layout(&mut s, &catalog, &second_id(), Hosting::Auto, "3");
    let events = until_all(&mut s, "the failure", "fx");
    let [
        PoolEvent::Reloaded,
        PoolEvent::Failed(LoadError::PluginPorts { got, was }),
    ] = events.as_slice()
    else {
        panic!("{events:?}");
    };
    assert_eq!(
        (got.inputs.clone(), was.inputs.clone()),
        (
            vec![ChannelLayout::MONO, ChannelLayout::MONO],
            vec![ChannelLayout::STEREO]
        )
    );
    assert_eq!(
        crate::pooled::clap_id(s.plugin_view(node).unwrap().plugin()),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().unwrap()
    );
}

/// **After a refused swap the entry loads again and the node swaps**: a
/// `retry` to a plugin that fits lands as a load of an entry that has a node,
/// and the node takes it, where a second node for the key is refused.
#[test]
fn a_retry_after_a_refused_swap_swaps_the_node() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node(&catalog);
    let first = pid_of(&s, node);
    declare_with_layout(&mut s, &catalog, &second_id(), Hosting::Auto, "1");
    let events = until_all(&mut s, "the failure", "fx");
    assert!(matches!(
        events.as_slice(),
        [
            PoolEvent::Reloaded,
            PoolEvent::Failed(LoadError::PluginPorts { .. })
        ]
    ));

    // The next server has the node's layout.
    layout("0");
    s.retry(&"fx").expect("a failed entry loads again");
    let events = until_all(&mut s, "the retry", "fx");
    assert!(
        matches!(events.as_slice(), [PoolEvent::Loaded]),
        "{events:?}"
    );
    let second = tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap();
    pump_until(&mut s, "the swap", |s| {
        crate::pooled::clap_id(s.plugin_view(node).unwrap().plugin()) == second
    });
    assert_ne!(pid_of(&s, node), first, "another server");
    assert!(matches!(
        s.add_pooled_plugin(&"fx"),
        Err(AddPluginError::Installed("fx"))
    ));
}

/// **A new identity after a refused swap reaches the node**: the entry that
/// failed loads the next identity declared, and the node ends on it.
#[test]
fn a_later_identity_is_the_one_the_node_ends_on() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node(&catalog);
    let first = pid_of(&s, node);
    declare_with_layout(&mut s, &catalog, &second_id(), Hosting::Auto, "1");
    let events = until_all(&mut s, "the failure", "fx");
    assert!(matches!(events.last(), Some(PoolEvent::Failed(_))));
    // The same plugin served explicitly is another identity, with fitting ports.
    declare_with_layout(&mut s, &catalog, &first_id(), Hosting::OutOfProcess, "0");
    pump_until(&mut s, "the swap", |s| pid_of(s, node) != first);
    assert_eq!(
        crate::pooled::clap_id(s.plugin_view(node).unwrap().plugin()),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().unwrap()
    );
}

/// What a graph that re-prepares does to a swap: it waits, with the executor
/// taken, until the audio side has run and an update has collected.
fn repreparing<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &mut Session<K, C>,
) -> tutti_session::AudioSide {
    let side = s.graph().take_audio_side();
    s.graph().set_sample_rate(SampleRate::SR_44K1);
    assert!(s.graph_ref().testing().is_repreparing(), "mid re-prepare");
    side
}

/// **A swap waits for a graph that is re-preparing, and the last identity
/// declared meanwhile is the one it makes.** The node keeps its plugin until
/// the re-prepare resumes, then ends on the second declaration.
#[test]
fn a_swap_waits_for_the_graph_and_ends_on_the_last_identity() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node_in(detached(), &catalog);
    let first = pid_of(&s, node);
    let mut side = repreparing(&mut s);

    s.pool(s.plugin_entry("fx", &second_id(), &catalog))
        .unwrap();
    let events = until_all(&mut s, "the landing", "fx");
    assert!(
        matches!(events.as_slice(), [PoolEvent::Reloaded]),
        "{events:?}"
    );
    assert_eq!(
        pid_of(&s, node),
        first,
        "the graph is re-preparing: it waits"
    );
    // The landed plugin is the node's swap: a second node is refused, and the
    // refusal leaves the plugin for the swap.
    assert!(matches!(
        s.add_pooled_plugin(&"fx"),
        Err(AddPluginError::Installed("fx"))
    ));
    s.pool(
        s.plugin_entry("fx", &first_id(), &catalog)
            .hosting(Hosting::OutOfProcess),
    )
    .unwrap();
    let events = until_all(&mut s, "the second landing", "fx");
    assert!(
        matches!(events.as_slice(), [PoolEvent::Reloaded]),
        "{events:?}"
    );
    assert_eq!(pid_of(&s, node), first, "still waiting");
    // The last landing's plugin is likewise kept.
    assert!(matches!(
        s.add_pooled_plugin(&"fx"),
        Err(AddPluginError::Installed("fx"))
    ));

    let mut out = vec![vec![0.0; BLOCK]; 2];
    settle_until(&mut s, "the swap", |s| {
        side.render(BLOCK, PlanesMut::from_vecs(&mut out).unwrap());
        pid_of(s, node) != first
    });
    assert_eq!(
        crate::pooled::clap_id(s.plugin_view(node).unwrap().plugin()),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().unwrap(),
        "the last identity declared"
    );
}

/// **A node removed while its swap waits drops the swap**, and the landed
/// plugin stays the entry's for a next node.
#[test]
fn a_node_removed_while_its_swap_waits_leaves_the_plugin_to_the_entry() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node_in(detached(), &catalog);
    let mut side = repreparing(&mut s);
    s.pool(s.plugin_entry("fx", &second_id(), &catalog))
        .unwrap();
    let events = until_all(&mut s, "the landing", "fx");
    assert!(
        matches!(events.as_slice(), [PoolEvent::Reloaded]),
        "{events:?}"
    );

    s.remove(node).unwrap();
    let mut out = vec![vec![0.0; BLOCK]; 2];
    settle_until(&mut s, "the re-prepare to resume", |s| {
        side.render(BLOCK, PlanesMut::from_vecs(&mut out).unwrap());
        !s.graph_ref().testing().is_repreparing()
    });
    assert!(matches!(s.pool_state(&"fx"), Some(PoolState::Loaded)));
    let next = s
        .add_pooled_plugin(&"fx")
        .expect("the landed plugin is the next node's");
    assert_eq!(
        crate::pooled::clap_id(s.plugin_view(next).unwrap().plugin()),
        tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap()
    );
}

/// **A graph that refuses the swap fails the entry with the refusal**: a
/// poisoned graph is `PluginSwap(Poisoned)`, and the node keeps its plugin.
#[test]
fn a_graph_that_refuses_the_swap_fails_the_entry() {
    let (catalog, _dir) = catalog();
    let (mut s, node) = with_node(&catalog);
    s.graph().add(Unforkable(Grenade));
    // The session's executor is local, so both halves of the re-prepare run
    // here and the grenade's panic has poisoned the editor when this returns.
    s.graph().set_sample_rate(SampleRate::SR_96K);
    s.pool(s.plugin_entry("fx", &second_id(), &catalog))
        .unwrap();
    let events = until_all(&mut s, "the refusal", "fx");
    let [
        PoolEvent::Reloaded,
        PoolEvent::Failed(LoadError::PluginSwap(refused)),
    ] = events.as_slice()
    else {
        panic!("{events:?}");
    };
    assert!(
        matches!(**refused, CrossfadeRefused::Poisoned(_)),
        "{refused:?}"
    );
    assert_eq!(
        crate::pooled::clap_id(s.plugin_view(node).unwrap().plugin()),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().unwrap()
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

/// The cases that name the server by its process id.
#[cfg(target_os = "linux")]
mod process {
    use super::*;

    /// **A plugin that died before it had a node stays lost** under recovery too
    /// (nothing recovers a plugin no node hosts): the entry refuses a node, never
    /// reading live again without `retry`.
    #[test]
    fn a_plugin_dead_before_its_node_is_lost_and_refuses_one() {
        let (catalog, _dir) = catalog();
        let mut s = session(true);
        s.pool(s.plugin_entry("verb", &first_id(), &catalog))
            .unwrap();
        loaded(&mut s, "verb");
        let handle = s.plugin(&"verb").unwrap();
        let pid = handle.info().server_pid().expect("a served plugin");
        let killed = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .expect("kill runs");
        assert!(killed.success());
        pump_until(&mut s, "the loss", |_| !handle.presence().is_live());
        assert!(matches!(
            handle.presence(),
            Presence::Lost(PluginCause::Died(_))
        ));
        assert!(matches!(
            s.add_pooled_plugin(&"verb"),
            Err(AddPluginError::Lost("verb"))
        ));
    }

    /// **Removing the node leaves the entry loaded with its plugin gone**: the
    /// entry's handle keeps no server alive, and a second node is refused
    /// (`Installed`) until the entry reloads.
    #[test]
    fn a_removed_node_leaves_the_entry_without_a_plugin_and_frees_its_server() {
        let (catalog, _dir) = catalog();
        let mut s = session(true);
        s.pool(s.plugin_entry("fx", &first_id(), &catalog)).unwrap();
        loaded(&mut s, "fx");
        let node = s.add_pooled_plugin(&"fx").unwrap();
        let handle = s.plugin(&"fx").unwrap();
        let pid = s
            .plugin_view(node)
            .unwrap()
            .diagnostics()
            .server_pid()
            .expect("a served plugin has its server");
        let alive = || std::path::Path::new(&format!("/proc/{pid}")).exists();
        assert!(alive());

        s.remove(node).unwrap();
        pump_until(&mut s, "the server's end", |s| {
            let _ = s.update_on(tutti_fixtures::main_thread());
            !alive()
        });
        assert!(
            matches!(handle.presence(), Presence::Live),
            "the entry is still loaded"
        );
        assert!(matches!(s.pool_state(&"fx"), Some(PoolState::Loaded)));
        assert!(matches!(
            s.add_pooled_plugin(&"fx"),
            Err(AddPluginError::Installed("fx"))
        ));
    }

    /// **A served plugin's record is not parked:** removing the node drops it
    /// at once, with nothing in the graveyard, and the server ends under
    /// `Session::update` alone, which has no main thread to empty a yard.
    #[test]
    fn a_removed_served_plugin_is_dropped_with_no_graveyard_entry() {
        let (catalog, _dir) = catalog();
        let mut s = session(true);
        s.pool(s.plugin_entry("fx", &first_id(), &catalog)).unwrap();
        loaded(&mut s, "fx");
        let node = s.add_pooled_plugin(&"fx").unwrap();
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

/// **The node's dropped events keep accumulating across a reload of the
/// plugin at it.** A block of 300 notes is cut by the graph to the node's
/// declared input bound (`MIDI_STACK_BOUND`): the rest is reported as
/// `EventsDropped` at the node, and only that. The server is terminated and
/// recovery reloads the plugin at the same key; a second block of 300 is cut
/// the same, so the node's reports total twice the cut. The cut is the graph's,
/// not the plugin's, so `losses()` stays empty throughout.
#[test]
fn dropped_events_at_the_node_accumulate_across_a_reload_of_its_plugin() {
    use tutti_fixtures::midi;
    use tutti_graph::EventCapacity;
    use tutti_session::{Losses, Root, Stamp};
    use tutti_ump::MidiChannel;

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
    s.graph().connect(node, Root).unwrap();
    let (inlet, mut writer) = s.graph().open_event_inlet(EventCapacity::of(512));
    s.graph()
        .connect(inlet.events_out(), node.events_in())
        .unwrap();
    let notes = || (0..300).map(|i| midi::note_on(MidiChannel::FIRST, (i % 128) as u8, 0xFFFF));
    let losses = |s: &Session| s.plugin_view(node).unwrap().losses();
    let _ = pump(&mut s);
    assert_eq!(losses(&s), Losses::NONE, "nothing sent yet");

    // The cut is charged in the block that rendered it, reported by the next update.
    let mut dropped = 0;
    writer.push_all(notes().map(|n| (Stamp::NextBlock, n)));
    let _ = pump(&mut s);
    dropped += dropped_at(pump(&mut s), node.into());
    assert_eq!(dropped, cut);
    assert_eq!(
        losses(&s),
        Losses::NONE,
        "the graph's cut, not the plugin's"
    );

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
    dropped += dropped_at(pump(&mut s), node.into());
    assert_eq!(dropped, cut, "the reload drops nothing more");

    writer.push_all(notes().map(|n| (Stamp::NextBlock, n)));
    let _ = pump(&mut s);
    dropped += dropped_at(pump(&mut s), node.into());
    assert_eq!(
        dropped,
        cut + cut,
        "the reloaded plugin's node is cut the same: the reports add"
    );
    assert_eq!(losses(&s), Losses::NONE);
}
