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
    // No update on the main thread meanwhile, longer than the deadline.
    thread::sleep(Duration::from_millis(2500));
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
];
