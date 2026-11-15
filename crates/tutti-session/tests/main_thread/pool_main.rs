//! A pool load's worker output, which only the main thread may drop, when its
//! session ends: on the main thread it is dropped there; anywhere else it is
//! handed to the session's main-work collector, which ends off the main thread
//! too and leaks it, counted. The target runs on the process's main thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};

use tutti_plugin_types::MainWork;
use tutti_session::{Declared, Job, Life, Load, PoolEntry, Session};
use tutti_types::SampleRate;

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
fn session_with_output() -> (Session, Arc<Mutex<Option<ThreadId>>>, Arc<AtomicBool>) {
    let held: Arc<Mutex<Vec<Job>>> = Arc::default();
    let spawned = Arc::clone(&held);
    let mut session = Session::builder()
        .spawn_with(move |job| spawned.lock().unwrap().push(job))
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
    for job in held.lock().unwrap().drain(..) {
        job.run();
    }
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

pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["pool_main";
a_session_that_ends_on_the_main_thread_drops_the_workers_output_there,
a_session_that_ends_off_the_main_thread_leaks_the_workers_output_and_counts_it,
];
