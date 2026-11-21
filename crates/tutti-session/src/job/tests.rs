use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use tutti_units::SampleRate;

use super::*;
use crate::{DetachedSession, Session, SessionBuilder};
use tutti_platform::Executor;

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

/// A job already landed with `n`: its ticket lands at once, so the test does not wait on
/// a worker.
fn landed(s: &mut DetachedSession, n: u32) -> Job<Echo> {
    let (job, ticket) = s.jobs.new_job::<Echo>();
    ticket.land(Ok(n));
    job
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
            SessionEvent::JobDone(output) => Some(output.id()),
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
fn a_job_is_pending_until_its_ticket_lands_and_done_after() {
    let s = session(Session::builder());
    let (job, ticket) = s.jobs.new_job::<Echo>();
    assert_eq!(job.status(), JobStatus::Pending);
    ticket.land(Ok(1));
    assert_eq!(job.status(), JobStatus::Done);
}

#[test]
fn wait_leaves_the_other_landings_for_the_next_update_in_order() {
    let mut s = session(Session::builder());
    let (first, second, third) = (landed(&mut s, 1), landed(&mut s, 2), landed(&mut s, 3));
    assert_eq!(second.wait(&mut s), Ok(2));
    let rest = s.update();
    assert_eq!(landings(&rest), [first.id(), third.id()]);
    assert_eq!(landings(&s.update()), [], "reported once");
}

#[test]
fn a_landing_is_taken_once_and_only_by_its_own_job() {
    let mut s = session(Session::builder());
    let (a, b) = (landed(&mut s, 1), landed(&mut s, 2));
    let mut events = s.update();
    let SessionEvent::JobDone(output) = &mut events[0] else {
        panic!("the first event is a landing");
    };
    assert_eq!(output.take(&b), None, "another job's landing");
    assert_eq!(output.take(&a), Some(Ok(1)));
    assert_eq!(output.take(&a), None, "taken");
}

#[test]
fn a_dropped_job_lands_nothing() {
    let mut s = session(Session::builder());
    let (job, ticket) = s.jobs.new_job::<Echo>();
    drop(job);
    assert!(ticket.cancelled());
    ticket.land(Ok(1));
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
fn a_job_waited_for_while_another_lands_still_returns_the_other_later() {
    let mut s = session(Session::builder());
    let (a, b) = (landed(&mut s, 1), landed(&mut s, 2));
    assert_eq!(a.wait(&mut s), Ok(1));
    assert_eq!(b.wait(&mut s), Ok(2), "b's landing was held for it");
}

#[test]
fn a_wait_after_the_result_was_taken_answers_already_landed() {
    let mut s = session(Session::builder());
    let (a, b) = (landed(&mut s, 1), landed(&mut s, 2));
    assert_eq!(a.wait(&mut s), Ok(1));
    assert_eq!(
        a.wait(&mut s),
        Err(EchoError::Wait(WaitError::AlreadyLanded))
    );
    // The app took b's landing from the event itself.
    let mut events = s.update();
    let SessionEvent::JobDone(output) = &mut events[0] else {
        panic!("b's landing");
    };
    assert_eq!(output.take(&b), Some(Ok(2)));
    assert_eq!(
        b.wait(&mut s),
        Err(EchoError::Wait(WaitError::AlreadyLanded))
    );
}

#[test]
fn ids_are_unique_across_sessions() {
    let mut first = session(Session::builder());
    let mut second = session(Session::builder());
    assert_ne!(echo(&mut first, 1).id(), echo(&mut second, 1).id());
}

#[test]
fn a_job_dropped_before_its_stage_runs_never_runs_it_and_lands_nothing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut s = session(Session::builder());
    let ran = Arc::new(AtomicBool::new(false));
    let stage = |ticket: Ticket<Echo>, ran: Arc<AtomicBool>| -> Box<dyn FnOnce()> {
        Box::new(move || {
            if !ticket.cancelled() {
                ran.store(true, Ordering::SeqCst);
                ticket.land(Ok(1));
            }
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
fn an_awaited_job_resolves_in_the_poll_after_the_update_that_found_it_landed() {
    let executor = Executor::new();
    let mut s = session(Session::builder());
    let (job, ticket) = s.jobs.new_job::<Echo>();
    let got = Arc::new(Mutex::new(None));
    let into = Arc::clone(&got);
    executor
        .spawn(async move {
            *into.lock().unwrap() = Some(job.await);
        })
        .detach();
    assert_eq!(
        executor.run_until_stalled(),
        1,
        "the poll registers its waker"
    );
    ticket.land(Ok(7));
    let events = s.update();
    assert!(got.lock().unwrap().is_none(), "the update only landed it");
    assert!(
        landings(&events).is_empty(),
        "an awaited job's landing is the future's, not an event"
    );
    assert_eq!(
        executor.run_until_stalled(),
        1,
        "the landing woke the awaiting future"
    );
    assert_eq!(got.lock().unwrap().take(), Some(Ok(7)));
}

#[test]
fn an_awaited_job_whose_landing_the_collect_phase_reported_answers_already_landed() {
    let executor = Executor::new();
    let mut s = session(Session::builder());
    let job = landed(&mut s, 7);
    let _events = s.update();
    let got = Arc::new(Mutex::new(None));
    let into = Arc::clone(&got);
    executor
        .spawn(async move {
            *into.lock().unwrap() = Some(job.await);
        })
        .detach();
    assert_eq!(executor.run_until_stalled(), 1);
    assert_eq!(
        got.lock().unwrap().take(),
        Some(Err(EchoError::Wait(WaitError::AlreadyLanded)))
    );
}
