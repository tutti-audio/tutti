//! The load runner: each stage kind and its thread, a cancel between stages,
//! the parallel cap, retry, the report and the shared load. The jobs run when
//! a test says; the main thread is a thread the test makes one for.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};

use tutti_rt::MainThreadMarker;

use super::{Spawned, events, session, state};
use crate::{
    Declared, HandleError, Life, Load, LoadError, PoolConfig, PoolEntry, PoolError, PoolOutcome,
    PoolWaitError, RetryError, Session, jobs::JobFailed,
};

/// What ran, and on which thread, in the order it ran.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<(String, ThreadId)>>>);

impl Log {
    fn push(&self, what: impl Into<String>) {
        self.0
            .lock()
            .unwrap()
            .push((what.into(), thread::current().id()));
    }

    fn entries(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(what, _)| what.clone())
            .collect()
    }

    /// The thread `what` was logged on.
    fn thread_of(&self, what: &str) -> ThreadId {
        let log = self.0.lock().unwrap();
        let found: Vec<_> = log.iter().filter(|(logged, _)| logged == what).collect();
        assert_eq!(
            found.len(),
            1,
            "{what:?} logged once, in {:?}",
            self.entries()
        );
        found[0].1
    }

    fn count(&self, what: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(logged, _)| logged == what)
            .count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum How {
    Worker,
    Main,
    Session,
}

/// What a stub loads: a value, and whether the kind reads it as lost.
struct Made {
    value: u32,
    lost: Arc<AtomicBool>,
}

/// What the worker stage hands the main stage: it logs where it is dropped.
struct Output {
    log: Log,
    name: &'static str,
}

impl Drop for Output {
    fn drop(&mut self) {
        self.log.push(format!("drop output {}", self.name));
    }
}

/// An entry kind that loads in the stages it is told to.
#[derive(Clone)]
struct Stub {
    name: &'static str,
    version: u32,
    how: How,
    log: Log,
    fail: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
}

fn stub(name: &'static str, how: How, log: &Log) -> Stub {
    Stub {
        name,
        version: 1,
        how,
        log: log.clone(),
        fail: Arc::default(),
        lost: Arc::default(),
    }
}

impl PoolEntry for Stub {
    type Identity = u32;
    type Loaded = Arc<Made>;
    type Handle = Arc<Made>;

    fn identity(&self) -> u32 {
        self.version
    }

    fn load(&self) -> Load<Arc<Made>> {
        let Self {
            name,
            version,
            how,
            log,
            fail,
            lost,
        } = self.clone();
        let made = move || {
            if fail.load(Ordering::SeqCst) {
                Err(LoadError::Job(JobFailed::Dropped))
            } else {
                Ok(Arc::new(Made {
                    value: version,
                    lost,
                }))
            }
        };
        match how {
            How::Worker => Load::on_worker(move || {
                log.push(format!("worker {name}"));
                made()
            }),
            How::Main => {
                let after = log.clone();
                Load::on_worker_then_main(
                    move || {
                        log.push(format!("worker {name}"));
                        Ok(Output { log, name })
                    },
                    move |output, _main: MainThreadMarker| {
                        after.push(format!("main {name}"));
                        drop(output);
                        made()
                    },
                )
            }
            How::Session => Load::on_session(move |cx| {
                log.push(format!("session {name} at {}", cx.rate().get()));
                made()
            }),
        }
    }

    fn handle(loaded: &Arc<Made>, _: &Life) -> Arc<Made> {
        Arc::clone(loaded)
    }

    fn is_lost(loaded: &Arc<Made>) -> bool {
        loaded.lost.load(Ordering::SeqCst)
    }
}

fn declare(stub: &Stub) -> Declared<&'static str, Stub> {
    Declared::new(stub.name, stub.clone())
}

fn made(session: &Session, key: &'static str) -> Result<Arc<Made>, HandleError<&'static str>> {
    session.pool.handle::<_, Stub>(&key)
}

/// Runs `f` on a thread that holds the main-thread proof, and hands the
/// session back with what `f` returned and the thread's id.
fn on_main<R: Send + 'static>(
    mut session: Session,
    f: impl FnOnce(&mut Session, MainThreadMarker) -> R + Send + 'static,
) -> (Session, R, ThreadId) {
    thread::spawn(move || {
        let marker = tutti_fixtures::main_thread();
        let result = f(&mut session, marker);
        (session, result, thread::current().id())
    })
    .join()
    .expect("the main-thread stand-in did not panic")
}

/// An update on a main thread has run: the session has one.
fn with_main_thread(session: Session) -> Session {
    on_main(session, |session, main| session.pool_main_stages(main)).0
}

#[test]
fn a_worker_stage_runs_on_the_spawners_thread_and_finishes_the_load() {
    let (mut s, spawned) = session();
    let log = Log::default();
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap();
    assert!(log.entries().is_empty(), "the stage waits for the spawner");
    assert_eq!(state(&s, "a").as_deref(), Some("loading"));
    let worker = thread::spawn({
        let spawned = spawned.clone();
        move || {
            spawned.run_all();
            thread::current().id()
        }
    })
    .join()
    .unwrap();
    assert_eq!(log.thread_of("worker a"), worker);
    assert_ne!(worker, thread::current().id());
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(made(&s, "a").unwrap().value, 1);
}

#[test]
fn a_worker_then_main_stage_waits_for_the_main_thread_then_runs_on_it_with_the_workers_output() {
    let (s, spawned) = session();
    let mut s = with_main_thread(s);
    let log = Log::default();
    s.pool(declare(&stub("a", How::Main, &log))).unwrap();
    spawned.run_all();
    assert!(events(&mut s).is_empty(), "no main thread in this update");
    assert_eq!(state(&s, "a").as_deref(), Some("waiting-for-main"));
    assert_eq!(log.entries(), ["worker a"], "the main stage has not run");
    assert!(matches!(
        made(&s, "a"),
        Err(HandleError::NotLoaded(
            crate::PoolState::WaitingForMainThread
        ))
    ));
    let (mut s, (), main) = on_main(s, |s, main| s.pool_main_stages(main));
    assert_eq!(
        log.thread_of("main a"),
        main,
        "the stage ran on the main thread"
    );
    assert_ne!(main, thread::current().id());
    assert_eq!(
        log.entries(),
        ["worker a", "main a", "drop output a"],
        "the output went to the main stage by value"
    );
    assert_eq!(log.thread_of("drop output a"), main);
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
}

#[test]
fn a_load_superseded_between_its_stages_drops_the_workers_output_on_the_main_thread_and_never_runs_its_main_stage()
 {
    let (s, spawned) = session();
    let mut s = with_main_thread(s);
    let log = Log::default();
    s.pool(declare(&stub("a", How::Main, &log))).unwrap();
    spawned.run_all();
    let _ = events(&mut s);
    assert_eq!(state(&s, "a").as_deref(), Some("waiting-for-main"));
    // Another identity: the load between its stages is superseded.
    let next = Stub {
        version: 2,
        ..stub("a", How::Worker, &log)
    };
    s.pool(declare(&next)).unwrap();
    assert_eq!(
        log.count("drop output a"),
        0,
        "not dropped where the supersede was"
    );
    let (mut s, (), main) = on_main(s, |s, main| s.pool_main_stages(main));
    assert_eq!(
        log.thread_of("drop output a"),
        main,
        "dropped on the main thread"
    );
    assert_eq!(log.count("main a"), 0, "the main stage never ran");
    spawned.run_all();
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(made(&s, "a").unwrap().value, 2, "the new load landed");
}

#[test]
fn a_load_cancelled_while_its_worker_runs_drops_the_workers_output_on_the_main_thread() {
    let (s, spawned) = session();
    let mut s = with_main_thread(s);
    let log = Log::default();
    s.pool(declare(&stub("a", How::Main, &log))).unwrap();
    // Undeclared and released while the worker has not run.
    s.pool::<&'static str>(()).unwrap();
    spawned.run_all();
    let _ = s.update(); // the worker's output lands on this thread, which is not main
    assert_eq!(log.entries(), ["worker a"]);
    assert_eq!(
        log.count("drop output a"),
        0,
        "the session thread never drops it"
    );
    let (_s, (), main) = on_main(s, |s, main| s.pool_main_stages(main));
    assert_eq!(log.thread_of("drop output a"), main);
    assert_eq!(log.count("main a"), 0);
}

#[test]
fn a_pool_with_a_main_stage_and_no_main_thread_is_refused_at_the_call_applying_nothing() {
    let (mut s, spawned) = session();
    let log = Log::default();
    let refused = s.pool((
        declare(&stub("plain", How::Worker, &log)),
        declare(&stub("plugin", How::Main, &log)),
    ));
    assert_eq!(refused, Err(PoolError::NeedsMainThread("plugin")));
    assert_eq!(state(&s, "plain"), None, "nothing of the set applied");
    assert!(spawned.take().is_empty(), "no load started");
    assert!(events(&mut s).is_empty());
    // A worker-only set is not refused.
    s.pool(declare(&stub("plain", How::Worker, &log))).unwrap();
}

#[test]
fn a_main_stage_is_accepted_once_an_update_has_run_on_a_main_thread() {
    let (s, _) = session();
    let mut s = with_main_thread(s);
    let log = Log::default();
    assert!(s.pool(declare(&stub("a", How::Main, &log))).is_ok());
}

#[test]
fn a_session_stage_runs_in_update_on_the_session_thread_with_no_worker() {
    let (mut s, spawned) = session();
    let log = Log::default();
    s.pool(declare(&stub("a", How::Session, &log))).unwrap();
    assert_eq!(state(&s, "a").as_deref(), Some("opening"));
    assert!(spawned.take().is_empty(), "a session stage has no worker");
    assert!(log.entries().is_empty(), "it runs in update, not in pool");
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(
        log.entries(),
        ["session a at 48000"],
        "given the session's rate"
    );
    assert_eq!(log.thread_of("session a at 48000"), thread::current().id());
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
}

#[test]
fn a_session_stage_that_panics_fails_its_entry_and_not_the_update() {
    let (mut s, _) = session();
    struct Panics;
    impl PoolEntry for Panics {
        type Identity = ();
        type Loaded = ();
        type Handle = ();
        fn identity(&self) {}
        fn load(&self) -> Load<()> {
            Load::on_session(|_| panic!("the port is gone"))
        }
        fn handle(_: &(), _: &Life) {}
    }
    s.pool(Declared::new("a", Panics)).unwrap();
    assert_eq!(
        events(&mut s),
        [(
            "a",
            "failed: a session job panicked: the port is gone".to_owned()
        )]
    );
}

#[test]
fn a_ready_load_lands_at_the_declaration_with_the_value_it_was_given() {
    struct Provided(Arc<Made>);
    impl PoolEntry for Provided {
        type Identity = ();
        type Loaded = Arc<Made>;
        type Handle = Arc<Made>;
        fn identity(&self) {}
        fn load(&self) -> Load<Arc<Made>> {
            Load::ready(Arc::clone(&self.0))
        }
        fn handle(loaded: &Arc<Made>, _: &Life) -> Arc<Made> {
            Arc::clone(loaded)
        }
    }
    let (mut s, spawned) = session();
    let given = Arc::new(Made {
        value: 7,
        lost: Arc::default(),
    });
    s.pool(Declared::new("a", Provided(Arc::clone(&given))))
        .unwrap();
    assert!(spawned.take().is_empty(), "no stage runs anywhere");
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"), "at once");
    assert!(Arc::ptr_eq(
        &s.pool.handle::<_, Provided>(&"a").unwrap(),
        &given
    ));
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
}

#[test]
fn at_most_the_configured_worker_stages_run_and_the_rest_start_in_declaration_order_as_they_land() {
    let log = Log::default();
    let spawned = Spawned::default();
    let held = spawned.clone();
    let mut s = Session::builder()
        .spawn_with(move |job| held.0.lock().unwrap().push(job))
        .build_detached(tutti_types::SampleRate::SR_48K)
        .unwrap();
    let names = ["a", "b", "c", "d", "e"];
    s.pool(names.map(|name| declare(&stub(name, How::Worker, &log))))
        .unwrap();
    let mut first = spawned.take();
    assert_eq!(first.len(), 4, "the default is four at once");
    assert_eq!(state(&s, "e").as_deref(), Some("loading"), "e waits");
    let a = first.remove(0);
    a.run();
    assert!(
        spawned.take().is_empty(),
        "nothing starts until a stage lands in an update"
    );
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    let next = spawned.take();
    assert_eq!(next.len(), 1, "a's landing starts the next in line");
    for job in next {
        job.run();
    }
    assert_eq!(
        log.entries(),
        ["worker a", "worker e"],
        "e, the fifth declared"
    );
    assert_eq!(first.len(), 3, "the other three have not run");
    for job in first {
        job.run();
    }
}

#[test]
fn the_parallel_cap_is_the_builders() {
    let log = Log::default();
    let spawned = Spawned::default();
    let held = spawned.clone();
    let mut s = Session::builder()
        .spawn_with(move |job| held.0.lock().unwrap().push(job))
        .pool_config(PoolConfig {
            parallel_loads: std::num::NonZeroUsize::new(2).unwrap(),
        })
        .build_detached(tutti_types::SampleRate::SR_48K)
        .unwrap();
    s.pool(["a", "b", "c"].map(|name| declare(&stub(name, How::Worker, &log))))
        .unwrap();
    assert_eq!(spawned.take().len(), 2);
    assert_eq!(PoolConfig::default().parallel_loads.get(), 4);
}

#[test]
fn a_queued_load_superseded_before_it_starts_never_runs() {
    let log = Log::default();
    let spawned = Spawned::default();
    let held = spawned.clone();
    let mut s = Session::builder()
        .spawn_with(move |job| held.0.lock().unwrap().push(job))
        .pool_config(PoolConfig {
            parallel_loads: std::num::NonZeroUsize::new(1).unwrap(),
        })
        .build_detached(tutti_types::SampleRate::SR_48K)
        .unwrap();
    s.pool((
        declare(&stub("a", How::Worker, &log)),
        declare(&stub("b", How::Worker, &log)),
    ))
    .unwrap();
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap(); // b undeclared
    for job in spawned.take() {
        job.run();
    }
    let _ = s.update();
    assert!(spawned.take().is_empty(), "b, released, is not started");
    assert_eq!(log.entries(), ["worker a"]);
}

#[test]
fn retry_loads_a_failed_entry_again_with_its_identity() {
    let (mut s, spawned) = session();
    let log = Log::default();
    let entry = stub("a", How::Worker, &log);
    entry.fail.store(true, Ordering::SeqCst);
    s.pool(declare(&entry)).unwrap();
    spawned.run_all();
    let _ = events(&mut s);
    assert_eq!(state(&s, "a").as_deref(), Some("failed"));
    entry.fail.store(false, Ordering::SeqCst);
    assert_eq!(s.retry(&"a"), Ok(()));
    assert_eq!(state(&s, "a").as_deref(), Some("loading"));
    spawned.run_all();
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(log.count("worker a"), 2, "loaded again");
    assert_eq!(made(&s, "a").unwrap().value, 1, "with the same identity");
}

#[test]
fn retry_reopens_a_loaded_entry_its_kind_reads_as_lost() {
    let (mut s, spawned) = session();
    let log = Log::default();
    let entry = stub("a", How::Worker, &log);
    s.pool(declare(&entry)).unwrap();
    spawned.run_all();
    let _ = events(&mut s);
    let before = made(&s, "a").unwrap();
    assert_eq!(
        s.retry(&"a"),
        Err(RetryError::NotFailed("a")),
        "loaded and present"
    );
    before.lost.store(true, Ordering::SeqCst);
    assert_eq!(s.retry(&"a"), Ok(()));
    assert_eq!(state(&s, "a").as_deref(), Some("reloading"));
    assert!(
        Arc::ptr_eq(&made(&s, "a").unwrap(), &before),
        "until it lands"
    );
    spawned.run_all();
    assert_eq!(events(&mut s), [("a", "reloaded".to_owned())]);
    let after = made(&s, "a").unwrap();
    assert!(!Arc::ptr_eq(&after, &before));
    assert_eq!(log.count("worker a"), 2);
}

#[test]
fn retry_names_the_key_it_refuses() {
    let (mut s, spawned) = session();
    let log = Log::default();
    assert_eq!(s.retry(&"nobody"), Err(RetryError::NotPooled("nobody")));
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap();
    assert_eq!(
        s.retry(&"a"),
        Err(RetryError::NotFailed("a")),
        "still loading"
    );
    spawned.run_all();
    let _ = events(&mut s);
    assert_eq!(s.retry(&"a"), Err(RetryError::NotFailed("a")), "loaded");
    s.pool::<&'static str>(()).unwrap();
    assert_eq!(
        s.retry(&"a"),
        Err(RetryError::NotPooled("a")),
        "released, so not declared"
    );
}

#[test]
fn pool_loaded_lands_once_every_declared_entry_has_an_outcome_and_reports_each() {
    let (mut s, spawned) = session();
    let log = Log::default();
    let bad = stub("b", How::Worker, &log);
    bad.fail.store(true, Ordering::SeqCst);
    s.pool((
        declare(&stub("a", How::Worker, &log)),
        declare(&bad),
        declare(&stub("c", How::Session, &log)),
    ))
    .unwrap();
    let loaded = s.pool_loaded::<&'static str>();
    assert!(loaded.take().is_none(), "nothing has an outcome");
    let mut jobs = spawned.take();
    jobs.remove(0).run();
    let _ = s.update();
    assert!(loaded.take().is_none(), "a and c landed, b has not");
    for job in jobs {
        job.run();
    }
    let _ = s.update();
    let report = loaded.take().expect("every entry has an outcome");
    let keys: Vec<_> = report.iter().map(|(key, _)| *key).collect();
    assert_eq!(keys, ["a", "b", "c"], "in declaration order");
    assert!(matches!(report.outcome(&"a"), Some(PoolOutcome::Loaded)));
    assert!(matches!(report.outcome(&"b"), Some(PoolOutcome::Failed(_))));
    assert!(matches!(report.outcome(&"c"), Some(PoolOutcome::Loaded)));
    assert!(!report.all_loaded());
    assert_eq!(
        report.failures().map(|(key, _)| *key).collect::<Vec<_>>(),
        ["b"]
    );
    assert!(loaded.take().is_none(), "it lands once");
}

#[test]
fn pool_loaded_wait_updates_until_it_lands_and_leaves_the_events_for_the_next_update() {
    let log = Log::default();
    let mut s = Session::builder()
        .spawn_with(|job| job.run())
        .build_detached(tutti_types::SampleRate::SR_48K)
        .unwrap();
    s.pool((
        declare(&stub("a", How::Worker, &log)),
        declare(&stub("b", How::Worker, &log)),
    ))
    .unwrap();
    let loaded = s.pool_loaded::<&'static str>();
    let report = loaded.wait(&mut s).unwrap();
    assert!(report.all_loaded());
    assert_eq!(report.len(), 2);
    assert_eq!(
        events(&mut s),
        [("a", "loaded".to_owned()), ("b", "loaded".to_owned())],
        "the waited updates' events are the next update's"
    );
    assert_eq!(loaded.wait(&mut s).unwrap_err(), PoolWaitError::Taken);
}

#[test]
fn pool_loaded_wait_off_the_main_thread_refuses_while_a_main_stage_waits() {
    let (s, spawned) = session();
    let mut s = with_main_thread(s);
    let log = Log::default();
    s.pool(declare(&stub("a", How::Main, &log))).unwrap();
    spawned.run_all();
    let loaded = s.pool_loaded::<&'static str>();
    assert_eq!(
        loaded.wait(&mut s).unwrap_err(),
        PoolWaitError::NeedsMainThread
    );
    let (mut s, (), _) = on_main(s, |s, main| s.pool_main_stages(main));
    let report = loaded.wait(&mut s).expect("still the caller's");
    assert!(report.all_loaded());
}

#[test]
fn an_empty_pool_reports_at_once() {
    let (mut s, _) = session();
    let loaded = s.pool_loaded::<&'static str>();
    assert!(loaded.take().expect("nothing is pending").is_empty());
}

#[test]
fn a_kind_that_shares_loads_once_for_two_keys_of_one_identity_and_both_read_the_same_value() {
    let (mut s, spawned) = session();
    let log = Log::default();
    let first = stub("a", How::Worker, &log);
    let second = Stub {
        name: "b",
        ..first.clone()
    };
    s.pool((Declared::shared("a", first), Declared::shared("b", second)))
        .unwrap();
    let jobs = spawned.take();
    assert_eq!(jobs.len(), 1, "one load");
    for job in jobs {
        job.run();
    }
    assert_eq!(
        events(&mut s),
        [("a", "loaded".to_owned()), ("b", "loaded".to_owned())]
    );
    assert_eq!(log.entries(), ["worker a"], "the stage ran once");
    assert!(Arc::ptr_eq(
        &made(&s, "a").unwrap(),
        &made(&s, "b").unwrap()
    ));
}

#[test]
fn a_key_of_a_sharing_kind_declared_after_the_load_landed_takes_the_loaded_value() {
    let (mut s, spawned) = session();
    let log = Log::default();
    let entry = stub("a", How::Worker, &log);
    s.pool(Declared::shared("a", entry.clone())).unwrap();
    spawned.run_all();
    let _ = events(&mut s);
    let again = Stub {
        name: "b",
        ..entry.clone()
    };
    s.pool((Declared::shared("a", entry), Declared::shared("b", again)))
        .unwrap();
    assert!(spawned.take().is_empty(), "no second load");
    assert_eq!(state(&s, "b").as_deref(), Some("loaded"), "at once");
    assert!(Arc::ptr_eq(
        &made(&s, "a").unwrap(),
        &made(&s, "b").unwrap()
    ));
    assert_eq!(log.count("worker a"), 1);
}

#[test]
fn a_shared_load_still_lands_for_the_key_that_remains_when_the_other_is_undeclared() {
    let (mut s, spawned) = session();
    let log = Log::default();
    let first = stub("a", How::Worker, &log);
    let second = Stub {
        name: "b",
        ..first.clone()
    };
    s.pool((
        Declared::shared("a", first),
        Declared::shared("b", second.clone()),
    ))
    .unwrap();
    s.pool(Declared::shared("b", second)).unwrap();
    spawned.run_all();
    assert_eq!(
        events(&mut s),
        [("b", "loaded".to_owned()), ("a", "released".to_owned())]
    );
    assert_eq!(made(&s, "b").unwrap().value, 1);
}

#[test]
fn a_kind_that_does_not_share_loads_once_per_key_even_with_one_identity() {
    let (mut s, spawned) = session();
    let log = Log::default();
    let first = stub("a", How::Worker, &log);
    let second = Stub {
        name: "b",
        ..first.clone()
    };
    s.pool((Declared::new("a", first), Declared::new("b", second)))
        .unwrap();
    let jobs = spawned.take();
    assert_eq!(jobs.len(), 2, "one instance per key");
    for job in jobs {
        job.run();
    }
    let _ = events(&mut s);
    assert_eq!(log.count("worker a") + log.count("worker b"), 2);
    assert!(
        !Arc::ptr_eq(&made(&s, "a").unwrap(), &made(&s, "b").unwrap()),
        "two instances"
    );
}

#[test]
fn a_worker_stage_whose_entry_was_released_still_frees_its_capacity() {
    let (mut s, spawned) = session();
    let log = Log::default();
    s.pool(["a", "b", "c", "d", "e"].map(|name| declare(&stub(name, How::Worker, &log))))
        .unwrap();
    s.pool::<&'static str>(()).unwrap();
    for job in spawned.take() {
        job.run();
    }
    let _ = s.update();
    s.pool(declare(&stub("z", How::Worker, &log))).unwrap();
    assert_eq!(spawned.take().len(), 1, "the four running stages ended");
}
