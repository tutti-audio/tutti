//! A plugin as a pool entry, served: the reference CLAP bundle (two plugins) in
//! a real `plugin-server`, resolved through a catalog that scanned it. The
//! in-process half is `tests/main_thread/plugin_pool_main.rs`.
//!
//! Build the server and the probe first: `cargo build -p tutti-plugin-server
//! -p tutti-plugin-probe`.

use std::time::{Duration, Instant};

use tutti_plugin::catalog::{
    Catalog, CatalogConfig, Hosting, Locator, NO_SCAN_DIRS, PluginId, PluginRef, PluginState,
};
use tutti_plugin::error::StateError;
use tutti_plugin_db::{PluginDb, PluginFormat, ScanPoll};
use tutti_plugin_types::TerminationReason;
use tutti_session::{
    AddPluginError, LoadError, Peer, PluginCause, PoolEvent, PoolState, Presence, RetryError,
    Session, SessionEvent,
};
use tutti_types::{MaxBlock, Samples};

use crate::support::{clap_probe, plugin_server};

fn first_id() -> PluginId {
    PluginId::new(
        PluginFormat::Clap,
        tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8"),
    )
}

fn second_id() -> PluginId {
    PluginId::new(
        PluginFormat::Clap,
        tutti_clap_test_plugin::SECOND_PLUGIN_ID
            .to_str()
            .expect("utf-8"),
    )
}

/// A catalog that scanned the reference bundle (both of its plugins).
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
                std::thread::sleep(Duration::from_millis(2));
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

fn session(recovery: bool) -> Session {
    // SAFETY: nextest runs each test in its own process, and this is set from
    // the test's own thread before anything spawns a server.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server()) };
    let builder = Session::builder().max_block(MaxBlock::of(Samples(512)));
    let builder = if recovery {
        builder
    } else {
        builder.plugin_recovery(None)
    };
    builder.build().expect("an offline session builds")
}

/// Updates until `done` finds what it waits for in an update's pool events.
fn until(
    s: &mut Session,
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

fn loaded(s: &mut Session, key: &'static str) {
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
    assert!(
        s.pooled_plugin(&"verb").is_err(),
        "no handle before it lands"
    );
    loaded(&mut s, "verb");

    let handle = s.pooled_plugin(&"verb").expect("loaded");
    assert_eq!(
        handle.info().descriptor().id,
        tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap(),
        "the plugin the id names, not the file's first"
    );
    assert_eq!(handle.info().hosting(), Hosting::OutOfProcess);
    assert_eq!(handle.presence(), Presence::Live);

    let node = s.add_pooled_plugin(&"verb").expect("the plugin enters");
    let view = s.plugin(node).expect("hosted");
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
    let absent = PluginId::new(PluginFormat::Clap, "com.example.absent");
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
            .find(|entry| entry.descriptor.id == id.id)
            .expect("the plugin is listed");
        PluginState::new(PluginRef::from(&entry), Vec::new())
    };
    let declared = s.plugin_entry("a", &second_id(), &catalog);
    assert!(matches!(
        declared.clone().initial_state(state_of(&first_id())),
        Err(StateError::OtherPlugin { .. })
    ));
    assert!(declared.initial_state(state_of(&second_id())).is_ok());
}

/// **The server's death with recovery off is `Lost`, on the pool's one
/// reading, and `retry` loads the entry again.** The old handle then reads
/// closed, the new one live.
#[test]
fn a_dead_server_with_recovery_off_is_lost_and_retry_reloads() {
    let (catalog, _dir) = catalog();
    let mut s = session(false);
    s.pool(s.plugin_entry("verb", &first_id(), &catalog))
        .unwrap();
    loaded(&mut s, "verb");
    let node = s.add_pooled_plugin(&"verb").unwrap();
    let before = s.pooled_plugin(&"verb").unwrap();
    assert_eq!(before.presence(), Presence::Live);
    assert!(matches!(
        s.retry(&"verb"),
        Err(RetryError::NotFailed("verb"))
    ));

    s.plugin(node)
        .unwrap()
        .diagnostics()
        .terminate(TerminationReason::Hung {
            blocks: 16,
            held: Duration::from_secs(5),
        });
    let deadline = Instant::now() + Duration::from_secs(10);
    while before.presence().is_live() {
        assert!(Instant::now() < deadline, "the death never showed");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(before.presence(), Presence::Lost(PluginCause::Died));

    s.retry(&"verb").expect("a lost entry reloads");
    let event = until(&mut s, "the reload", |k, _| *k == "verb");
    assert!(matches!(event, PoolEvent::Reloaded), "{event:?}");
    assert_eq!(
        before.presence(),
        Presence::Closed,
        "its entry was reloaded"
    );
    assert_eq!(s.pooled_plugin(&"verb").unwrap().presence(), Presence::Live);
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
    let first = s.pooled_plugin(&"fx").unwrap();
    let id_of_node = |s: &Session| s.plugin(node).unwrap().descriptor().id.clone();
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
    assert_eq!(first.presence(), Presence::Closed);
    assert_eq!(
        s.pooled_plugin(&"fx").unwrap().info().descriptor().id,
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
    let handle = s.pooled_plugin(&"fx").unwrap();

    s.pool::<&'static str>(()).unwrap();
    let _ = s.update();
    assert!(matches!(s.pool_state(&"fx"), Some(PoolState::Held)));
    assert_eq!(handle.presence(), Presence::Live);

    s.remove(node).unwrap();
    let event = until(&mut s, "the release", |k, _| *k == "fx");
    assert!(matches!(event, PoolEvent::Released), "{event:?}");
    assert_eq!(handle.presence(), Presence::Closed);
}
