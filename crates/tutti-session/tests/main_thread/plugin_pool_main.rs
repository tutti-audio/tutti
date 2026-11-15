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
    let first = s.pooled_plugin(&"fx").unwrap();
    let second = tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap();
    assert_ne!(s.plugin(node).unwrap().descriptor().id, second);

    s.pool(
        s.plugin_entry("fx", &second_id(), &catalog)
            .hosting(Hosting::InProcess),
    )
    .unwrap();
    assert!(matches!(next_event(&mut s, "fx"), PoolEvent::Reloaded));
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.plugin(node).unwrap().descriptor().id != second {
        assert!(Instant::now() < deadline, "the node never swapped");
        let _ = s.update();
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(first.presence(), Presence::Closed);
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
    let before = s.plugin(node).unwrap().descriptor().id.clone();

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
    assert_eq!(s.plugin(node).unwrap().descriptor().id, before);
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
    assert_ne!(s.plugin(node).unwrap().descriptor().id, second);

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
            s.plugin(node).unwrap().descriptor().id,
            second,
            "an update off the main thread does not swap an in-process plugin"
        );
        s
    })
    .join()
    .unwrap();
    let _ = s.update();
    assert_eq!(s.plugin(node).unwrap().descriptor().id, second);
    s.close(tutti_fixtures::main_thread());
}

pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["plugin_pool_main";
an_in_process_plugin_lands_in_an_update_on_the_main_thread,
an_in_process_plugin_needs_the_main_thread_and_is_refused_without_one,
a_released_in_process_plugin_is_closed_on_the_main_thread,
a_released_in_process_plugin_on_the_main_thread_is_closed_in_place,
a_changed_identity_swaps_an_in_process_node,
an_in_process_swap_to_other_ports_fails_the_entry,
an_in_process_swap_waits_for_the_main_thread,
];
