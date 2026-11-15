//! A plugin hosted in process, as a pool entry: its main-thread stage, and the
//! drop of what only the main thread may drop. The reference CLAP plugin
//! (`Hosting::InProcess`) resolved through a catalog that probed it. The served
//! half is `tests/plugin/plugin_pool.rs`.
//!
//! Build the probe first: `cargo build -p tutti-plugin-probe`.

use std::thread;
use std::time::{Duration, Instant};

use tutti_plugin::catalog::{Catalog, CatalogConfig, Hosting, Locator, NO_SCAN_DIRS, PluginId};
use tutti_plugin_db::{PluginDb, PluginFormat, ScanPoll};
use tutti_plugin_types::MainWork;
use tutti_session::{Peer, PoolError, PoolEvent, PoolState, Presence, Session, SessionEvent};
use tutti_types::{MaxBlock, Samples};

use crate::support::clap_probe;

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
fn next_event(s: &mut Session, key: &'static str) -> PoolEvent {
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
    assert!(
        s.pooled_plugin(&"verb").is_err(),
        "nothing lands before an update"
    );
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Loaded));

    let handle = s.pooled_plugin(&"verb").expect("loaded");
    assert_eq!(handle.info().hosting(), Hosting::InProcess);
    assert_eq!(handle.presence(), Presence::Live);
    let node = s.add_pooled_plugin(&"verb").expect("the plugin enters");
    assert!(
        s.plugin(node).unwrap().diagnostics().server_pid().is_none(),
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

/// **A released entry's main-affine parts are dropped on the main thread**,
/// whichever thread releases it: the entry is released in an update on another
/// thread, which hands the drop of the in-process plugin and of its handle to
/// the session's main-work collector (`MainWork::deferred_drops` counts each),
/// and the main thread's next update runs them.
fn a_released_in_process_plugin_is_dropped_on_the_main_thread() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    s.pool(
        s.plugin_entry("verb", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Loaded));

    let before = MainWork::deferred_drops();
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
    assert_eq!(
        MainWork::deferred_drops() - before,
        2,
        "the plugin and the handle it was read through were handed to the main thread"
    );
    let _ = s.update();
    s.close(tutti_fixtures::main_thread());
}

/// **Released on the main thread, the same parts drop in place**: nothing is
/// handed over.
fn a_released_in_process_plugin_on_the_main_thread_is_not_deferred() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    s.pool(
        s.plugin_entry("verb", &id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Loaded));
    let before = MainWork::deferred_drops();
    s.pool::<&'static str>(()).unwrap();
    assert!(matches!(next_event(&mut s, "verb"), PoolEvent::Released));
    assert_eq!(MainWork::deferred_drops(), before);
    s.close(tutti_fixtures::main_thread());
}

pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["plugin_pool_main";
an_in_process_plugin_lands_in_an_update_on_the_main_thread,
an_in_process_plugin_needs_the_main_thread_and_is_refused_without_one,
a_released_in_process_plugin_is_dropped_on_the_main_thread,
a_released_in_process_plugin_on_the_main_thread_is_not_deferred,
];
