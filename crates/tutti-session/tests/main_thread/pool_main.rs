//! A pool load's worker output, which only the main thread may drop, when its
//! session ends: on the main thread it is dropped there; anywhere else it is
//! handed to the session's main-work collector, which ends off the main thread
//! too and leaks it, counted. The target runs on the process's main thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};

use tutti_plugin_types::MainWork;
use tutti_session::{Declared, DetachedSession, Life, Load, PoolEntry, RetryError, Session};
use tutti_units::SampleRate;

/// What the worker hands the main stage: it says where it is dropped.
struct Output {
    dropped_on: Arc<Mutex<Option<ThreadId>>>,
    dropped: Arc<AtomicBool>,
}

impl Drop for Output {
    fn drop(&mut self) {
        *self.dropped_on.lock().unwrap() = Some(thread::current().id());
        self.dropped.store(true, Ordering::SeqCst);
    }
}

struct InProcess {
    dropped_on: Arc<Mutex<Option<ThreadId>>>,
    dropped: Arc<AtomicBool>,
}

impl PoolEntry for InProcess {
    type Identity = ();
    type Loaded = ();
    type Handle = ();

    fn identity(&self) {}

    fn load(&self) -> Load<()> {
        let (dropped_on, dropped) = (Arc::clone(&self.dropped_on), Arc::clone(&self.dropped));
        Load::on_worker_then_main(
            move || {
                Ok(Output {
                    dropped_on,
                    dropped,
                })
            },
            |_output, _main| Ok(()),
        )
    }

    fn handle(_: &(), _: &Life) {}
}

/// A session whose worker has run and whose update has not: the output is in
/// the jobs' channel, with its main stage.
fn session_with_output() -> (
    DetachedSession,
    Arc<Mutex<Option<ThreadId>>>,
    Arc<AtomicBool>,
) {
    let executor = tutti_platform::Executor::polled();
    let mut session = Session::builder()
        .executor(executor.clone())
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    let dropped_on = Arc::default();
    let dropped = Arc::new(AtomicBool::new(false));
    session
        .pool(Declared::new(
            "a",
            InProcess {
                dropped_on: Arc::clone(&dropped_on),
                dropped: Arc::clone(&dropped),
            },
        ))
        .expect("this is the main thread");
    executor.poll(usize::MAX);
    (session, dropped_on, dropped)
}

fn a_session_that_ends_on_the_main_thread_drops_the_workers_output_there() {
    let (session, dropped_on, dropped) = session_with_output();
    assert!(
        !dropped.load(Ordering::SeqCst),
        "held until the session ends"
    );
    drop(session);
    assert!(dropped.load(Ordering::SeqCst), "dropped, not leaked");
    assert_eq!(*dropped_on.lock().unwrap(), Some(thread::current().id()));
}

fn a_session_that_ends_off_the_main_thread_leaks_the_workers_output_and_counts_it() {
    let (session, _, dropped) = session_with_output();
    let before = MainWork::stranded();
    thread::spawn(move || drop(session)).join().unwrap();
    assert!(
        !dropped.load(Ordering::SeqCst),
        "never dropped off the main thread"
    );
    assert_eq!(MainWork::stranded(), before + 1, "and counted");
}

/// An entry of one kind whose load is in process (a main stage) or served
/// (a worker only), by the entry.
struct Dual {
    main: bool,
    fail: Arc<AtomicBool>,
}

impl PoolEntry for Dual {
    type Identity = ();
    type Loaded = Arc<u32>;
    type Handle = Arc<u32>;

    fn identity(&self) {}

    fn load(&self) -> Load<Arc<u32>> {
        let fail = Arc::clone(&self.fail);
        let made = move || {
            if fail.load(Ordering::SeqCst) {
                Err(tutti_session::LoadError::Job(
                    tutti_session::WorkerError::Dropped,
                ))
            } else {
                Ok(Arc::new(1))
            }
        };
        if self.main {
            let after = made.clone();
            Load::on_worker_then_main(move || made().map(|_| ()), move |(), _main| after())
        } else {
            Load::on_worker(made)
        }
    }

    fn handle(loaded: &Arc<u32>, _: &Life) -> Arc<u32> {
        Arc::clone(loaded)
    }
}

fn held_session() -> (DetachedSession, tutti_platform::Executor) {
    let executor = tutti_platform::Executor::polled();
    let session = Session::builder()
        .executor(executor.clone())
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    (session, executor)
}

fn run_held(executor: &tutti_platform::Executor) -> usize {
    executor.poll(usize::MAX)
}

fn retry_of_a_main_stage_load_is_refused_off_the_main_thread_until_an_update_ran_on_it() {
    let (mut session, executor) = held_session();
    let fail = Arc::new(AtomicBool::new(true));
    let entry = |fail: &Arc<AtomicBool>| {
        Declared::new(
            "a",
            Dual {
                main: true,
                fail: Arc::clone(fail),
            },
        )
    };
    session.pool(entry(&fail)).expect("on the main thread");
    run_held(&executor);
    // The failure lands in an update that is not on the main thread.
    let mut session = thread::spawn(move || {
        let _ = session.update();
        assert_eq!(session.retry(&"a"), Err(RetryError::NeedsMainThread("a")));
        session
    })
    .join()
    .unwrap();
    fail.store(false, Ordering::SeqCst);
    assert_eq!(session.retry(&"a"), Ok(()), "this is the main thread");
}

fn a_key_whose_load_needs_no_main_thread_does_not_join_a_flight_that_does_off_the_main_thread() {
    let (mut session, executor) = held_session();
    let fail = Arc::new(AtomicBool::new(false));
    let served = move |name, main| {
        Declared::shared(
            name,
            Dual {
                main,
                fail: Arc::clone(&fail),
            },
        )
    };
    session.pool(served("a", true)).expect("on the main thread");
    assert_eq!(run_held(&executor), 1, "a's load started");
    // On another thread, a is declared again in its served form and b joins in
    // that form: a's flight waits for a main thread this call has not got.
    let again = served.clone();
    let mut session = thread::spawn(move || {
        session
            .pool((again("a", false), again("b", false)))
            .expect("neither load has a main stage");
        session
    })
    .join()
    .unwrap();
    assert_eq!(run_held(&executor), 1, "b started its own load");
    // On the main thread b joins a's flight instead.
    session
        .pool((served("a", false), served("b", false), served("c", false)))
        .unwrap();
    assert_eq!(run_held(&executor), 0, "c joined a flight");
}

pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["pool_main";
a_session_that_ends_on_the_main_thread_drops_the_workers_output_there,
a_session_that_ends_off_the_main_thread_leaks_the_workers_output_and_counts_it,
retry_of_a_main_stage_load_is_refused_off_the_main_thread_until_an_update_ran_on_it,
a_key_whose_load_needs_no_main_thread_does_not_join_a_flight_that_does_off_the_main_thread,
];
