//! What a load asks of the catalog and of the entry's `Hosting`, through the
//! session: the reference CLAP plugin (`clap_probe`), declared as a pool entry.

use crate::pooled::{LoadEntry, LoadPlugin, Spec, catalog_of, probe};
use crate::support::clap_probe;
use std::time::Duration;
use tutti_buffers::MaxBlock;
use tutti_platform::Instant;
use tutti_plugin::catalog::{Hosting, PluginRef};
use tutti_plugin_types::IpcError;
use tutti_session::{LoadError, PoolEvent, Session, SessionEvent};
use tutti_units::Samples;

/// An offline session with one background thread running: the loads here are
/// background work, and the tests update until they land.
fn session() -> (Session, tutti_session::Background) {
    let (s, mut background) = Session::builder()
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("an offline session builds");
    background
        .spawn_threads(std::num::NonZeroUsize::MIN)
        .expect("a background thread starts");
    (s, background)
}

/// Names the server the test's plugin is served by. It is the first thing a
/// test does: nextest runs each test in its own process, and nothing of the
/// test (no catalog, no session) has started a thread yet.
fn set_server(path: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: as above, no other thread exists.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", path) };
}

/// **A blacklisted plugin is not in the catalog's answer, and is admitted once
/// the user unblacklists it**: the entry of a blacklisted file fails with
/// `Unresolved` (a blacklisted file keeps no descriptors, so its plugins resolve
/// missing), and one built after the user unblacklists and the file is probed
/// again loads.
#[test]
fn a_blacklisted_plugin_does_not_resolve_and_loads_once_unblacklisted() {
    set_server(tutti_fixtures::plugin_server());
    let path = clap_probe();
    let plugin = PluginRef::file(&path);
    let (mut catalog, id, _dir) = catalog_of(&plugin);
    catalog.blacklist(&path, "SIGSEGV during probe");
    let (mut s, _bg) = session();
    let main = tutti_fixtures::main_thread();

    match s.load_entry_blocking(main, &catalog, &id, Hosting::Auto, None) {
        Err(LoadError::Unresolved(_)) => {}
        other => panic!("expected an unresolved entry, got {other:?}"),
    }

    assert!(catalog.unblacklist(&path));
    probe(&mut catalog, &path);
    s.load_entry_blocking(main, &catalog, &id, Hosting::Auto, None)
        .expect("an unblacklisted plugin loads");
}

/// The first pool event that is an outcome (a load that landed or failed), on
/// this thread.
fn outcome(s: &mut Session, main: tutti_plugin_types::MainThreadMarker) -> PoolEvent {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        for event in s.update_on(main) {
            if let SessionEvent::Pool {
                event: event @ (PoolEvent::Loaded | PoolEvent::Failed(_)),
                ..
            } = event
            {
                return event;
            }
        }
        assert!(Instant::now() < deadline, "the load never landed");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A session whose background the test drives: the futures it is handed wait until the
/// test runs the `Background`.
fn holding() -> (Session, tutti_session::Background) {
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("an offline session builds")
}

fn run_held(background: &mut tutti_session::Background) {
    background.run_until_stalled();
}

/// **The blacklist is asked when the load runs, not when the entry is built**:
/// a plugin the catalog blacklists after the entry resolved it is refused with
/// the recorded reason, and one it blacklists and then clears before the load
/// runs is admitted. The entry is built while the plugin is admitted (a
/// blacklisted one resolves missing); the load is held between the entry and
/// its run.
#[test]
fn the_blacklist_is_asked_when_the_load_runs() {
    set_server(tutti_fixtures::plugin_server());
    let path = clap_probe();
    let plugin = PluginRef::file(&path);
    let (mut catalog, id, _dir) = catalog_of(&plugin);
    let main = tutti_fixtures::main_thread();

    // Blacklisted before the load runs, and still: refused.
    let (mut s, mut background) = holding();
    let _ = s.update_on(main);
    s.pool(s.plugin_entry(0u64, &id, &catalog)).unwrap();
    catalog.blacklist(&path, "hung in its load");
    run_held(&mut background);
    match outcome(&mut s, main) {
        PoolEvent::Failed(LoadError::Plugin(error)) => match &*error {
            IpcError::Blacklisted { reason, .. } => assert_eq!(reason, "hung in its load"),
            other => panic!("expected a Blacklisted refusal, got {other:?}"),
        },
        other => panic!("expected a Blacklisted refusal at load, got {other:?}"),
    }

    // Blacklisted and cleared before the load runs: admitted. The cleared file
    // keeps no descriptors, so its entry is built before the blacklist.
    assert!(catalog.unblacklist(&path));
    probe(&mut catalog, &path);
    let (mut s, mut background) = holding();
    let _ = s.update_on(main);
    s.pool(s.plugin_entry(0u64, &id, &catalog)).unwrap();
    catalog.blacklist(&path, "hung in its load");
    assert!(catalog.unblacklist(&path));
    run_held(&mut background);
    assert!(
        matches!(outcome(&mut s, main), PoolEvent::Loaded),
        "an unblacklisted plugin was refused"
    );
}

/// **A hosting the format cannot have is refused at load**: VST2 asked to be
/// served is `NotOutOfProcess`, and the entry is not changed to in process.
#[cfg(feature = "vst2")]
#[test]
fn vst2_asked_for_out_of_process_is_refused_at_load() {
    let plugin = PluginRef::file(tutti_fixtures::vst2_probe());
    let (catalog, id, _dir) = catalog_of(&plugin);
    let (mut s, _bg) = session();
    match s.load_entry_blocking(
        tutti_fixtures::main_thread(),
        &catalog,
        &id,
        Hosting::OutOfProcess,
        None,
    ) {
        Err(LoadError::Plugin(error)) => match &*error {
            IpcError::NotOutOfProcess { format, .. } => assert_eq!(*format, "VST2"),
            other => panic!("expected NotOutOfProcess, got {other:?}"),
        },
        other => panic!("expected NotOutOfProcess, got {other:?}"),
    }
}

/// **`Hosting::InProcess` makes the session host a CLAP plugin in its own
/// process**: no server is started (none can be found here), and the hosted
/// plugin is the one asked for.
#[test]
fn a_clap_plugin_asked_in_process_is_hosted_here() {
    set_server("/nonexistent/plugin-server");
    let plugin = PluginRef::file(clap_probe());
    let (mut s, _bg) = session();
    let node = s
        .load_plugin_blocking(
            tutti_fixtures::main_thread(),
            Spec::new(plugin).hosting(Hosting::InProcess),
        )
        .expect("the reference plugin loads in this process");
    let hosted = s.plugin_view(node).expect("it is hosted");
    assert_eq!(hosted.diagnostics().server_pid(), None);
    assert_eq!(
        crate::pooled::clap_id(hosted.plugin()),
        tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8")
    );
}
