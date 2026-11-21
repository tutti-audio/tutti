use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Wake;
use std::time::Duration;

/// A waker that counts how often it is woken.
struct Count(AtomicUsize);

impl Wake for Count {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting() -> (Arc<Count>, Waker) {
    let count = Arc::new(Count(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&count));
    (count, waker)
}

fn woken(count: &Count) -> usize {
    count.0.load(Ordering::SeqCst)
}

fn poll_with(start: &mut Start, waker: &Waker) -> Poll<Result<Started, MidiHardwareError>> {
    Pin::new(start).poll(&mut Context::from_waker(waker))
}

fn error() -> Arc<MidiHardwareError> {
    Arc::new(MidiHardwareError::DescriptorBroken)
}

fn gave_up(state: &HubState) -> Option<Instant> {
    match state {
        HubState::Starting { gave_up, .. } | HubState::Failed { gave_up, .. } => *gave_up,
        HubState::Ready => None,
    }
}

#[test]
fn the_default_start_of_a_backend_is_ready_on_the_first_poll() {
    let (backend, _) = crate::test_support::FakeBackend::build();
    let (_, waker) = counting();
    assert!(matches!(
        poll_with(&mut backend.start(), &waker),
        Poll::Ready(Ok(_))
    ));
}

#[test]
fn a_start_over_a_gate_that_goes_ready_wakes_its_waker_and_then_resolves() {
    let gate = Arc::new(StartGate::new());
    let mut start = Start::over(Arc::clone(&gate), gate.begin());
    let (count, waker) = counting();
    assert!(poll_with(&mut start, &waker).is_pending());
    assert_eq!(woken(&count), 0);
    gate.publish_ready();
    assert_eq!(woken(&count), 1);
    assert!(matches!(poll_with(&mut start, &waker), Poll::Ready(Ok(_))));
}

#[test]
fn a_waker_polled_twice_is_stored_once() {
    let gate = Arc::new(StartGate::new());
    let mut start = Start::over(Arc::clone(&gate), gate.begin());
    let (count, waker) = counting();
    assert!(poll_with(&mut start, &waker).is_pending());
    assert!(poll_with(&mut start, &waker).is_pending());
    gate.publish_ready();
    assert_eq!(woken(&count), 1);
}

#[test]
fn a_failed_start_wakes_its_waker_and_resolves_to_the_error() {
    let gate = Arc::new(StartGate::new());
    let mut start = Start::over(Arc::clone(&gate), gate.begin());
    let (count, waker) = counting();
    assert!(poll_with(&mut start, &waker).is_pending());
    gate.publish_failed(error(), None);
    assert_eq!(woken(&count), 1);
    let Poll::Ready(Err(MidiHardwareError::BackendFailed(why))) = poll_with(&mut start, &waker)
    else {
        panic!("the start failed");
    };
    assert!(matches!(*why, MidiHardwareError::DescriptorBroken));
}

#[test]
fn a_thread_that_ends_before_answering_fails_the_start_and_wakes_its_waker() {
    let gate = Arc::new(StartGate::new());
    let mut start = Start::over(Arc::clone(&gate), gate.begin());
    let (count, waker) = counting();
    assert!(poll_with(&mut start, &waker).is_pending());
    gate.ended();
    assert_eq!(woken(&count), 1);
    let Poll::Ready(Err(MidiHardwareError::BackendFailed(why))) = poll_with(&mut start, &waker)
    else {
        panic!("the start failed");
    };
    assert!(matches!(
        *why,
        MidiHardwareError::ThreadEnded {
            thread: MidiThread::DeviceWatch
        }
    ));
}

#[test]
fn a_thread_that_ends_after_the_client_is_made_changes_nothing() {
    let mut state = HubState::new();
    assert!(state.ready());
    assert!(!state.ended());
    assert!(matches!(state, HubState::Ready));
}

#[test]
fn ready_is_terminal() {
    let mut state = HubState::new();
    assert!(state.ready());
    assert!(!state.failed(error(), None));
    assert!(!state.ready());
    assert_eq!(
        state.begin(),
        Begun {
            generation: 0,
            restarted: false
        }
    );
    assert!(matches!(state, HubState::Ready));
    assert!(state.outcome(5).is_some_and(|outcome| outcome.is_ok()));
}

#[test]
fn a_failed_start_is_not_made_ready_by_a_late_client() {
    let mut state = HubState::new();
    assert!(state.failed(error(), None));
    assert!(!state.ready());
    assert!(matches!(state, HubState::Failed { .. }));
}

#[test]
fn a_future_made_while_a_start_is_under_way_joins_it() {
    let mut state = HubState::new();
    let first = state.begin();
    let second = state.begin();
    assert_eq!(first, second);
    assert!(!first.restarted);
    assert_eq!(state.wanted(None), Some((0, None)));
    assert_eq!(
        state.wanted(Some(0)),
        None,
        "a start the thread ran is not wanted again"
    );
}

#[test]
fn a_future_made_after_a_failure_restarts_with_the_next_generation() {
    let mut state = HubState::new();
    let first = state.begin();
    assert!(state.failed(error(), None));
    assert!(state.outcome(first.generation).is_some_and(|o| o.is_err()));
    let second = state.begin();
    assert_eq!(
        second,
        Begun {
            generation: first.generation + 1,
            restarted: true
        }
    );
    assert_eq!(state.wanted(Some(0)), Some((1, None)));
    // Two futures after one failure ask for one restart.
    assert!(!state.begin().restarted);
}

#[test]
fn a_failure_resolves_the_futures_of_its_own_and_earlier_starts_and_not_a_later_one() {
    let mut state = HubState::new();
    state.begin();
    state.failed(error(), None);
    state.begin();
    assert!(state.outcome(0).is_none(), "the restart is under way");
    assert!(state.outcome(1).is_none());
    state.failed(error(), None);
    assert!(state.outcome(0).is_some_and(|o| o.is_err()));
    assert!(state.outcome(1).is_some_and(|o| o.is_err()));
    assert!(
        state.outcome(2).is_none(),
        "a future of a start not yet made waits"
    );
}

#[test]
fn the_window_a_start_ended_is_carried_into_the_restart_and_a_later_failure_keeps_it() {
    let at = Instant::now();
    let mut state = HubState::new();
    state.begin();
    assert_eq!(gave_up(&state), None);
    state.failed(error(), Some(at));
    state.begin();
    assert_eq!(state.wanted(Some(0)), Some((1, Some(at))));
    // A start that made its one attempt inside the window passes the window on.
    state.failed(error(), Some(at));
    state.begin();
    assert_eq!(gave_up(&state), Some(at));
    // A thread that ended carries it too.
    assert!(state.ended());
    assert_eq!(gave_up(&state), Some(at));
}

#[test]
fn a_start_failed_before_it_could_wait_resolves_to_the_error() {
    let mut start = Start::failed(MidiHardwareError::DescriptorBroken);
    let (_, waker) = counting();
    assert!(matches!(
        poll_with(&mut start, &waker),
        Poll::Ready(Err(MidiHardwareError::BackendFailed(_)))
    ));
}

/// A start that another thread publishes wakes the thread waiting on it. The bound
/// only guards a hang: the test asserts that the wait completed.
#[test]
fn a_gate_used_across_threads_wakes_the_waiting_thread() {
    let gate = Arc::new(StartGate::new());
    let start = Start::over(Arc::clone(&gate), gate.begin());
    let (done, completed) = std::sync::mpsc::channel();
    let (waiting, is_waiting) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let mut start = start;
        let mut said = false;
        // Says it is waiting once its first poll has stored the waker.
        let polled = std::future::poll_fn(move |cx| {
            let polled = Pin::new(&mut start).poll(cx);
            if polled.is_pending() && !std::mem::replace(&mut said, true) {
                let _ = waiting.send(());
            }
            polled
        });
        let _ = done.send(tutti_fixtures::block_on(polled).is_ok());
    });
    let publisher = {
        let gate = Arc::clone(&gate);
        std::thread::spawn(move || {
            is_waiting.recv().expect("the waiter polled");
            gate.publish_ready();
        })
    };
    assert_eq!(
        completed.recv_timeout(Duration::from_secs(30)),
        Ok(true),
        "the waiting thread was never woken"
    );
    publisher.join().unwrap();
    waiter.join().unwrap();
}

#[test]
fn a_gate_tells_its_thread_of_a_restart_only_after_a_future_asks_for_one() {
    let gate = StartGate::new();
    assert_eq!(gate.wanted(None).map(|(generation, _)| generation), Some(0));
    assert_eq!(gate.wanted(Some(0)), None);
    gate.begin();
    gate.publish_failed(error(), None);
    assert!(gate.failure().is_some());
    assert_eq!(
        gate.wanted(Some(0)),
        None,
        "a failed start is not restarted by itself"
    );
    assert!(gate.begin().restarted);
    assert!(gate.failure().is_none());
    assert_eq!(
        gate.wanted(Some(0)).map(|(generation, _)| generation),
        Some(1)
    );
}
