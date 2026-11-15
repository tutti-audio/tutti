//! The pool's rules: the diff by key and identity, an entry's life, and the
//! refusals at the call. The jobs run when a test says, on the test's thread.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tutti_types::SampleRate;

use super::{Declared, Life, Load, LoadError, PoolEntry, PoolError, PoolEvent, PoolKey, PoolState};
use crate::peer::sealed;
use crate::{HandleError, Job, Peer, PeerError, Presence, Resource, ResourceId, Session};
use crate::{SessionEvent, jobs::JobFailed};

mod loads;

/// The loads a session handed its spawner, held until the test runs them.
#[derive(Clone, Default)]
struct Spawned(Arc<Mutex<Vec<Job>>>);

impl Spawned {
    fn take(&self) -> Vec<Job> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    fn run_all(&self) {
        for job in self.take() {
            job.run();
        }
    }
}

fn session() -> (Session, Spawned) {
    let spawned = Spawned::default();
    let held = spawned.clone();
    let session = Session::builder()
        .spawn_with(move |job| held.0.lock().unwrap().push(job))
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
    runs: Arc<AtomicUsize>,
}

fn probe(version: u32, runs: &Arc<AtomicUsize>) -> Probe {
    Probe {
        version,
        initial: 0,
        fail: false,
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
        let (version, initial, fail, runs) = (
            self.version,
            self.initial,
            self.fail,
            Arc::clone(&self.runs),
        );
        Load::on_worker(move || {
            runs.fetch_add(1, Ordering::SeqCst);
            if fail {
                Err(LoadError::Job(JobFailed::Dropped))
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
fn events(session: &mut Session) -> Vec<(&'static str, String)> {
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

fn state(session: &Session, key: &'static str) -> Option<String> {
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

fn handle(session: &Session, key: &'static str) -> Result<ProbeHandle, HandleError<&'static str>> {
    session.pool.handle::<_, Probe>(&key)
}

fn undeclare(session: &mut Session) {
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
    assert!(
        events(&mut s).is_empty(),
        "nothing lands before the work ends"
    );
    spawned.run_all();
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
    spawned.run_all();
    let _ = events(&mut s);
    // `initial` is not identity: it is applied at the load and never compared.
    let again = Probe {
        initial: 9,
        ..probe(1, &runs)
    };
    s.pool(entry("a", again)).unwrap();
    assert!(spawned.take().is_empty(), "no load started");
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
    spawned.run_all();
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
    assert_eq!(spawned.take().len(), 2, "b and d load; a, c do not");
}

#[test]
fn a_changed_identity_keeps_the_old_value_until_the_new_one_lands_then_closes_the_old_handle() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.run_all();
    let _ = events(&mut s);
    let old = handle(&s, "a").unwrap();
    s.pool(entry("a", probe(2, &runs))).unwrap();
    assert_eq!(
        *handle(&s, "a").unwrap().info(),
        100,
        "Reloading reads as loaded"
    );
    assert_eq!(old.presence(), Presence::Live);
    spawned.run_all();
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
fn a_load_superseded_while_running_never_lands() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    let first = spawned.take();
    s.pool(entry("a", probe(2, &runs))).unwrap();
    let second = spawned.take();
    // The superseded load ends last, so a landing that ignored it would win.
    for job in second.into_iter().chain(first) {
        job.run();
    }
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
    spawned.run_all();
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
fn a_job_that_did_not_finish_fails_the_entry() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    drop(spawned.take()); // the spawner dropped it unrun
    assert!(matches!(s.pool_state(&"a"), Some(PoolState::Loading)));
    let landed = events(&mut s);
    assert_eq!(landed.len(), 1);
    assert!(
        landed[0].1.contains(&JobFailed::Dropped.to_string()),
        "{landed:?}"
    );
}

#[test]
fn landings_of_one_update_are_reported_in_declaration_order_not_landing_order() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool((
        entry("first", probe(1, &runs)),
        entry("second", probe(1, &runs)),
        entry("third", probe(1, &runs)),
    ))
    .unwrap();
    let mut jobs = spawned.take();
    jobs.reverse();
    for job in jobs {
        job.run();
    }
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
    spawned.run_all();
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
    spawned.run_all();
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
    spawned.run_all();
    let _ = events(&mut s);
    let one = s.pool.hold(&PoolKey::new(&"a")).unwrap();
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
    spawned.run_all();
    let _ = events(&mut s);
    let held = handle(&s, "a").unwrap();
    let hold = s.pool.hold(&PoolKey::new(&"a")).unwrap();
    undeclare(&mut s);
    assert_eq!(state(&s, "a").as_deref(), Some("held"));
    s.pool(entry("a", probe(1, &runs))).unwrap();
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
    assert!(spawned.take().is_empty(), "no load started");
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
    spawned.run_all();
    let _ = events(&mut s);
    let _hold = s.pool.hold(&PoolKey::new(&"a")).unwrap();
    undeclare(&mut s);
    s.pool(entry("a", probe(2, &runs))).unwrap();
    assert_eq!(state(&s, "a").as_deref(), Some("reloading"));
    spawned.run_all();
    assert_eq!(events(&mut s), [("a", "reloaded".to_owned())]);
}

#[test]
fn a_key_named_twice_is_refused_and_nothing_of_the_set_is_applied() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("kept", probe(1, &runs))).unwrap();
    spawned.run_all();
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
    assert!(spawned.take().is_empty());
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
    spawned.run_all();
    let _ = events(&mut s);
    assert!(matches!(
        s.pool.handle::<_, Other>(&"a"),
        Err(HandleError::WrongKind("a"))
    ));
    assert!(matches!(
        s.pool.handle::<_, Probe>(&"nope"),
        Err(HandleError::NotPooled("nope"))
    ));
}

#[test]
fn a_released_key_can_be_declared_again_and_loads_afresh() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.run_all();
    let _ = events(&mut s);
    undeclare(&mut s);
    let _ = events(&mut s);
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.run_all();
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
    spawned.run_all();
    let _ = events(&mut s);
    let old = handle(&s, "a").unwrap();
    s.pool(Declared::new("a", Twin(1))).unwrap();
    assert_eq!(state(&s, "a").as_deref(), Some("reloading"));
    assert_eq!(spawned.take().len(), 1, "a load started");
    s.pool(Declared::new("a", Twin(1))).unwrap();
    assert!(matches!(handle(&s, "a"), Err(HandleError::WrongKind("a"))));
    drop(old);
}

#[test]
fn a_failed_reload_drops_the_old_value_and_closes_its_handles() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.run_all();
    let _ = events(&mut s);
    let old = handle(&s, "a").unwrap();
    let bad = Probe {
        fail: true,
        ..probe(2, &runs)
    };
    s.pool(entry("a", bad)).unwrap();
    assert_eq!(old.presence(), Presence::Live, "until the reload lands");
    spawned.run_all();
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
    spawned.run_all();
    let _ = events(&mut s);
    s.pool(entry("a", bad())).unwrap();
    assert!(spawned.take().is_empty());
    assert_eq!(state(&s, "a").as_deref(), Some("failed"));
}

#[test]
fn an_outcome_that_landed_while_held_is_reported_when_the_entry_is_adopted() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    let hold = s.pool.hold(&PoolKey::new(&"a")).unwrap();
    undeclare(&mut s);
    spawned.run_all();
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
    spawned.run_all();
    let _ = events(&mut s);
    s.pool(entry("new", probe(1, &runs))).unwrap();
    spawned.run_all();
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
    spawned.run_all();
    let _ = events(&mut s);
    undeclare(&mut s);
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.run_all();
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
}

#[test]
fn a_released_slots_queued_outcome_is_not_reported_for_the_key_declared_again() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    let hold = s.pool.hold(&PoolKey::new(&"a")).unwrap();
    undeclare(&mut s);
    spawned.run_all();
    assert!(events(&mut s).is_empty());
    // Adopting queues the held outcome; releasing and declaring again before
    // the report leaves only the new slot's landing.
    s.pool(entry("a", probe(1, &runs))).unwrap();
    drop(hold);
    undeclare(&mut s);
    s.pool(entry("a", probe(2, &runs))).unwrap();
    spawned.run_all();
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
}

#[test]
fn declaring_keys_of_another_type_undeclares_every_entry_of_the_first() {
    let (mut s, spawned) = session();
    let runs = runs();
    s.pool(entry("a", probe(1, &runs))).unwrap();
    spawned.run_all();
    let _ = events(&mut s);
    s.pool(Declared::new(1_u32, probe(1, &runs))).unwrap();
    assert_eq!(state(&s, "a"), None);
    assert!(s.pool_state(&1_u32).is_some());
    spawned.run_all();
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
    spawned.run_all();
    let _ = events(&mut s);
    s.pool(entry("kept", probe(1, &runs))).unwrap(); // releases "gone", unreported
    spawned.run_all();
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

    fn wave(session: &Session, key: &'static str) -> Arc<tutti_codec::Wave> {
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
        spawned.run_all();
        assert_eq!(events(&mut s), [("kick", "loaded".to_owned())]);
        let loaded = wave(&s, "kick");
        assert_eq!(loaded.len().0, 100);
        assert_eq!(loaded.channels(), 2);
        assert_eq!(loaded.at(1, 50), -0.5, "the file's own samples");
        s.pool(audio_file("kick", &short)).unwrap();
        assert!(spawned.take().is_empty(), "the same path is the same entry");
        s.pool(audio_file("kick", &long)).unwrap();
        spawned.run_all();
        assert_eq!(events(&mut s), [("kick", "reloaded".to_owned())]);
        assert_eq!(wave(&s, "kick").len().0, 250);
        assert_eq!(loaded.len().0, 100, "a reader of the old wave keeps it");
    }

    #[test]
    fn dropping_an_audio_handle_leaves_the_entry_loaded_and_its_wave_shared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 10);
        let (mut s, spawned) = session();
        s.pool(audio_file("kick", &path)).unwrap();
        spawned.run_all();
        let _ = events(&mut s);
        let first = wave(&s, "kick");
        let handle = s.audio(&"kick").unwrap();
        drop(handle);
        drop(first.clone());
        assert!(events(&mut s).is_empty(), "no release, no reload");
        assert_eq!(state(&s, "kick").as_deref(), Some("loaded"));
        assert!(Arc::ptr_eq(&first, &wave(&s, "kick")), "the one decode");
        assert!(spawned.take().is_empty(), "nothing loads again");
    }

    #[test]
    fn a_streamed_entry_reads_the_files_header_not_its_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 480);
        let (mut s, spawned) = session();
        s.pool(audio_file("long", &path).streamed()).unwrap();
        spawned.run_all();
        assert_eq!(events(&mut s), [("long", "loaded".to_owned())]);
        let AudioEntry::Streamed(info) = s.audio(&"long").unwrap() else {
            panic!("a streamed entry reads as its header");
        };
        assert_eq!(info.path(), path);
        assert_eq!(info.facts().frames.map(|frames| frames.0), Some(480));
        assert_eq!(info.facts().layout.count(), 2);
        assert_eq!(info.facts().sample_rate, tutti_types::SampleRate::SR_48K);
    }

    #[test]
    fn the_tier_is_part_of_the_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(&path, 10);
        let (mut s, spawned) = session();
        s.pool(audio_file("a", &path)).unwrap();
        spawned.run_all();
        let _ = events(&mut s);
        s.pool(audio_file("a", &path).streamed()).unwrap();
        assert_eq!(state(&s, "a").as_deref(), Some("reloading"));
        spawned.run_all();
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
        spawned.run_all();
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
        spawned.run_all();
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
        let jobs = spawned.take();
        assert_eq!(jobs.len(), 1, "one decode for both keys");
        for job in jobs {
            job.run();
        }
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
        spawned.run_all();
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
        spawned.run_all();
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
        spawned.run_all();
        assert_eq!(events(&mut s), [("gm", "loaded".to_owned())]);
        let font = s.soundfont(&"gm").unwrap();
        let again = s.soundfont(&"gm").unwrap();
        assert!(Arc::ptr_eq(&font, &again), "one parsed font");
        assert!(!font.presets().is_empty());
    }
}
