//! The load runner: each stage kind and its thread, a cancel between stages,
//! the parallel cap, retry, the report and the shared load. The jobs run when
//! a test says; the main thread is a thread the test makes one for.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};

use tutti_platform::MainThreadMarker;

use super::{Background, events, session, state, took};
use crate::{
    BackgroundError, Declared, HandleError, JobStatus, Life, Load, LoadError, PoolConfig,
    PoolEntry, PoolError, PoolOutcome, Presence, RetryError, Session,
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

/// What the background stage hands the main stage: it logs where it is dropped.
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
    type Handle = StubHandle;

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
                Err(LoadError::Job(BackgroundError::Dropped))
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

    fn handle(loaded: &Arc<Made>, _: &Life) -> StubHandle {
        StubHandle {
            made: Arc::clone(loaded),
        }
    }

    fn is_lost(loaded: &Arc<Made>) -> bool {
        loaded.lost.load(Ordering::SeqCst)
    }
}

/// The handle of a stub: its presence is read from the loaded value, as a peer
/// kind's is.
struct StubHandle {
    made: Arc<Made>,
}

impl StubHandle {
    fn presence(&self) -> Presence<&'static str> {
        if Stub::is_lost(&self.made) {
            Presence::Lost("gone")
        } else {
            Presence::Live
        }
    }
}

fn declare(stub: &Stub) -> Declared<&'static str, Stub> {
    Declared::new(stub.name, stub.clone())
}

fn made(
    session: &crate::DetachedSession,
    key: &'static str,
) -> Result<Arc<Made>, HandleError<&'static str>> {
    session
        .pool
        .entries
        .handle::<_, Stub>(&key)
        .map(|handle| handle.made)
}

/// Runs `f` on a thread that holds the main-thread proof, and hands the
/// session back with what `f` returned and the thread's id.
fn on_main<R: Send + 'static>(
    mut session: crate::DetachedSession,
    f: impl FnOnce(&mut crate::DetachedSession, MainThreadMarker) -> R + Send + 'static,
) -> (crate::DetachedSession, R, ThreadId) {
    thread::spawn(move || {
        let marker = tutti_fixtures::main_thread();
        let result = f(&mut session, marker);
        (session, result, thread::current().id())
    })
    .join()
    .expect("the main-thread stand-in did not panic")
}

/// An update on a main thread has run: the session has one.
fn with_main_thread(session: crate::DetachedSession) -> crate::DetachedSession {
    on_main(session, |session, main| session.pool_main_stages(main)).0
}

#[test]
fn a_worker_stage_runs_on_the_executors_thread_and_finishes_the_load() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap();
    assert!(log.entries().is_empty(), "the stage waits for the executor");
    assert_eq!(state(&s, "a").as_deref(), Some("loading"));
    let (worker, _bg) = thread::spawn(move || {
        bg.run_until_stalled();
        (thread::current().id(), bg)
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
    let (s, mut bg) = session();
    let mut s = with_main_thread(s);
    let log = Log::default();
    s.pool(declare(&stub("a", How::Main, &log))).unwrap();
    bg.run_until_stalled();
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
    let (s, mut bg) = session();
    let mut s = with_main_thread(s);
    let log = Log::default();
    s.pool(declare(&stub("a", How::Main, &log))).unwrap();
    bg.run_until_stalled();
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
    bg.run_until_stalled();
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(made(&s, "a").unwrap().value, 2, "the new load landed");
}

#[test]
fn a_load_cancelled_while_its_worker_runs_drops_the_workers_output_on_the_main_thread() {
    let (s, mut bg) = session();
    let mut s = with_main_thread(s);
    let log = Log::default();
    s.pool(declare(&stub("a", How::Main, &log))).unwrap();
    // Undeclared and released while the worker has not run.
    s.pool::<&'static str>(()).unwrap();
    bg.run_until_stalled();
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
    let (mut s, mut bg) = session();
    let log = Log::default();
    let refused = s.pool((
        declare(&stub("plain", How::Worker, &log)),
        declare(&stub("plugin", How::Main, &log)),
    ));
    assert_eq!(refused, Err(PoolError::NeedsMainThread("plugin")));
    assert_eq!(state(&s, "plain"), None, "nothing of the set applied");
    assert!(bg.run_until_stalled() == 0, "no load started");
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
    let (mut s, mut bg) = session();
    let log = Log::default();
    s.pool(declare(&stub("a", How::Session, &log))).unwrap();
    assert_eq!(state(&s, "a").as_deref(), Some("opening"));
    assert!(bg.run_until_stalled() == 0, "a session stage has no worker");
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
    let (mut s, mut bg) = session();
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
    bg.run_until_stalled();
    assert_eq!(
        events(&mut s),
        [(
            "a",
            "failed: background work panicked: the port is gone".to_owned()
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
    let (mut s, mut bg) = session();
    let given = Arc::new(Made {
        value: 7,
        lost: Arc::default(),
    });
    s.pool(Declared::new("a", Provided(Arc::clone(&given))))
        .unwrap();
    assert!(bg.run_until_stalled() == 0, "no stage runs anywhere");
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"), "at once");
    assert!(Arc::ptr_eq(
        &s.pool.entries.handle::<_, Provided>(&"a").unwrap(),
        &given
    ));
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
}

/// A session that runs one background stage at a time: the next starts when the last
/// one's landing is reported, so it lands in a later update.
fn one_at_a_time() -> (crate::DetachedSession, Background) {
    Session::builder()
        .pool_config(PoolConfig::new().with_parallel_loads(std::num::NonZeroUsize::new(1).unwrap()))
        .build_detached(tutti_units::SampleRate::SR_48K)
        .unwrap()
}

#[test]
fn at_most_the_configured_worker_stages_run_and_the_rest_start_in_declaration_order_as_they_land() {
    let log = Log::default();
    let (mut s, mut bg) = Session::builder()
        .build_detached(tutti_units::SampleRate::SR_48K)
        .unwrap();
    let names = ["a", "b", "c", "d", "e"];
    s.pool(names.map(|name| declare(&stub(name, How::Worker, &log))))
        .unwrap();
    assert_eq!(state(&s, "e").as_deref(), Some("loading"), "e waits");
    assert!(bg.try_tick(), "a, the first queued, runs");
    assert_eq!(
        log.entries(),
        ["worker a"],
        "nothing starts until a stage lands in an update"
    );
    // The drive runs the other three queued stages, which are all the default
    // allows at once, and the update lands the four.
    assert_eq!(bg.run_until_stalled(), 3, "the other three start together");
    assert_eq!(
        events(&mut s),
        ["a", "b", "c", "d"].map(|key| (key, "loaded".to_owned())),
        "the default is four at once"
    );
    assert_eq!(
        log.entries(),
        ["worker a", "worker b", "worker c", "worker d"]
    );
    assert_eq!(
        bg.run_until_stalled(),
        1,
        "the landings start the next in line"
    );
    assert_eq!(
        log.entries(),
        ["worker a", "worker b", "worker c", "worker d", "worker e"],
        "e, the fifth declared"
    );
}

#[test]
fn the_parallel_cap_is_the_builders() {
    let log = Log::default();
    let (mut s, mut bg) = Session::builder()
        .pool_config(PoolConfig::new().with_parallel_loads(std::num::NonZeroUsize::new(2).unwrap()))
        .build_detached(tutti_units::SampleRate::SR_48K)
        .unwrap();
    s.pool(["a", "b", "c"].map(|name| declare(&stub(name, How::Worker, &log))))
        .unwrap();
    assert_eq!(bg.run_until_stalled(), 2);
}

#[test]
fn a_queued_load_superseded_before_it_starts_never_runs() {
    let log = Log::default();
    let (mut s, mut bg) = Session::builder()
        .pool_config(PoolConfig::new().with_parallel_loads(std::num::NonZeroUsize::new(1).unwrap()))
        .build_detached(tutti_units::SampleRate::SR_48K)
        .unwrap();
    s.pool((
        declare(&stub("a", How::Worker, &log)),
        declare(&stub("b", How::Worker, &log)),
    ))
    .unwrap();
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap(); // b undeclared
    assert_eq!(bg.run_until_stalled(), 1, "only a started");
    let _ = s.update();
    assert!(bg.run_until_stalled() == 0, "b, released, is not started");
    assert_eq!(log.entries(), ["worker a"]);
}

#[test]
fn retry_loads_a_failed_entry_again_with_its_identity() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    let entry = stub("a", How::Worker, &log);
    entry.fail.store(true, Ordering::SeqCst);
    s.pool(declare(&entry)).unwrap();
    bg.run_until_stalled();
    let _ = events(&mut s);
    assert_eq!(state(&s, "a").as_deref(), Some("failed"));
    entry.fail.store(false, Ordering::SeqCst);
    assert_eq!(s.retry(&"a"), Ok(()));
    assert_eq!(state(&s, "a").as_deref(), Some("loading"));
    bg.run_until_stalled();
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(log.count("worker a"), 2, "loaded again");
    assert_eq!(made(&s, "a").unwrap().value, 1, "with the same identity");
}

#[test]
fn retry_reopens_a_loaded_entry_its_kind_reads_as_lost() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    let entry = stub("a", How::Worker, &log);
    s.pool(declare(&entry)).unwrap();
    bg.run_until_stalled();
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
    bg.run_until_stalled();
    assert_eq!(events(&mut s), [("a", "reloaded".to_owned())]);
    let after = made(&s, "a").unwrap();
    assert!(!Arc::ptr_eq(&after, &before));
    assert_eq!(log.count("worker a"), 2);
}

#[test]
fn retry_names_the_key_it_refuses() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    assert_eq!(s.retry(&"nobody"), Err(RetryError::NotPooled("nobody")));
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap();
    assert_eq!(
        s.retry(&"a"),
        Err(RetryError::NotFailed("a")),
        "still loading"
    );
    bg.run_until_stalled();
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
    let log = Log::default();
    let (mut s, mut bg) = one_at_a_time();
    let bad = stub("b", How::Worker, &log);
    bad.fail.store(true, Ordering::SeqCst);
    s.pool((
        declare(&stub("a", How::Worker, &log)),
        declare(&bad),
        declare(&stub("c", How::Session, &log)),
    ))
    .unwrap();
    let loaded = s.pool_loaded::<&'static str>();
    assert_ne!(loaded.status(), JobStatus::Done, "nothing has an outcome");
    bg.run_until_stalled();
    let mut events = s.update();
    assert!(
        took(&mut events, &loaded).is_none(),
        "a and c landed, b has not"
    );
    assert_eq!(state(&s, "a").as_deref(), Some("loaded"));
    assert_eq!(state(&s, "c").as_deref(), Some("loaded"));
    assert_eq!(state(&s, "b").as_deref(), Some("loading"));
    bg.run_until_stalled();
    let mut events = s.update();
    let report = took(&mut events, &loaded)
        .expect("every entry has an outcome")
        .unwrap();
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
    assert!(took(&mut events, &loaded).is_none(), "it lands once");
}

#[test]
fn pool_loaded_lands_in_the_update_that_settles_the_last_entry_and_a_dropped_one_ends_its_wait() {
    let log = Log::default();
    let (mut s, mut bg) = one_at_a_time();
    s.pool((
        declare(&stub("a", How::Worker, &log)),
        declare(&stub("b", How::Worker, &log)),
    ))
    .unwrap();
    let loaded = s.pool_loaded::<&'static str>();
    let dropped = s.pool_loaded::<&'static str>();
    assert_eq!(s.pool.entries.waiting(), 2);
    drop(dropped);
    // The update lands a and starts b.
    bg.run_until_stalled();
    let _ = s.update();
    assert_eq!(s.pool.entries.waiting(), 1, "a dropped handle's wait ended");
    assert_ne!(loaded.status(), JobStatus::Done);
    bg.run_until_stalled();
    let mut events = s.update();
    assert_eq!(
        loaded.status(),
        JobStatus::Done,
        "the update that landed the last entry settled it"
    );
    assert_eq!(s.pool.entries.waiting(), 0);
    assert!(
        took(&mut events, &loaded)
            .expect("landed")
            .unwrap()
            .all_loaded()
    );
}

#[test]
fn pool_loaded_waits_for_the_main_thread_while_an_entry_is_in_a_main_stage() {
    let (s, mut bg) = session();
    let mut s = with_main_thread(s);
    let log = Log::default();
    s.pool(declare(&stub("a", How::Main, &log))).unwrap();
    let job = s.pool_loaded::<&'static str>();
    bg.run_until_stalled();
    // This thread is not the process's main thread: the wait would never end.
    assert!(matches!(
        job.wait(&mut s, &mut bg),
        Err(crate::PoolJobError::Wait(crate::WaitError::NeedsMainThread))
    ));
    assert_eq!(job.status(), JobStatus::WaitingForMainThread);
}

#[test]
fn an_empty_pool_reports_at_once() {
    let (mut s, mut bg) = session();
    let loaded = s.pool_loaded::<&'static str>();
    assert!(
        loaded.wait(&mut s, &mut bg).unwrap().is_empty(),
        "nothing is pending"
    );
}

#[test]
fn a_kind_that_shares_loads_once_for_two_keys_of_one_identity_and_both_read_the_same_value() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    let first = stub("a", How::Worker, &log);
    let second = Stub {
        name: "b",
        ..first.clone()
    };
    s.pool((Declared::shared("a", first), Declared::shared("b", second)))
        .unwrap();
    assert_eq!(bg.run_until_stalled(), 1, "one load");
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
    let (mut s, mut bg) = session();
    let log = Log::default();
    let entry = stub("a", How::Worker, &log);
    s.pool(Declared::shared("a", entry.clone())).unwrap();
    bg.run_until_stalled();
    let _ = events(&mut s);
    let again = Stub {
        name: "b",
        ..entry.clone()
    };
    s.pool((Declared::shared("a", entry), Declared::shared("b", again)))
        .unwrap();
    assert!(bg.run_until_stalled() == 0, "no second load");
    assert_eq!(state(&s, "b").as_deref(), Some("loaded"), "at once");
    assert!(Arc::ptr_eq(
        &made(&s, "a").unwrap(),
        &made(&s, "b").unwrap()
    ));
    assert_eq!(log.count("worker a"), 1);
}

#[test]
fn a_shared_load_still_lands_for_the_key_that_remains_when_the_other_is_undeclared() {
    let (mut s, mut bg) = session();
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
    bg.run_until_stalled();
    assert_eq!(
        events(&mut s),
        [("b", "loaded".to_owned()), ("a", "released".to_owned())]
    );
    assert_eq!(made(&s, "b").unwrap().value, 1);
}

#[test]
fn a_kind_that_does_not_share_loads_once_per_key_even_with_one_identity() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    let first = stub("a", How::Worker, &log);
    let second = Stub {
        name: "b",
        ..first.clone()
    };
    s.pool((Declared::new("a", first), Declared::new("b", second)))
        .unwrap();
    assert_eq!(bg.run_until_stalled(), 2, "one instance per key");
    let _ = events(&mut s);
    assert_eq!(log.count("worker a") + log.count("worker b"), 2);
    assert!(
        !Arc::ptr_eq(&made(&s, "a").unwrap(), &made(&s, "b").unwrap()),
        "two instances"
    );
}

#[test]
fn a_worker_stage_whose_entry_was_released_still_frees_its_capacity() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    s.pool(["a", "b", "c", "d", "e"].map(|name| declare(&stub(name, How::Worker, &log))))
        .unwrap();
    s.pool::<&'static str>(()).unwrap();
    bg.run_until_stalled();
    let _ = s.update();
    s.pool(declare(&stub("z", How::Worker, &log))).unwrap();
    assert_eq!(bg.run_until_stalled(), 1, "the four running stages ended");
}

/// Worker outputs the pool leaked (and, with `plugin`, those a collector could
/// not run).
fn stranded_total() -> u64 {
    #[cfg(feature = "plugin")]
    {
        crate::stranded_loads() + tutti_plugin_types::MainWork::stranded()
    }
    #[cfg(not(feature = "plugin"))]
    {
        crate::stranded_loads()
    }
}

#[test]
fn a_handles_presence_reads_lost_exactly_when_the_kind_says_lost_and_retry_follows_it() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    let entry = stub("a", How::Worker, &log);
    s.pool(declare(&entry)).unwrap();
    bg.run_until_stalled();
    let _ = events(&mut s);
    let handle = s.pool.entries.handle::<_, Stub>(&"a").unwrap();
    assert_eq!(handle.presence(), Presence::Live);
    assert_eq!(s.retry(&"a"), Err(RetryError::NotFailed("a")));
    handle.made.lost.store(true, Ordering::SeqCst);
    assert_eq!(handle.presence(), Presence::Lost("gone"));
    assert_eq!(s.retry(&"a"), Ok(()), "lost for the pool as for the handle");
}

#[test]
fn only_an_entry_whose_own_load_has_a_main_stage_needs_the_main_thread() {
    let (mut s, _) = session();
    let log = Log::default();
    // One kind: the entry that loads on a worker (a plugin served out of
    // process) passes with no main thread, the one with a main stage does not.
    assert!(s.pool(declare(&stub("served", How::Worker, &log))).is_ok());
    assert_eq!(
        s.pool(declare(&stub("in-process", How::Main, &log))),
        Err(PoolError::NeedsMainThread("in-process"))
    );
    assert_eq!(state(&s, "in-process"), None);
}

#[test]
fn a_panic_in_a_main_stage_fails_its_entry_and_not_the_update() {
    struct Panics;
    impl PoolEntry for Panics {
        type Identity = ();
        type Loaded = ();
        type Handle = ();
        fn identity(&self) {}
        fn load(&self) -> Load<()> {
            Load::on_worker_then_main(|| Ok(()), |(), _main| panic!("the plugin aborted"))
        }
        fn handle(_: &(), _: &Life) {}
    }
    let (s, mut bg) = session();
    let mut s = with_main_thread(s);
    s.pool(Declared::new("a", Panics)).unwrap();
    bg.run_until_stalled();
    let _ = s.update();
    let (mut s, (), _) = on_main(s, |s, main| s.pool_main_stages(main));
    assert_eq!(
        events(&mut s),
        [(
            "a",
            "failed: background work panicked: the plugin aborted".to_owned()
        )]
    );
}

#[test]
fn a_key_joining_a_shared_load_already_waiting_for_the_main_thread_waits_with_it() {
    let (s, mut bg) = session();
    let mut s = with_main_thread(s);
    let log = Log::default();
    let first = stub("a", How::Main, &log);
    let second = Stub {
        name: "b",
        ..first.clone()
    };
    s.pool(Declared::shared("a", first.clone())).unwrap();
    bg.run_until_stalled();
    let _ = s.update();
    assert_eq!(state(&s, "a").as_deref(), Some("waiting-for-main"));
    s.pool((Declared::shared("a", first), Declared::shared("b", second)))
        .unwrap();
    assert_eq!(
        state(&s, "b").as_deref(),
        Some("waiting-for-main"),
        "joined it"
    );
    assert!(bg.run_until_stalled() == 0, "no second worker");
    let (mut s, (), _) = on_main(s, |s, main| s.pool_main_stages(main));
    assert_eq!(
        events(&mut s),
        [("a", "loaded".to_owned()), ("b", "loaded".to_owned())]
    );
    assert!(Arc::ptr_eq(
        &made(&s, "a").unwrap(),
        &made(&s, "b").unwrap()
    ));
    assert_eq!(log.count("main a"), 1, "the main stage ran once");
}

#[test]
fn a_shared_key_whose_identity_changes_mid_flight_leaves_the_shared_load_and_gets_its_own() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    let first = stub("a", How::Worker, &log);
    let second = Stub {
        name: "b",
        ..first.clone()
    };
    s.pool((
        Declared::shared("a", first.clone()),
        Declared::shared("b", second.clone()),
    ))
    .unwrap();
    let changed = Stub {
        version: 2,
        ..second
    };
    s.pool((Declared::shared("a", first), Declared::shared("b", changed)))
        .unwrap();
    assert_eq!(bg.run_until_stalled(), 2, "the shared load, and b's own");
    assert_eq!(
        events(&mut s),
        [("a", "loaded".to_owned()), ("b", "loaded".to_owned())]
    );
    assert_eq!(made(&s, "a").unwrap().value, 1);
    assert_eq!(made(&s, "b").unwrap().value, 2);
}

#[test]
fn a_loaded_value_its_kind_reads_as_lost_is_not_copied_to_a_key_that_shares_its_identity() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    let entry = stub("a", How::Worker, &log);
    s.pool(Declared::shared("a", entry.clone())).unwrap();
    bg.run_until_stalled();
    let _ = events(&mut s);
    entry.lost.store(true, Ordering::SeqCst);
    let again = Stub {
        name: "b",
        lost: Arc::default(),
        ..entry.clone()
    };
    s.pool((Declared::shared("a", entry), Declared::shared("b", again)))
        .unwrap();
    assert_eq!(bg.run_until_stalled(), 1, "b loads for itself");
}

/// A stage dropped off the main thread: its output must not drop there.
fn stage(log: &Log, defer: Option<crate::pool::entry::Defer>) -> crate::pool::entry::MainStage {
    crate::pool::entry::MainStage::new(
        Output {
            log: log.clone(),
            name: "a",
        },
        |_output, _main| Ok(1_u32),
    )
    .deferring(defer)
}

/// The count is process-wide and read as a difference: nextest runs each test in a
/// process of its own, which keeps the difference this test's.
#[test]
fn a_stage_dropped_off_the_main_thread_with_no_collector_leaks_its_output_and_counts_it() {
    let log = Log::default();
    let before = crate::stranded_loads();
    drop(stage(&log, None));
    assert_eq!(crate::stranded_loads(), before + 1);
    assert_eq!(log.count("drop output a"), 0, "not dropped here");
}

#[cfg(feature = "plugin")]
#[test]
fn a_stage_dropped_off_the_main_thread_hands_its_output_to_the_collector_which_drops_it_on_main() {
    let log = Log::default();
    let (work, handle) = tutti_plugin_types::MainWork::new();
    let defer: crate::pool::entry::Defer = Arc::new(move |discard| handle.defer(discard));
    drop(stage(&log, Some(defer)));
    assert_eq!(
        log.count("drop output a"),
        0,
        "not dropped where it was let go"
    );
    let main = thread::spawn(move || {
        let marker = tutti_fixtures::main_thread();
        assert_eq!(work.run(marker), 1);
        thread::current().id()
    })
    .join()
    .unwrap();
    assert_eq!(log.thread_of("drop output a"), main);
}

fn session_with_pending_output(queued: bool, log: &Log) -> crate::DetachedSession {
    let (s, mut bg) = session();
    let mut s = with_main_thread(s);
    s.pool(declare(&stub("a", How::Main, log))).unwrap();
    bg.run_until_stalled();
    if queued {
        let _ = s.update(); // the output lands in the pool's main queue
    }
    s
}

/// The count is process-wide and read as a difference: nextest runs each test in a
/// process of its own, which keeps the difference this test's.
#[test]
fn a_session_dropped_off_the_main_thread_leaks_the_output_waiting_in_the_pool_and_in_the_jobs_channel()
 {
    for queued in [true, false] {
        let log = Log::default();
        let s = session_with_pending_output(queued, &log);
        let before = stranded_total();
        drop(s);
        assert_eq!(stranded_total(), before + 1, "queued: {queued}");
        assert_eq!(log.count("drop output a"), 0, "queued: {queued}");
    }
}

#[test]
fn a_pending_entry_that_starts_to_share_does_not_leave_a_shared_flight_without_a_share() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    let entry = stub("a", How::Worker, &log);
    let sharer = Stub {
        name: "b",
        ..entry.clone()
    };
    s.pool(Declared::new("a", entry.clone())).unwrap();
    // The same identity again, now sharing, while its load is pending; then a
    // sibling joins it.
    s.pool(Declared::shared("a", entry.clone())).unwrap();
    s.pool((Declared::shared("a", entry), Declared::shared("b", sharer)))
        .unwrap();
    bg.run_until_stalled();
    assert_eq!(
        events(&mut s),
        [("a", "loaded".to_owned()), ("b", "loaded".to_owned())],
        "a lands for the load it now waits on, and b with it"
    );
    assert!(Arc::ptr_eq(
        &made(&s, "a").unwrap(),
        &made(&s, "b").unwrap()
    ));
}

/// What `when_loaded` runs saw: the entry's state and its pool events' order.
fn recorder() -> (
    Arc<Mutex<Vec<String>>>,
    impl Fn(&'static str) -> Box<dyn FnOnce(&mut crate::DetachedSession) + Send>,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let into = Arc::clone(&seen);
    let make = move |what: &'static str| -> Box<dyn FnOnce(&mut crate::DetachedSession) + Send> {
        let seen = Arc::clone(&into);
        Box::new(move |session: &mut crate::DetachedSession| {
            let stood = state(session, "a").unwrap_or_default();
            seen.lock().unwrap().push(format!("{what} while {stood}"));
        })
    };
    (seen, make)
}

#[test]
fn when_loaded_runs_once_in_the_update_the_entry_lands_in_the_order_asked() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap();
    let (seen, run) = recorder();
    s.when_loaded(&"a", run("first")).unwrap();
    s.when_loaded(&"a", run("second")).unwrap();
    assert!(seen.lock().unwrap().is_empty(), "the load has not landed");
    bg.run_until_stalled();
    assert_eq!(events(&mut s), [("a", "loaded".to_owned())]);
    assert_eq!(
        *seen.lock().unwrap(),
        ["first while loaded", "second while loaded"],
        "in the landing's update, over the loaded entry, in the order asked"
    );
    let _ = s.update();
    assert_eq!(seen.lock().unwrap().len(), 2, "each ran once");
}

#[test]
fn when_loaded_over_a_loaded_entry_runs_in_the_next_update() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap();
    bg.run_until_stalled();
    let _ = s.update();
    let (seen, run) = recorder();
    s.when_loaded(&"a", run("late")).unwrap();
    assert!(seen.lock().unwrap().is_empty(), "never inside the call");
    let _ = s.update();
    assert_eq!(*seen.lock().unwrap(), ["late while loaded"]);
}

#[test]
fn when_loaded_waits_through_a_failure_and_runs_when_a_retry_lands() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    let entry = stub("a", How::Worker, &log);
    entry.fail.store(true, Ordering::SeqCst);
    s.pool(declare(&entry)).unwrap();
    let (seen, run) = recorder();
    s.when_loaded(&"a", run("run")).unwrap();
    bg.run_until_stalled();
    let _ = s.update();
    assert_eq!(state(&s, "a").as_deref(), Some("failed"));
    assert!(
        seen.lock().unwrap().is_empty(),
        "a failed entry runs nothing"
    );
    entry.fail.store(false, Ordering::SeqCst);
    s.retry(&"a").unwrap();
    bg.run_until_stalled();
    let _ = s.update();
    assert_eq!(*seen.lock().unwrap(), ["run while loaded"]);
}

#[test]
fn when_loaded_is_dropped_when_its_entry_is_released() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap();
    let (seen, run) = recorder();
    s.when_loaded(&"a", run("run")).unwrap();
    s.pool::<&'static str>(()).unwrap();
    bg.run_until_stalled();
    let _ = s.update();
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap();
    bg.run_until_stalled();
    let _ = s.update();
    assert!(
        seen.lock().unwrap().is_empty(),
        "the run belonged to the entry released, not to the key declared after"
    );
}

#[test]
fn when_loaded_names_a_key_the_pool_does_not_declare_at_the_call() {
    let (mut s, _bg) = session();
    let (_, run) = recorder();
    assert_eq!(
        s.when_loaded(&"a", run("run")),
        Err(crate::WhenLoadedError::NotPooled("a"))
    );
}

#[test]
fn a_when_loaded_run_whose_entry_an_earlier_run_released_is_dropped() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    s.pool((
        declare(&stub("a", How::Worker, &log)),
        declare(&stub("b", How::Worker, &log)),
    ))
    .unwrap();
    bg.run_until_stalled();
    let _ = s.update();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let (first, second) = (Arc::clone(&ran), Arc::clone(&ran));
    s.when_loaded(&"a", move |session: &mut crate::DetachedSession| {
        first.lock().unwrap().push("a");
        session.pool::<&'static str>(()).unwrap();
    })
    .unwrap();
    s.when_loaded(&"b", move |_: &mut crate::DetachedSession| {
        second.lock().unwrap().push("b")
    })
    .unwrap();
    let _ = s.update();
    assert_eq!(*ran.lock().unwrap(), ["a"], "b was released by a's run");
}

#[test]
fn a_when_loaded_run_asked_inside_a_run_waits_for_the_next_update() {
    let (mut s, mut bg) = session();
    let log = Log::default();
    s.pool(declare(&stub("a", How::Worker, &log))).unwrap();
    bg.run_until_stalled();
    let _ = s.update();
    let count = Arc::new(Mutex::new(0));
    let inner = Arc::clone(&count);
    s.when_loaded(&"a", move |session: &mut crate::DetachedSession| {
        *inner.lock().unwrap() += 1;
        let again = Arc::clone(&inner);
        session
            .when_loaded(&"a", move |_: &mut crate::DetachedSession| {
                *again.lock().unwrap() += 10
            })
            .unwrap();
    })
    .unwrap();
    let _ = s.update();
    assert_eq!(*count.lock().unwrap(), 1);
    let _ = s.update();
    assert_eq!(*count.lock().unwrap(), 11);
}
