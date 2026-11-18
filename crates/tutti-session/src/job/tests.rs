use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tutti_units::SampleRate;

use super::*;
use crate::{DetachedSession, Session, SessionBuilder, Task};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum EchoError {
    #[error(transparent)]
    Worker(#[from] WorkerError),
    #[error(transparent)]
    Wait(#[from] WaitError),
}

job_kind!(Echo => u32, EchoError);

const LONG: Duration = Duration::from_secs(10);

fn session(builder: SessionBuilder) -> DetachedSession {
    builder.build_detached(SampleRate::SR_48K).unwrap()
}

/// A session whose tasks wait in the returned list, run by the test.
fn holding() -> (DetachedSession, Arc<Mutex<Vec<Task>>>) {
    let held: Arc<Mutex<Vec<Task>>> = Arc::default();
    let into = Arc::clone(&held);
    let s = session(Session::builder().spawn_with(move |task| into.lock().unwrap().push(task)));
    (s, held)
}

fn run_all(held: &Arc<Mutex<Vec<Task>>>) {
    let tasks: Vec<Task> = held.lock().unwrap().drain(..).collect();
    for task in tasks {
        task.run();
    }
}

fn echo(s: &mut DetachedSession, n: u32) -> Job<Echo> {
    s.jobs.submit_job(
        move |_| n,
        |_, done, ticket| ticket.land(done.map_err(EchoError::from)),
    )
}

fn landings(events: &[SessionEvent]) -> Vec<JobId> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Landed(landed) => Some(landed.id()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_running_job_reports_running_and_wait_returns_its_result() {
    let mut s = session(Session::builder());
    let (started, started_rx) = sync_channel::<()>(0);
    let (go, go_rx) = sync_channel::<()>(0);
    let job = s.jobs.submit_job(
        move |_| {
            started.send(()).unwrap();
            go_rx.recv().unwrap();
            7_u32
        },
        |_, done, ticket: Ticket<Echo>| ticket.land(done.map_err(EchoError::from)),
    );
    started_rx.recv_timeout(LONG).unwrap();
    assert_eq!(job.status(), JobStatus::Running);
    go.send(()).unwrap();
    assert_eq!(job.wait(&mut s), Ok(7));
    assert_eq!(job.status(), JobStatus::Done);
}

#[test]
fn a_job_is_pending_until_its_task_runs_and_done_once_it_ended() {
    let (mut s, held) = holding();
    let job = echo(&mut s, 1);
    assert_eq!(job.status(), JobStatus::Pending);
    run_all(&held);
    let _ = s.update();
    assert_eq!(job.status(), JobStatus::Done);
}

#[test]
fn wait_leaves_the_other_landings_for_the_next_update_in_order() {
    let (mut s, held) = holding();
    let (first, second, third) = (echo(&mut s, 1), echo(&mut s, 2), echo(&mut s, 3));
    run_all(&held);
    assert_eq!(second.wait(&mut s), Ok(2));
    let rest = s.update();
    assert_eq!(landings(&rest), [first.id(), third.id()]);
    assert_eq!(landings(&s.update()), [], "reported once");
}

#[test]
fn a_landing_is_taken_once_and_only_by_its_own_job() {
    let (mut s, held) = holding();
    let (a, b) = (echo(&mut s, 1), echo(&mut s, 2));
    run_all(&held);
    let mut events = s.update();
    let SessionEvent::Landed(landed) = &mut events[0] else {
        panic!("the first event is a landing");
    };
    assert_eq!(landed.take(&b), None, "another job's landing");
    assert_eq!(landed.take(&a), Some(Ok(1)));
    assert_eq!(landed.take(&a), None, "taken");
}

#[test]
fn a_dropped_job_lands_nothing() {
    let (mut s, held) = holding();
    let job = echo(&mut s, 1);
    drop(job);
    run_all(&held);
    assert_eq!(landings(&s.update()), []);
}

#[test]
fn a_worker_that_panicked_lands_panicked_and_wait_returns_it() {
    let mut s = session(Session::builder());
    let job = s.jobs.submit_job(
        |_| -> u32 { panic!("the file was gone") },
        |_, done, ticket: Ticket<Echo>| ticket.land(done.map_err(EchoError::from)),
    );
    assert_eq!(
        job.wait(&mut s),
        Err(EchoError::Worker(WorkerError::Panicked {
            message: "the file was gone".into()
        }))
    );
}

#[test]
fn wait_ends_when_the_spawner_drops_the_task() {
    let mut s = session(Session::builder().spawn_with(drop));
    let job = echo(&mut s, 1);
    assert_eq!(
        job.wait(&mut s),
        Err(EchoError::Worker(WorkerError::Dropped))
    );
}

#[test]
fn wait_ends_when_the_sessions_end_of_the_job_is_gone_unanswered() {
    let mut s = session(Session::builder());
    let (job, ticket) = s.jobs.new_job::<Echo>();
    drop(ticket);
    assert_eq!(
        job.wait(&mut s),
        Err(EchoError::Worker(WorkerError::Dropped))
    );
}

#[test]
fn a_job_waiting_for_the_main_thread_is_not_waited_for_off_it() {
    // A test thread is not the process's main thread.
    assert!(tutti_rt::MainThreadMarker::new().is_none());
    let mut s = session(Session::builder());
    let (job, ticket) = s.jobs.new_job::<Echo>();
    ticket.waiting_for_main_thread();
    assert_eq!(job.status(), JobStatus::WaitingForMainThread);
    assert_eq!(
        job.wait(&mut s),
        Err(EchoError::Wait(WaitError::NeedsMainThread))
    );
    // The job is still the caller's: it lands, and a wait then returns it.
    ticket.land(Ok(5));
    assert_eq!(job.wait(&mut s), Ok(5));
}

#[test]
fn dropping_the_session_returns_though_a_worker_is_stuck() {
    let mut s = session(Session::builder());
    let (started, started_rx) = sync_channel::<()>(0);
    let (release, release_rx) = sync_channel::<()>(0);
    let job = s.jobs.submit_job(
        move |_| {
            started.send(()).unwrap();
            let _ = release_rx.recv();
        },
        |_, _, ticket: Ticket<Unit>| ticket.land(Ok(())),
    );
    started_rx.recv_timeout(LONG).unwrap();
    // The worker stays stuck until `release` is sent below, so a drop that
    // waited for it would never signal.
    let (dropped, dropped_rx) = sync_channel::<()>(0);
    let dropper = std::thread::spawn(move || {
        drop(s);
        let _ = dropped.send(());
    });
    let returned = dropped_rx.recv_timeout(LONG);
    let _ = release.send(());
    returned.expect("drop returns though a worker is stuck");
    dropper.join().unwrap();
    drop(job);
}

job_kind!(Unit => (), EchoError);

#[test]
fn a_job_waited_for_while_another_lands_still_returns_the_other_later() {
    let (mut s, held) = holding();
    let (a, b) = (echo(&mut s, 1), echo(&mut s, 2));
    run_all(&held);
    assert_eq!(a.wait(&mut s), Ok(1));
    assert_eq!(b.wait(&mut s), Ok(2), "b's landing was held for it");
}

#[test]
fn a_wait_after_the_result_was_taken_answers_already_landed() {
    let (mut s, held) = holding();
    let (a, b) = (echo(&mut s, 1), echo(&mut s, 2));
    run_all(&held);
    assert_eq!(a.wait(&mut s), Ok(1));
    assert_eq!(
        a.wait(&mut s),
        Err(EchoError::Wait(WaitError::AlreadyLanded))
    );
    // The app took b's landing from the event itself.
    let mut events = s.update();
    let SessionEvent::Landed(landed) = &mut events[0] else {
        panic!("b's landing");
    };
    assert_eq!(landed.take(&b), Some(Ok(2)));
    assert_eq!(
        b.wait(&mut s),
        Err(EchoError::Wait(WaitError::AlreadyLanded))
    );
}

#[test]
fn ids_are_unique_across_sessions() {
    let (mut first, _held_a) = holding();
    let (mut second, _held_b) = holding();
    assert_ne!(echo(&mut first, 1).id(), echo(&mut second, 1).id());
}

#[test]
fn a_job_dropped_before_its_stage_runs_never_runs_it_and_lands_nothing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut s = session(Session::builder());
    let ran = Arc::new(AtomicBool::new(false));
    let stage = |ticket: Ticket<Echo>, ran: Arc<AtomicBool>| -> Box<dyn FnOnce()> {
        Box::new(move || {
            ticket.complete(|| {
                ran.store(true, Ordering::SeqCst);
                Ok(1)
            });
        })
    };
    // Dropped before the stage starts: the effect never runs.
    let (job, ticket) = s.jobs.new_job::<Echo>();
    let deferred = stage(ticket, Arc::clone(&ran));
    drop(job);
    deferred();
    assert!(!ran.load(Ordering::SeqCst), "the effect did not run");
    assert_eq!(landings(&s.update()), []);
    // Kept: it runs and lands.
    let (job, ticket) = s.jobs.new_job::<Echo>();
    stage(ticket, Arc::clone(&ran))();
    assert_eq!(job.wait(&mut s), Ok(1));
    assert!(ran.load(Ordering::SeqCst));
}

#[test]
fn a_job_dropped_after_its_landing_was_queued_takes_the_landing_with_it() {
    let mut s = session(Session::builder());
    let (job, ticket) = s.jobs.new_job::<Echo>();
    ticket.land(Ok(2));
    drop(job);
    assert_eq!(landings(&s.update()), [], "no orphan landing");
}

#[test]
fn a_save_job_dropped_before_its_worker_starts_does_the_round_trip_never() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (mut s, held) = holding();
    let saved = Arc::new(AtomicBool::new(false));
    let effect = Arc::clone(&saved);
    // The shape of `save_state`: the worker skips the round trip for a
    // cancelled job.
    let job: Job<Echo> = s.jobs.submit_job(
        move |cancel| {
            if cancel.is_cancelled() {
                return None;
            }
            effect.store(true, Ordering::SeqCst);
            Some(1_u32)
        },
        |_, done, ticket| ticket.land(done.map_err(EchoError::from).map(|n| n.unwrap_or(0))),
    );
    drop(job);
    run_all(&held);
    assert!(!saved.load(Ordering::SeqCst), "nothing was saved");
    assert_eq!(landings(&s.update()), []);
}
