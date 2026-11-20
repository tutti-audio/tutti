//! The pool's rules: the diff by key and identity, an entry's life, and the
//! refusals at the call. The jobs run on the test's thread: in `update`, or when
//! a test polls the shared executor first.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tutti_platform::Executor;
use tutti_units::SampleRate;

use super::table::HoldKind;
use super::{Declared, Life, Load, LoadError, PoolEntry, PoolError, PoolEvent, PoolKey, PoolState};
use crate::peer::sealed;
use crate::{HandleError, Peer, PeerError, Presence, Resource, ResourceId, Session};
use crate::{Job, JobKind, SessionEvent, WorkerError};

mod holds;
mod loads;
mod refresh;

/// What `events` landed for `job`, taken.
fn took<J: JobKind>(
    events: &mut [SessionEvent],
    job: &Job<J>,
) -> Option<Result<J::Output, J::Error>> {
    events.iter_mut().find_map(|event| match event {
        SessionEvent::JobDone(output) => output.take(job),
        _ => None,
    })
}

/// The session's polled executor, which a test shares with it. A load runs when the
/// session's `update` polls it, or when the test polls it first: `poll` returns how
/// many loads it ran.
type Spawned = Executor;

fn session() -> (crate::DetachedSession, Spawned) {
    let spawned = Executor::polled();
    let session = Session::builder()
        .executor(spawned.clone())
        .build_detached(SampleRate::SR_48K)
        .unwrap();
    (session, spawned)
}

/// The entry kind of a peer, as the plugin lane's would be: its identity is
/// the `version`, its `initial` is applied by the load and never compared.
struct Probe {
    version: u32,
    initial: u32,
    fail: bool,
    /// The load panics.
    panics: bool,
    runs: Arc<AtomicUsize>,
}

fn probe(version: u32, runs: &Arc<AtomicUsize>) -> Probe {
    Probe {
        version,
        initial: 0,
        fail: false,
        panics: false,
        runs: Arc::clone(runs),
    }
}

#[derive(Clone)]
struct ProbeHandle {
    id: ResourceId,
    loaded: u32,
    life: Life,
}

impl sealed::Sealed for ProbeHandle {}

impl Resource for ProbeHandle {
    fn id(&self) -> ResourceId {
        self.id
    }
}

impl Peer for ProbeHandle {
    type Info = u32;
    type Cause = &'static str;

    fn info(&self) -> Arc<u32> {
        Arc::new(self.loaded)
    }

    fn presence(&self) -> Presence<&'static str> {
        self.life.presence(|| Presence::Live)
    }
}

impl PoolEntry for Probe {
    type Identity = u32;
    type Loaded = u32;
    type Handle = ProbeHandle;

    fn identity(&self) -> u32 {
        self.version
    }

    fn load(&self) -> Load<u32> {
        let (version, initial, fail, panics, runs) = (
            self.version,
            self.initial,
            self.fail,
            self.panics,
            Arc::clone(&self.runs),
        );
        Load::on_worker(move || {
            runs.fetch_add(1, Ordering::SeqCst);
            assert!(!panics, "the load blew up");
            if fail {
                Err(LoadError::Job(WorkerError::Dropped))
            } else {
                Ok(version * 100 + initial)
            }
        })
    }

    fn handle(loaded: &u32, life: &Life) -> ProbeHandle {
        ProbeHandle {
            id: crate::peer::ResourceIds::new().mint(),
            loaded: *loaded,
            life: life.clone(),
        }
    }
}

fn entry(key: &'static str, probe: Probe) -> Declared<&'static str, Probe> {
    Declared::new(key, probe)
}

/// The pool events of one update, as `(key, what)`.
fn events<K: crate::AppKey, C: crate::Clock>(
    session: &mut Session<K, C>,
) -> Vec<(&'static str, String)> {
    session
        .update()
        .into_iter()
        .filter_map(|event| match event {
            SessionEvent::Pool { key, event } => Some((
                *key.get::<&'static str>().expect("a str key"),
                match event {
                    PoolEvent::Loaded => "loaded".to_owned(),
                    PoolEvent::Reloaded => "reloaded".to_owned(),
                    PoolEvent::Failed(error) => format!("failed: {error}"),
                    PoolEvent::Released => "released".to_owned(),
                },
            )),
            _ => None,
        })
        .collect()
}

fn state<K: crate::AppKey, C: crate::Clock<Float = f32>>(
    session: &Session<K, C>,
    key: &'static str,
) -> Option<String> {
    session.pool_state(&key).map(|state| match state {
        PoolState::Loading => "loading".to_owned(),
        PoolState::Loaded => "loaded".to_owned(),
        PoolState::Reloading => "reloading".to_owned(),
        PoolState::WaitingForMainThread => "waiting-for-main".to_owned(),
        PoolState::Opening => "opening".to_owned(),
        PoolState::Failed(_) => "failed".to_owned(),
        PoolState::Held => "held".to_owned(),
    })
}

fn handle<K: crate::AppKey, C: crate::Clock<Float = f32>>(
    session: &Session<K, C>,
    key: &'static str,
) -> Result<ProbeHandle, HandleError<&'static str>> {
    session.pool.entries.handle::<_, Probe>(&key)
}

fn undeclare(session: &mut crate::DetachedSession) {
    session.pool::<&'static str>(()).unwrap();
}

fn runs() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

#[test]
fn a_new_key_loads_once_and_lands_in_update() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    assert_eq!(state(&s, "a").as_deref(), Some("loading"));
    assert!(handle(&s, "a").is_err(), "nothing to read before it lands");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "no work runs before an update"
    );
    spawned.poll(usize::MAX);
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
    assert_eq!(*handle(&s, "a").unwrap().info(), 100);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[test]
fn declaring_a_loaded_key_with_the_same_identity_does_not_reload() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    // `initial` is not identity: it is applied at the load and never compared.
    let again = Probe {
        initial: 9,
        ..probe(1, &runs)
    };
    s.pool(entry("a", again)).unwrap();
    assert!(spawned.poll(usize::MAX) == 0, "no load started");
    assert!(events(&mut s).is_empty());
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
    assert_eq!(
        *handle(&s, "a").unwrap().info(),
        100,
        "the first load's value"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[test]
fn the_diff_adds_keeps_reloads_and_undeclares_by_key_and_identity() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool((
        entry("a", probe(1, &runs)),
        entry("b", probe(1, &runs)),
        entry("c", probe(1, &runs)),
    ))
    .unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    s.pool((
        entry("b", probe(2, &runs)), // identity changed
        entry("c", probe(1, &runs)), // unchanged
        entry("d", probe(1, &runs)), // new
    ))
    .unwrap();
    assert_eq!(state(&s, "a"), None, "undeclared, no hold: released");
    assert_eq!(state(&s, "b").as_deref(), Some("reloading"));
    assert_eq!(state(&s, "c").as_deref(), Some("loaded"));
    assert_eq!(state(&s, "d").as_deref(), Some("loading"));
    assert_eq!(spawned.poll(usize::MAX), 2, "b and d load; a, c do not");
}

#[test]
fn a_changed_identity_keeps_the_old_value_until_the_new_one_lands_then_closes_the_old_handle() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    let old = handle(&s, "a").unwrap();
    s.pool(entry("a", probe(2, &runs))).unwrap();
    assert_eq!(
        *handle(&s, "a").unwrap().info(),
        100,
        "Reloading reads as loaded"
    );
    assert_eq!(old.presence(), Presence::Live);
    spawned.poll(usize::MAX);
    assert_eq!(events(&mut s), [("a", "reloaded".to_owned())]);
    assert_eq!(*handle(&s, "a").unwrap().info(), 200);
    assert_eq!(
        old.presence(),
        Presence::Closed,
        "swapped to another identity"
    );
    assert_eq!(handle(&s, "a").unwrap().presence(), Presence::Live);
}

#[test]
fn a_load_superseded_after_it_ran_never_lands() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    // The first load ran and sent its end; it has not landed.
    assert_eq!(spawned.poll(usize::MAX), 1);
    s.pool(entry("a", probe(2, &runs))).unwrap();
    // One update polls the newer load and lands both ends, the superseded one
    // first: a landing that ignored the supersession would report twice.
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())], "one landing");
    assert_eq!(
        *handle(&s, "a").unwrap().info(),
        200,
        "the newer identity's value"
    );
}

#[test]
fn a_failed_load_reports_and_reads_failed() {
    let (mut s, spawned) = session();
    let runs = runs();
    let failing = Probe {
        fail: true,
        ..probe(1, &runs)
    };
    s.pool(entry("a", failing)).unwrap();
    spawned.poll(usize::MAX);
    let landed = events(&mut s);
    assert_eq!(landed.len(), 1);
    assert!(landed[0].1.starts_with("failed"), "{landed:?}");
    assert!(matches!(s.pool_state(&"a"), Some(PoolState::Failed(_))));
    assert!(matches!(
        handle(&s, "a"),
        Err(HandleError::NotLoaded(PoolState::Failed(_)))
    ));
}

#[test]
fn a_job_that_ended_without_a_result_fails_the_entry() {
    let (mut s, _spawned) = session();
    let runs = runs();
    let panicking = Probe {
        panics: true,
        ..probe(1, &runs)
    };
    s.pool(entry("a", panicking)).unwrap();
    assert!(matches!(s.pool_state(&"a"), Some(PoolState::Loading)));
    let landed = events(&mut s);
    assert_eq!(landed.len(), 1);
    assert!(landed[0].1.contains("the load blew up"), "{landed:?}");
    assert!(matches!(s.pool_state(&"a"), Some(PoolState::Failed(_))));
}

#[test]
fn landings_of_one_update_are_reported_in_declaration_order_not_landing_order() {
    let (mut s, _spawned) = session();
    let runs = runs();
    s.pool((
        entry("first", probe(1, &runs)),
        entry("second", probe(1, &runs)),
        entry("third", probe(1, &runs)),
    ))
    .unwrap();
    // "first" is declared again at a new identity: its superseding load is
    // queued behind the loads of "second" and "third", so it lands last.
    s.pool((
        entry("first", probe(2, &runs)),
        entry("second", probe(1, &runs)),
        entry("third", probe(1, &runs)),
    ))
    .unwrap();
    assert_eq!(
        events(&mut s)
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
}

#[test]
fn an_entry_no_one_holds_is_released_at_the_next_update_and_its_handle_closes() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    let handle = handle(&s, "a").unwrap();
    undeclare(&mut s);
    assert_eq!(state(&s, "a"), None);
    assert_eq!(handle.presence(), Presence::Closed);
    assert!(matches!(
        handle.life.check::<()>(),
        Err(PeerError::Released)
    ));
    assert_eq!(events(&mut s), [("a", "released".to_owned())]);
    assert!(events(&mut s).is_empty(), "released once");
}

#[test]
fn dropping_a_handle_leaves_its_entry_loaded() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    let first = handle(&s, "a").unwrap();
    let second = first.clone();
    drop(first);
    drop(second);
    assert!(events(&mut s).is_empty(), "no release, no reload");
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
    let again = handle(&s, "a").unwrap();
    assert_eq!(again.presence(), Presence::Live);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "loaded once");
}

#[test]
fn a_held_entry_is_released_at_its_last_holds_drop_not_before() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    let one = s
        .pool
        .entries
        .hold(&PoolKey::new(&"a"), HoldKind::Node)
        .unwrap();
    let two = one.clone();
    undeclare(&mut s);
    assert_eq!(state(&s, "a").as_deref(), Some("held"));
    assert!(events(&mut s).is_empty(), "held, not released");
    drop(one);
    assert!(events(&mut s).is_empty(), "one hold is left");
    assert_eq!(state(&s, "a").as_deref(), Some("held"));
    drop(two);
    assert_eq!(events(&mut s), [("a", "released".to_owned())]);
    assert_eq!(state(&s, "a"), None);
}

#[test]
fn declaring_a_held_key_with_its_identity_adopts_it_without_a_reload() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    let held = handle(&s, "a").unwrap();
    let hold = s
        .pool
        .entries
        .hold(&PoolKey::new(&"a"), HoldKind::Node)
        .unwrap();
    undeclare(&mut s);
    assert_eq!(state(&s, "a").as_deref(), Some("held"));
    s.pool(entry("a", probe(1, &runs))).unwrap();
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
    assert!(spawned.poll(usize::MAX) == 0, "no load started");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(held.presence(), Presence::Live, "the same entry");
    drop(hold);
    assert!(events(&mut s).is_empty(), "declared again, so not released");
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
}

#[test]
fn declaring_a_held_key_with_another_identity_reloads_it() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    let _hold = s
        .pool
        .entries
        .hold(&PoolKey::new(&"a"), HoldKind::Node)
        .unwrap();
    undeclare(&mut s);
    s.pool(entry("a", probe(2, &runs))).unwrap();
    assert_eq!(state(&s, "a").as_deref(), Some("reloading"));
    spawned.poll(usize::MAX);
    assert_eq!(events(&mut s), [("a", "reloaded".to_owned())]);
}

#[test]
fn a_key_named_twice_is_refused_and_nothing_of_the_set_is_applied() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("kept", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    let refused = s.pool((
        entry("new", probe(1, &runs)),
        entry("twice", probe(1, &runs)),
        entry("twice", probe(2, &runs)),
    ));
    assert_eq!(refused, Err(PoolError::DuplicateKey("twice")));
    assert_eq!(state(&s, "new"), None);
    assert_eq!(state(&s, "twice"), None);
    assert_eq!(
        state(&s, "kept").as_deref(),
        Some("loaded"),
        "not undeclared"
    );
    assert!(spawned.poll(usize::MAX) == 0);
}

#[test]
fn a_handle_of_another_kind_or_an_undeclared_key_is_refused() {
    struct Other;
    impl PoolEntry for Other {
        type Identity = ();
        type Loaded = ();
        type Handle = ();
        fn identity(&self) {}
        fn load(&self) -> Load<()> {
            Load::on_worker(|| Ok(()))
        }
        fn handle((): &(), _: &Life) {}
    }
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    assert!(matches!(
        s.pool.entries.handle::<_, Other>(&"a"),
        Err(HandleError::WrongKind("a"))
    ));
    assert!(matches!(
        s.pool.entries.handle::<_, Probe>(&"nope"),
        Err(HandleError::NotPooled("nope"))
    ));
}

#[test]
fn a_released_key_can_be_declared_again_and_loads_afresh() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    undeclare(&mut s);
    let _ = events(&mut s);
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

/// A second kind with the same identity type and value as [`Probe`].
struct Twin(u32);

impl PoolEntry for Twin {
    type Identity = u32;
    type Loaded = u32;
    type Handle = u32;

    fn identity(&self) -> u32 {
        self.0
    }

    fn load(&self) -> Load<u32> {
        let version = self.0;
        Load::on_worker(move || Ok(version))
    }

    fn handle(loaded: &u32, _: &Life) -> u32 {
        *loaded
    }
}

#[test]
fn a_key_declared_as_another_kind_with_an_equal_identity_reloads() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    let old = handle(&s, "a").unwrap();
    s.pool(Declared::new("a", Twin(1))).unwrap();
    assert_eq!(state(&s, "a").as_deref(), Some("reloading"));
    assert_eq!(spawned.poll(usize::MAX), 1, "a load started");
    s.pool(Declared::new("a", Twin(1))).unwrap();
    assert!(matches!(handle(&s, "a"), Err(HandleError::WrongKind("a"))));
    drop(old);
}

#[test]
fn a_failed_reload_drops_the_old_value_and_closes_its_handles() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    let old = handle(&s, "a").unwrap();
    let bad = Probe {
        fail: true,
        ..probe(2, &runs)
    };
    s.pool(entry("a", bad)).unwrap();
    assert_eq!(old.presence(), Presence::Live, "until the reload lands");
    spawned.poll(usize::MAX);
    let landed = events(&mut s);
    assert!(landed[0].1.starts_with("failed"), "{landed:?}");
    assert_eq!(old.presence(), Presence::Closed);
    assert_eq!(state(&s, "a").as_deref(), Some("failed"));
    assert!(handle(&s, "a").is_err());
}

#[test]
fn a_failed_entry_declared_again_with_its_identity_does_not_retry() {
    let (mut s, spawned) = session();
    let runs = runs();
    let bad = || Probe {
        fail: true,
        ..probe(1, &runs)
    };
    s.pool(entry("a", bad())).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    s.pool(entry("a", bad())).unwrap();
    assert!(spawned.poll(usize::MAX) == 0);
    assert_eq!(state(&s, "a").as_deref(), Some("failed"));
}

#[test]
fn an_outcome_that_landed_while_held_is_reported_when_the_entry_is_adopted() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    let hold = s
        .pool
        .entries
        .hold(&PoolKey::new(&"a"), HoldKind::Node)
        .unwrap();
    undeclare(&mut s);
    spawned.poll(usize::MAX);
    assert!(events(&mut s).is_empty(), "held: nothing to report yet");
    s.pool(entry("a", probe(1, &runs))).unwrap();
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())], "once");
    assert!(events(&mut s).is_empty());
    drop(hold);
}

#[test]
fn releases_are_reported_after_the_sets_landings_in_key_order() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool((
        entry("z", probe(1, &runs)),
        entry("m", probe(1, &runs)),
        entry("b", probe(1, &runs)),
    ))
    .unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    s.pool(entry("new", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    assert_eq!(
        events(&mut s),
        [
            ("new", "loaded".to_owned()),
            ("b", "released".to_owned()),
            ("m", "released".to_owned()),
            ("z", "released".to_owned()),
        ]
    );
}

#[test]
fn a_key_released_and_declared_again_before_the_report_is_not_released() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    undeclare(&mut s);
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
}

#[test]
fn a_released_slots_queued_outcome_is_not_reported_for_the_key_declared_again() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    let hold = s
        .pool
        .entries
        .hold(&PoolKey::new(&"a"), HoldKind::Node)
        .unwrap();
    undeclare(&mut s);
    spawned.poll(usize::MAX);
    assert!(events(&mut s).is_empty());
    // Adopting queues the held outcome; releasing and declaring again before
    // the report leaves only the new slot's landing.
    s.pool(entry("a", probe(1, &runs))).unwrap();
    drop(hold);
    undeclare(&mut s);
    s.pool(entry("a", probe(2, &runs))).unwrap();
    spawned.poll(usize::MAX);
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
}

#[test]
fn declaring_keys_of_another_type_undeclares_every_entry_of_the_first() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    s.pool(Declared::new(1_u32, probe(1, &runs))).unwrap();
    assert_eq!(state(&s, "a"), None);
    assert!(s.pool_state(&1_u32).is_some());
    spawned.poll(usize::MAX);
    let landed = s
        .update()
        .into_iter()
        .filter(|event| matches!(event, SessionEvent::Pool { .. }))
        .count();
    assert_eq!(landed, 2, "a is released, 1 loaded");
}

#[test]
fn a_refused_set_applies_nothing_while_a_release_is_pending() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("gone", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    s.pool(entry("kept", probe(1, &runs))).unwrap(); // releases "gone", unreported
    spawned.poll(usize::MAX);
    let refused = s.pool((
        entry("new", probe(1, &runs)),
        entry("twice", probe(1, &runs)),
        entry("twice", probe(1, &runs)),
    ));
    assert_eq!(refused, Err(PoolError::DuplicateKey("twice")));
    assert_eq!(
        state(&s, "kept").as_deref(),
        Some("loading"),
        "still declared"
    );
    assert_eq!(state(&s, "new"), None);
    assert_eq!(
        events(&mut s),
        [
            ("kept", "loaded".to_owned()),
            ("gone", "released".to_owned())
        ]
    );
}

#[cfg(feature = "sampler")]
mod audio {
    use super::*;
    use crate::{AudioEntry, audio_file};

    fn write_wav(path: &std::path::Path, frames: u32) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for frame in 0..frames {
            let value = frame as f32 / frames as f32;
            writer.write_sample(value).unwrap();
            writer.write_sample(-value).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn wave(session: &crate::DetachedSession, key: &'static str) -> Arc<tutti_buffers::Wave> {
        match session.audio(&key).unwrap() {
            AudioEntry::Resident(wave) => wave,
            other => panic!("a resident entry reads as its wave, got {other:?}"),
        }
    }

    #[test]
    fn an_audio_file_declared_loads_its_samples_and_a_new_path_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let (short, long) = (dir.path().join("short.wav"), dir.path().join("long.wav"));
        write_wav(&short, 100);
        write_wav(&long, 250);
        let (mut s, spawned) = session();
        s.pool(audio_file("kick", &short)).unwrap();
        spawned.poll(usize::MAX);
        assert_eq!(events(&mut s), [("kick", "loaded".to_owned())]);
        let loaded = wave(&s, "kick");
        assert_eq!(loaded.len().get(), 100);
        assert_eq!(loaded.channels(), 2);
        assert_eq!(loaded.at(1, 50), -0.5, "the file's own samples");
        s.pool(audio_file("kick", &short)).unwrap();
        assert!(
            spawned.poll(usize::MAX) == 0,
            "the same path is the same entry"
        );
        s.pool(audio_file("kick", &long)).unwrap();
        spawned.poll(usize::MAX);
        assert_eq!(events(&mut s), [("kick", "reloaded".to_owned())]);
        assert_eq!(wave(&s, "kick").len().get(), 250);
        assert_eq!(loaded.len().get(), 100, "a reader of the old wave keeps it");
    }

    /// An audio file held only as bytes (the `Vec<u8>` is the asset) loads the
    /// file's own samples, resident and streamed, and the entry still names it.
    #[test]
    fn an_audio_file_declared_from_bytes_loads_the_files_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 100);
        let bytes = std::fs::read(&path).unwrap();
        drop(dir);
        let (mut s, spawned) = session();
        s.pool((
            audio_file("whole", bytes.clone()),
            audio_file("piece", bytes).streamed(),
        ))
        .unwrap();
        spawned.poll(usize::MAX);
        let _ = events(&mut s);
        let loaded = wave(&s, "whole");
        assert_eq!(loaded.len().get(), 100);
        assert_eq!(loaded.at(1, 50), -0.5, "the file's own samples");
        let AudioEntry::Streamed(info) = s.audio(&"piece").unwrap() else {
            panic!("a streamed entry reads as its header");
        };
        assert_eq!(info.facts().frames.map(|frames| frames.get()), Some(100));
        assert!(info.asset().open().is_ok(), "the entry keeps the bytes");
    }

    #[test]
    fn dropping_an_audio_handle_leaves_the_entry_loaded_and_its_wave_shared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 10);
        let (mut s, spawned) = session();
        s.pool(audio_file("kick", &path)).unwrap();
        spawned.poll(usize::MAX);
        let _ = events(&mut s);
        let first = wave(&s, "kick");
        let handle = s.audio(&"kick").unwrap();
        drop(handle);
        drop(first.clone());
        assert!(events(&mut s).is_empty(), "no release, no reload");
        assert_eq!(state(&s, "kick").as_deref(), Some("loaded"));
        assert!(Arc::ptr_eq(&first, &wave(&s, "kick")), "the one decode");
        assert!(spawned.poll(usize::MAX) == 0, "nothing loads again");
    }

    #[test]
    fn a_streamed_entry_reads_the_files_header_not_its_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 480);
        let (mut s, spawned) = session();
        s.pool(audio_file("long", &path).streamed()).unwrap();
        spawned.poll(usize::MAX);
        assert_eq!(events(&mut s), [("long", "loaded".to_owned())]);
        let AudioEntry::Streamed(info) = s.audio(&"long").unwrap() else {
            panic!("a streamed entry reads as its header");
        };
        assert_eq!(info.asset().name(), path.display().to_string());
        assert_eq!(info.facts().frames.map(|frames| frames.get()), Some(480));
        assert_eq!(info.facts().layout.count(), 2);
        assert_eq!(info.facts().sample_rate, tutti_units::SampleRate::SR_48K);
    }

    #[test]
    fn the_tier_is_part_of_the_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 10);
        let (mut s, spawned) = session();
        s.pool(audio_file("a", &path)).unwrap();
        spawned.poll(usize::MAX);
        let _ = events(&mut s);
        s.pool(audio_file("a", &path).streamed()).unwrap();
        assert_eq!(state(&s, "a").as_deref(), Some("reloading"));
        spawned.poll(usize::MAX);
        assert_eq!(events(&mut s), [("a", "reloaded".to_owned())]);
        assert!(matches!(s.audio(&"a"), Ok(AudioEntry::Streamed(_))));
    }

    #[test]
    fn a_streamed_entry_of_a_file_that_is_no_audio_fails_with_a_probe_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        std::fs::write(&path, b"not audio").unwrap();
        let (mut s, spawned) = session();
        s.pool(audio_file("a", &path).streamed()).unwrap();
        spawned.poll(usize::MAX);
        let _ = s.update();
        assert!(matches!(
            s.pool_state(&"a"),
            Some(PoolState::Failed(LoadError::Probe(_)))
        ));
    }

    #[test]
    fn the_audio_reader_names_why_there_is_no_wave() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 10);
        let (mut s, spawned) = session();
        assert!(matches!(
            s.audio(&"kick"),
            Err(HandleError::NotPooled("kick"))
        ));
        s.pool((
            audio_file("kick", &path),
            Declared::new("other", probe(1, &runs())),
        ))
        .unwrap();
        assert!(matches!(
            s.audio(&"kick"),
            Err(HandleError::NotLoaded(PoolState::Loading))
        ));
        spawned.poll(usize::MAX);
        let _ = s.update();
        assert!(s.audio(&"kick").is_ok());
        assert!(matches!(
            s.audio(&"other"),
            Err(HandleError::WrongKind("other"))
        ));
        undeclare(&mut s);
        assert!(matches!(
            s.audio(&"kick"),
            Err(HandleError::NotPooled("kick"))
        ));
    }

    #[test]
    fn two_keys_of_one_audio_path_share_one_decode_and_one_wave() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 10);
        let (mut s, spawned) = session();
        s.pool((audio_file("kick", &path), audio_file("again", &path)))
            .unwrap();
        assert_eq!(spawned.poll(usize::MAX), 1, "one decode for both keys");
        assert_eq!(
            events(&mut s),
            [
                ("kick", "loaded".to_owned()),
                ("again", "loaded".to_owned())
            ]
        );
        assert!(Arc::ptr_eq(&wave(&s, "kick"), &wave(&s, "again")));
    }

    #[test]
    fn a_missing_audio_file_fails_with_a_decode_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, spawned) = session();
        s.pool(audio_file("kick", dir.path().join("absent.wav")))
            .unwrap();
        spawned.poll(usize::MAX);
        let _ = s.update();
        assert!(matches!(
            s.pool_state(&"kick"),
            Some(PoolState::Failed(LoadError::Decode(_)))
        ));
        assert!(matches!(
            s.audio(&"kick"),
            Err(HandleError::NotLoaded(PoolState::Failed(_)))
        ));
    }

    #[test]
    fn an_audio_file_undeclared_with_no_hold_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 10);
        let (mut s, spawned) = session();
        s.pool(audio_file("kick", &path)).unwrap();
        spawned.poll(usize::MAX);
        let _ = s.update();
        undeclare(&mut s);
        assert_eq!(events(&mut s), [("kick", "released".to_owned())]);
        assert!(s.pool_state(&"kick").is_none());
    }
}

#[cfg(feature = "soundfont")]
mod font {
    use super::*;
    use crate::soundfont;

    #[test]
    fn a_soundfont_declared_loads_its_presets_and_is_shared_by_its_arc() {
        let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
        let (mut s, spawned) = session();
        s.pool(soundfont("gm", &path)).unwrap();
        spawned.poll(usize::MAX);
        assert_eq!(events(&mut s), [("gm", "loaded".to_owned())]);
        let font = s.soundfont(&"gm").unwrap();
        let again = s.soundfont(&"gm").unwrap();
        assert!(Arc::ptr_eq(&font, &again), "one parsed font");
        assert!(!font.presets().is_empty());
    }
}

/// **A refusal of a landing that something newer superseded records nothing**:
/// the kind that refuses a landed value names the landing's generation, and an
/// entry with a load in flight (or a newer landing) is left to that load.
#[cfg(feature = "plugin")]
#[test]
fn a_stale_refusal_of_a_landing_leaves_the_load_in_flight_alone() {
    let (mut s, spawned) = session();
    let runs = Arc::new(AtomicUsize::new(0));
    let key = PoolKey::new(&"a");
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.poll(usize::MAX);
    let _ = events(&mut s);
    let (landing, _) = s.pool.entries.landing_of::<Probe>(&key).expect("loaded");
    let refusal = || LoadError::Job(WorkerError::Dropped);

    // A new declaration is in flight: the refusal of the old landing is stale.
    s.pool(entry("a", probe(2, &runs))).unwrap();
    assert_eq!(state(&s, "a").as_deref(), Some("reloading"));
    assert!(!s.pool.entries.refuse_landed(&key, landing, refusal()));
    assert_eq!(state(&s, "a").as_deref(), Some("reloading"), "untouched");
    spawned.poll(usize::MAX);
    assert_eq!(events(&mut s), [("a", "reloaded".to_owned())]);

    // The newer landing is refused by its own generation, and a refusal of
    // the old one (a second time over) still records nothing.
    assert!(!s.pool.entries.refuse_landed(&key, landing, refusal()));
    let (newer, _) = s.pool.entries.landing_of::<Probe>(&key).expect("loaded");
    assert_ne!(newer, landing);
    assert!(s.pool.entries.refuse_landed(&key, newer, refusal()));
    assert_eq!(state(&s, "a").as_deref(), Some("failed"));
    assert!(handle(&s, "a").is_err(), "its value is gone");
    assert_eq!(events(&mut s).len(), 1, "one failure");

    // A failed entry loads again as a first landing.
    s.retry(&"a").unwrap();
    spawned.poll(usize::MAX);
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
}

/// A clock switch, with loads of the session's own in flight (the pool's loads
/// run as jobs on the session's executor).
#[cfg(feature = "device")]
mod clock_switch {
    use tutti_buffers::ChannelLayout;
    use tutti_engine::{ManualStreamDriver, OutputSpec};

    use crate::JobStatus;

    use super::*;

    fn system_session() -> (crate::SystemSession, Spawned) {
        let spawned = Executor::polled();
        let session = Session::builder()
            .executor(spawned.clone())
            .system_clock()
            .build()
            .unwrap();
        // The streamer's future is not a load: it starts here, so what the tests
        // poll afterwards is the loads.
        spawned.poll(usize::MAX);
        (session, spawned)
    }

    fn card() -> (ManualStreamDriver, OutputSpec) {
        let (driver, _stream) = ManualStreamDriver::new();
        let spec = OutputSpec::new(
            SampleRate::SR_48K,
            ChannelLayout::STEREO,
            tutti_buffers::SampleFormat::F32,
        );
        (driver, spec)
    }

    /// **A loaded entry survives a clock switch, and each load in flight lands
    /// after it**: a job's lander is typed to nothing of the clock, so the
    /// loads that had not landed when the session converted land on the new
    /// session, once each, while the entry that had loaded is as it was.
    #[test]
    fn a_loaded_entry_survives_a_clock_switch_and_loads_in_flight_land_after_it() {
        let (mut s, _spawned) = system_session();
        let runs = runs();
        s.pool(entry("loaded", probe(1, &runs))).unwrap();
        assert_eq!(events(&mut s), [("loaded", "loaded".to_owned())]);
        // Declared after that update, "late" and "later" have not run.
        s.pool((
            entry("loaded", probe(1, &runs)),
            entry("late", probe(1, &runs)),
            entry("later", probe(1, &runs)),
        ))
        .unwrap();
        let (driver, spec) = card();
        let mut live = s
            .into_device_on(spec, driver)
            .unwrap_or_else(|e| panic!("switches: {}", e.error()));
        assert_eq!(state(&live, "late").as_deref(), Some("loading"));
        assert_eq!(state(&live, "later").as_deref(), Some("loading"));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "nothing ran at the switch");
        assert_eq!(
            events(&mut live),
            [
                ("late", "loaded".to_owned()),
                ("later", "loaded".to_owned())
            ],
            "each lands once, on the converted session"
        );
        assert_eq!(state(&live, "loaded").as_deref(), Some("loaded"));
        assert_eq!(*handle(&live, "late").unwrap().info(), 100);
        assert_eq!(*handle(&live, "loaded").unwrap().info(), 100, "intact");
        assert!(events(&mut live).is_empty(), "and no more");
    }

    fn live_session() -> (crate::LiveSession, Spawned, tutti_engine::ManualStream) {
        let spawned = Executor::polled();
        let (driver, stream) = ManualStreamDriver::new();
        let spec = OutputSpec::new(
            SampleRate::SR_48K,
            ChannelLayout::STEREO,
            tutti_buffers::SampleFormat::F32,
        );
        let session = Session::builder()
            .executor(spawned.clone())
            .device(Default::default())
            .build_device_on(spec, driver)
            .unwrap();
        // The streamer's future is not a load: it starts here, so what the tests
        // poll afterwards is the loads.
        spawned.poll(usize::MAX);
        (session, spawned, stream)
    }

    /// **A run whose entry has loaded runs before the switch begins, with the
    /// session as it is**: a run that reads the stream's faults or updates the
    /// session (what a run may do) finds the clock's stream in its place, in
    /// both directions, and the switch goes through.
    #[test]
    fn a_run_for_a_loaded_entry_runs_before_the_switch_with_the_session_intact() {
        let ran = Arc::new(AtomicUsize::new(0));

        let (mut s, spawned) = system_session();
        let runs = runs();
        s.pool(entry("a", probe(1, &runs))).unwrap();
        spawned.poll(usize::MAX);
        let _ = events(&mut s);
        let seen = Arc::clone(&ran);
        s.when_loaded(&"a", move |s| {
            let _ = s.faults();
            let _ = s.update();
            seen.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let (driver, spec) = card();
        let _live = s
            .into_device_on(spec, driver)
            .unwrap_or_else(|e| panic!("switches: {}", e.error()));
        assert_eq!(ran.load(Ordering::SeqCst), 1, "ran on the timer's session");

        let (mut live, spawned, _stream) = live_session();
        let runs = self::runs();
        live.pool(entry("b", probe(1, &runs))).unwrap();
        spawned.poll(usize::MAX);
        let _ = events(&mut live);
        let seen = Arc::clone(&ran);
        live.when_loaded(&"b", move |s| {
            let _ = s.faults();
            let _ = s.update();
            let _ = s.device();
            seen.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let _system = live
            .into_system_clock()
            .unwrap_or_else(|e| panic!("switches: {}", e.error()));
        assert_eq!(ran.load(Ordering::SeqCst), 2, "ran on the card's session");
    }

    /// **A switch is refused, before anything stops, while a run waits for an
    /// entry that has not loaded**: the error names the keys, the session is
    /// back on its old clock with its closure intact, and the run fires once
    /// the entry loads. In both directions.
    #[test]
    fn a_switch_is_refused_while_a_run_waits_for_an_entry_that_has_not_loaded() {
        let ran = Arc::new(AtomicUsize::new(0));

        let (mut s, spawned) = system_session();
        let runs = runs();
        s.pool(entry("pending", probe(1, &runs))).unwrap();
        let seen = Arc::clone(&ran);
        s.when_loaded(&"pending", move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let (driver, spec) = card();
        let err = s
            .into_device_on(spec, driver)
            .expect_err("a run waits for the entry");
        assert!(
            matches!(
                err.error().error(),
                crate::Error::RunsPending { keys } if keys.len() == 1
            ),
            "{}",
            err.error()
        );
        let mut s = err.into_session();
        assert_eq!(s.stream_spec().sample_rate, SampleRate::SR_48K);
        spawned.poll(usize::MAX);
        let _ = events(&mut s);
        let _ = s.update();
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the run fires when it loads");
        let (driver, spec) = card();
        let _live = s
            .into_device_on(spec, driver)
            .unwrap_or_else(|e| panic!("then it goes through: {}", e.error()));

        let (mut live, spawned, stream) = live_session();
        let runs = self::runs();
        live.pool(entry("pending", probe(1, &runs))).unwrap();
        let seen = Arc::clone(&ran);
        live.when_loaded(&"pending", move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let err = live
            .into_system_clock()
            .expect_err("a run waits for the entry");
        assert!(
            matches!(err.error().error(), crate::Error::RunsPending { .. }),
            "{}",
            err.error()
        );
        let mut live = err.into_session();
        assert!(live.device().is_running(), "still on the card");
        assert!(
            stream.render_block(tutti_units::Samples::new(64)).is_some(),
            "and rendering"
        );
        spawned.poll(usize::MAX);
        let _ = events(&mut live);
        let _ = live.update();
        assert_eq!(ran.load(Ordering::SeqCst), 2, "the run fires when it loads");
    }

    /// **A run asked from inside a due run runs before the switch too**: the
    /// due runs run until none is due, so one asked for a loaded entry is not
    /// left to the session's next clock, and one asked for an entry that has
    /// not loaded refuses the switch. In both directions.
    #[test]
    fn a_run_asked_by_a_run_runs_before_the_switch_and_one_for_a_pending_entry_refuses_it() {
        let ran = Arc::new(AtomicUsize::new(0));

        // The timer: R1 on a loaded entry asks R2 on the same entry.
        let (mut s, _spawned) = system_session();
        let runs = runs();
        s.pool(entry("a", probe(1, &runs))).unwrap();
        let _ = events(&mut s);
        // "b", declared after that update, has not run.
        s.pool((entry("a", probe(1, &runs)), entry("b", probe(1, &runs))))
            .unwrap();
        let (first, second) = (Arc::clone(&ran), Arc::clone(&ran));
        s.when_loaded(&"a", move |s| {
            first.fetch_add(1, Ordering::SeqCst);
            s.when_loaded(&"a", move |_| {
                second.fetch_add(10, Ordering::SeqCst);
            })
            .unwrap();
        })
        .unwrap();
        let (driver, spec) = card();
        let live = s
            .into_device_on(spec, driver)
            .unwrap_or_else(|e| panic!("switches: {}", e.error()));
        assert_eq!(ran.load(Ordering::SeqCst), 11, "both ran, on the timer");
        drop(live);

        // R1 on a loaded entry asks R2 on an entry that has not loaded.
        let (mut s, _spawned) = system_session();
        let runs = self::runs();
        s.pool(entry("a", probe(1, &runs))).unwrap();
        let _ = events(&mut s);
        s.pool((entry("a", probe(1, &runs)), entry("b", probe(1, &runs))))
            .unwrap();
        let asked = Arc::clone(&ran);
        s.when_loaded(&"a", move |s| {
            let late = Arc::clone(&asked);
            s.when_loaded(&"b", move |_| {
                late.fetch_add(100, Ordering::SeqCst);
            })
            .unwrap();
        })
        .unwrap();
        let (driver, spec) = card();
        let err = s
            .into_device_on(spec, driver)
            .expect_err("a run for an entry that has not loaded was asked");
        assert!(
            matches!(err.error().error(), crate::Error::RunsPending { keys } if keys.len() == 1),
            "{}",
            err.error()
        );
        let mut s = err.into_session();
        let _ = events(&mut s);
        let _ = s.update();
        assert_eq!(ran.load(Ordering::SeqCst), 111, "the asked run fires later");

        // The card: the same two.
        let (mut live, _spawned, _stream) = live_session();
        let runs = self::runs();
        live.pool(entry("a", probe(1, &runs))).unwrap();
        let _ = events(&mut live);
        live.pool((entry("a", probe(1, &runs)), entry("b", probe(1, &runs))))
            .unwrap();
        let (first, second) = (Arc::clone(&ran), Arc::clone(&ran));
        live.when_loaded(&"a", move |s| {
            first.fetch_add(1000, Ordering::SeqCst);
            s.when_loaded(&"a", move |_| {
                second.fetch_add(10_000, Ordering::SeqCst);
            })
            .unwrap();
        })
        .unwrap();
        let asked = Arc::clone(&ran);
        live.when_loaded(&"a", move |s| {
            let late = Arc::clone(&asked);
            s.when_loaded(&"b", move |_| {
                late.fetch_add(100_000, Ordering::SeqCst);
            })
            .unwrap();
        })
        .unwrap();
        let err = live
            .into_system_clock()
            .expect_err("a run for an entry that has not loaded was asked");
        assert!(
            matches!(err.error().error(), crate::Error::RunsPending { .. }),
            "{}",
            err.error()
        );
        assert_eq!(
            ran.load(Ordering::SeqCst),
            11_111,
            "the one asked of a loaded entry ran before the refusal"
        );
        drop(err);
    }

    /// **A switch is of the session as the due runs left it**: a run that
    /// sets an output map the next card cannot place makes the switch refuse it,
    /// and a run that restarts the card at another rate makes the switch to the
    /// timer carry that rate.
    #[test]
    fn a_switch_honours_what_a_due_run_did_to_the_session() {
        let spawned = Executor::polled();
        let mut s = Session::builder()
            .executor(spawned.clone())
            .outputs(ChannelLayout::from(6u16))
            .system_clock()
            .build()
            .unwrap();
        let runs = runs();
        s.pool(entry("a", probe(1, &runs))).unwrap();
        spawned.poll(usize::MAX);
        let _ = events(&mut s);
        s.when_loaded(&"a", |s| {
            s.set(s.policy::<crate::OutputPolicy>().to(crate::OutputPolicy {
                monitor: Some(tutti_buffers::Bed::Stereo.topology()),
                map: tutti_engine::OutputMap::channels([4, 5]).unwrap(),
                ..crate::OutputPolicy::default()
            }))
            .unwrap();
        })
        .unwrap();
        let (driver, spec) = card();
        let err = s
            .into_device_on(spec, driver)
            .expect_err("the run set a map the card has no outputs for");
        assert!(
            matches!(err.error().error(), crate::Error::MapUnplaceable { .. }),
            "{}",
            err.error()
        );
        let _ = err.into_session();

        let (mut live, spawned, _stream) = live_session();
        let runs = self::runs();
        live.pool(entry("a", probe(1, &runs))).unwrap();
        spawned.poll(usize::MAX);
        let _ = events(&mut live);
        live.when_loaded(&"a", |s| {
            let (driver, _new) = ManualStreamDriver::new();
            s.restart_on(
                OutputSpec::new(
                    SampleRate::SR_44K1,
                    ChannelLayout::STEREO,
                    tutti_buffers::SampleFormat::F32,
                ),
                driver,
                None,
                None,
            )
            .unwrap();
        })
        .unwrap();
        let system = live
            .into_system_clock()
            .unwrap_or_else(|e| panic!("switches: {}", e.error()));
        assert_eq!(
            system.stream_spec().sample_rate,
            SampleRate::SR_44K1,
            "the timer runs at the rate the run restarted the card to"
        );
    }

    /// **A wait for the pool to load carries over a clock switch**: the load
    /// in flight carries over too, and the wait taken before the switch lands
    /// its report on the new clock once the load does.
    #[test]
    fn a_pool_wait_carries_over_a_clock_switch() {
        let (mut s, _spawned) = system_session();
        let runs = runs();
        s.pool(entry("a", probe(1, &runs))).unwrap();
        let wait = s.pool_loaded::<&'static str>();
        assert_ne!(wait.status(), JobStatus::Done, "the load is in flight");
        let (driver, spec) = card();
        let mut live = s
            .into_device_on(spec, driver)
            .unwrap_or_else(|e| panic!("switches: {}", e.error()));
        assert_ne!(
            wait.status(),
            JobStatus::Done,
            "the load is still in flight"
        );
        assert_eq!(state(&live, "a").as_deref(), Some("loading"));
        let mut landed = live.update();
        assert!(
            took(&mut landed, &wait).is_some_and(|report| report.is_ok()),
            "the wait landed on the new clock"
        );
    }
}
