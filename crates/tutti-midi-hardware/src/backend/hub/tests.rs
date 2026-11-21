use super::*;
use crate::start::Started;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Wake};
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

fn poll_with(start: &mut Start, waker: &Waker) -> Poll<Result<Started, crate::StartFailed>> {
    Pin::new(start).poll(&mut Context::from_waker(waker))
}

/// Makes a future's round, waking whatever the call took.
fn begin(gate: &StartGate) -> Round {
    let (round, wakers) = gate.begin(false);
    wakers.wake();
    round
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
fn a_start_over_a_gate_that_goes_ready_wakes_its_waker_and_then_resolves() {
    let gate = Arc::new(StartGate::new());
    let mut start = gate.start(begin(&gate));
    let (count, waker) = counting();
    assert!(poll_with(&mut start, &waker).is_pending());
    assert_eq!(woken(&count), 0);
    gate.publish_ready().wake();
    assert_eq!(woken(&count), 1);
    assert!(matches!(poll_with(&mut start, &waker), Poll::Ready(Ok(_))));
}

#[test]
fn a_waker_polled_twice_is_stored_once() {
    let gate = Arc::new(StartGate::new());
    let mut start = gate.start(begin(&gate));
    let (count, waker) = counting();
    assert!(poll_with(&mut start, &waker).is_pending());
    assert!(poll_with(&mut start, &waker).is_pending());
    gate.publish_ready().wake();
    assert_eq!(woken(&count), 1);
}

#[test]
fn a_failed_start_wakes_its_waker_and_resolves_to_the_error() {
    let gate = Arc::new(StartGate::new());
    let mut start = gate.start(begin(&gate));
    let (count, waker) = counting();
    assert!(poll_with(&mut start, &waker).is_pending());
    gate.publish_failed(error(), None).wake();
    assert_eq!(woken(&count), 1);
    let Poll::Ready(Err(why)) = poll_with(&mut start, &waker) else {
        panic!("the start failed");
    };
    assert!(matches!(why.cause(), MidiHardwareError::DescriptorBroken));
}

#[test]
fn a_thread_that_ends_before_answering_fails_the_start_and_wakes_its_waker() {
    let gate = Arc::new(StartGate::new());
    let mut start = gate.start(begin(&gate));
    let (count, waker) = counting();
    assert!(poll_with(&mut start, &waker).is_pending());
    gate.ended(false).wake();
    assert_eq!(woken(&count), 1);
    let Poll::Ready(Err(why)) = poll_with(&mut start, &waker) else {
        panic!("the start failed");
    };
    assert!(matches!(
        why.cause(),
        MidiHardwareError::ThreadEnded {
            thread: MidiThread::DeviceWatch
        }
    ));
}

/// **Every future waiting on one failed start gets the one cause**: both resolve to
/// the same error value, not a copy each.
#[test]
fn every_waiter_of_a_failed_start_shares_its_cause() {
    let gate = Arc::new(StartGate::new());
    let mut first = gate.start(begin(&gate));
    let mut second = gate.start(begin(&gate));
    let (_, waker) = counting();
    assert!(poll_with(&mut first, &waker).is_pending());
    assert!(poll_with(&mut second, &waker).is_pending());
    let _ = gate.publish_failed(error(), None);
    let (Poll::Ready(Err(first)), Poll::Ready(Err(second))) = (
        poll_with(&mut first, &waker),
        poll_with(&mut second, &waker),
    ) else {
        panic!("both failed");
    };
    assert!(std::ptr::eq(first.cause(), second.cause()));
}

#[test]
fn a_thread_that_ends_after_the_client_is_made_changes_nothing() {
    let mut state = HubState::new();
    assert!(state.ready());
    assert!(!state.ended(false));
    assert!(matches!(state, HubState::Ready));
}

#[test]
fn ready_is_terminal() {
    let mut state = HubState::new();
    assert!(state.ready());
    assert!(!state.failed(error(), None));
    assert!(!state.ready());
    assert_eq!(
        state.begin(false).0,
        Round {
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
    let first = state.begin(false).0;
    let second = state.begin(false).0;
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
    let first = state.begin(false).0;
    assert!(state.failed(error(), None));
    assert!(state.outcome(first.generation).is_some_and(|o| o.is_err()));
    let second = state.begin(false).0;
    assert_eq!(
        second,
        Round {
            generation: first.generation + 1,
            restarted: true
        }
    );
    assert_eq!(state.wanted(Some(0)), Some((1, None)));
    // Two futures after one failure ask for one restart.
    assert!(!state.begin(false).0.restarted);
}

#[test]
fn a_failure_resolves_the_futures_of_its_own_and_earlier_starts_and_not_a_later_one() {
    let mut state = HubState::new();
    state.begin(false);
    state.failed(error(), None);
    state.begin(false);
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
    state.begin(false);
    assert_eq!(gave_up(&state), None);
    state.failed(error(), Some(at));
    state.begin(false);
    assert_eq!(state.wanted(Some(0)), Some((1, Some(at))));
    // A start that made its one attempt inside the window passes the window on.
    state.failed(error(), Some(at));
    state.begin(false);
    assert_eq!(gave_up(&state), Some(at));
    // A thread that ended carries it too.
    assert!(state.ended(false));
    assert_eq!(gave_up(&state), Some(at));
}

#[test]
fn a_start_failed_before_it_could_wait_resolves_to_the_error() {
    let mut start = failed_start(MidiHardwareError::DescriptorBroken);
    let (_, waker) = counting();
    assert!(matches!(poll_with(&mut start, &waker), Poll::Ready(Err(_))));
}

/// A start that another thread publishes wakes the thread waiting on it. The bound
/// only guards a hang: the test asserts that the wait completed.
#[test]
fn a_gate_used_across_threads_wakes_the_waiting_thread() {
    let gate = Arc::new(StartGate::new());
    let start = gate.start(begin(&gate));
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
            gate.publish_ready().wake();
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
    begin(&gate);
    gate.publish_failed(error(), None).wake();
    assert_eq!(
        gate.wanted(Some(0)),
        None,
        "a failed round is not restarted by itself"
    );
    assert!(begin(&gate).restarted);
    assert_eq!(
        gate.wanted(Some(0)).map(|(generation, _)| generation),
        Some(1)
    );
}

#[test]
fn a_thread_that_ends_after_it_made_the_client_leaves_the_state_ready_and_wakes() {
    let gate = Arc::new(StartGate::new());
    let mut start = gate.start(begin(&gate));
    let (count, waker) = counting();
    assert!(poll_with(&mut start, &waker).is_pending());
    gate.ended(true).wake();
    assert_eq!(woken(&count), 1);
    assert!(matches!(poll_with(&mut start, &waker), Poll::Ready(Ok(_))));
}

#[test]
fn a_client_that_exists_is_never_restarted_over() {
    let mut state = HubState::new();
    state.failed(error(), None);
    let (round, _) = state.begin(true);
    assert!(!round.restarted);
    assert!(matches!(state, HubState::Ready));
}

#[test]
fn a_round_restarted_onto_a_thread_that_then_ends_fails_for_the_future_that_asked() {
    let mut state = HubState::new();
    state.begin(false);
    state.failed(error(), None);
    // A future asks for a new round, and the thread it woke is the one that is ending.
    let asked = state.begin(false).0;
    assert!(asked.restarted);
    assert!(state.ended(false));
    assert!(
        state
            .outcome(asked.generation)
            .is_some_and(|outcome| outcome.is_err()),
        "the future that asked is not left pending"
    );
}

/// A device list that a test changes, as the OS's enumeration.
static PRESENT: std::sync::Mutex<Vec<crate::OsEndpoint>> = std::sync::Mutex::new(Vec::new());

fn present() -> Vec<tutti_midi_device::OsDevice> {
    crate::devices::reporter::os_devices(&PRESENT.lock().unwrap(), &[])
}

/// A backend over a registry, whose enumeration needs a client the test makes.
struct OverRegistry {
    registry: Arc<Registry>,
    client: Arc<AtomicBool>,
}

impl crate::MidiEndpoints for OverRegistry {
    fn watch(&self, sink: crate::ChangeSink) -> crate::Result<Box<dyn crate::Watch>> {
        let client = Arc::clone(&self.client);
        Ok(self
            .registry
            .register(sink, present, move || client.load(Ordering::SeqCst)))
    }

    fn inputs(&self) -> crate::Result<Vec<crate::OsEndpoint>> {
        Ok(Vec::new())
    }

    fn outputs(&self) -> crate::Result<Vec<crate::OsEndpoint>> {
        Ok(Vec::new())
    }

    fn open_input(
        &self,
        _: Started,
        _: tutti_midi_device::EndpointId,
        _: crate::InputProducerHandle,
    ) -> crate::Result<Box<dyn crate::InputConnection>> {
        Err(MidiHardwareError::Unsupported("test"))
    }

    fn open_output(
        &self,
        _: Started,
        _: tutti_midi_device::EndpointId,
    ) -> crate::Result<Box<dyn crate::DeviceOut>> {
        Err(MidiHardwareError::Unsupported("test"))
    }
}

/// **A start that fails does not end the device watch**: the watch stays registered
/// and unseeded, a later start that makes the client seeds it, and a change after
/// it is reported.
#[test]
fn a_failed_start_leaves_the_watch_to_be_seeded_by_the_next_start() {
    let gate = Arc::new(StartGate::new());
    let registry = Arc::new(Registry::new());
    let client = Arc::new(AtomicBool::new(false));
    *PRESENT.lock().unwrap() = vec![crate::test_support::endpoint(1, "Keys")];
    // A `Devices` opened while the first round is under way.
    let devices = crate::Devices::open_over(
        &tutti_platform::Executor::new(),
        Box::new(OverRegistry {
            registry: Arc::clone(&registry),
            client: Arc::clone(&client),
        }),
    )
    .unwrap();
    let events = devices.subscribe();
    let mut first = gate.start(begin(&gate));
    let (_, waker) = counting();
    assert!(poll_with(&mut first, &waker).is_pending());

    // The round fails: the future says so, and the watch is neither seeded nor ended.
    gate.publish_failed(error(), None).wake();
    assert!(matches!(poll_with(&mut first, &waker), Poll::Ready(Err(_))));
    assert!(devices.list().is_empty());
    assert!(
        events.failure().is_none(),
        "the failure is the start's, not the watch's"
    );

    // A `Devices` opened while the gate is failed does not fail either.
    let late = crate::Devices::open_over(
        &tutti_platform::Executor::new(),
        Box::new(OverRegistry {
            registry: Arc::clone(&registry),
            client: Arc::clone(&client),
        }),
    )
    .unwrap();
    let late_events = late.subscribe();
    assert!(late_events.failure().is_none());

    // The app asks again, and the thread makes the client: set, look, then ready.
    let again = begin(&gate);
    assert!(again.restarted);
    let mut second = gate.start(again);
    client.store(true, Ordering::SeqCst);
    registry.look_all();
    gate.publish_ready().wake();
    assert!(matches!(poll_with(&mut second, &waker), Poll::Ready(Ok(_))));
    assert_eq!(devices.list().len(), 1, "the list is seeded");
    assert_eq!(late.list().len(), 1);
    assert!(events.failure().is_none());
    assert!(late_events.failure().is_none());

    // The watch is live: a device that comes is reported.
    PRESENT
        .lock()
        .unwrap()
        .push(crate::test_support::endpoint(2, "Pads"));
    registry.look_all();
    let seen: Vec<_> = events.pending().collect();
    assert!(
        seen.iter().any(|event| matches!(
            event,
            tutti_midi_device::DeviceEvent::Added(info) if info.name.contains("Pads")
        )),
        "{seen:?}"
    );
    drop((devices, late, events, late_events));
    assert_eq!(registry.len(), 0);
}

/// **A future made once the client exists is not left waiting for the thread's own
/// publication**: `begin` moves a round to `Ready` itself, and the waiters hear it
/// then, because the thread's `publish_ready` finds the state already `Ready` and
/// wakes no one.
#[test]
fn a_future_made_after_the_client_exists_wakes_those_already_waiting() {
    let gate = Arc::new(StartGate::new());
    let mut waiter = gate.start(begin(&gate));
    let (count, waker) = counting();
    assert!(poll_with(&mut waiter, &waker).is_pending());
    // The client is set; the thread is still enumerating when another caller asks.
    let (round, wakers) = gate.begin(true);
    assert!(!round.restarted);
    wakers.wake();
    assert_eq!(
        woken(&count),
        1,
        "the waiter was woken by the change to Ready"
    );
    // The thread then publishes: nothing is left to wake.
    gate.publish_ready().wake();
    assert_eq!(woken(&count), 1);
    assert!(matches!(poll_with(&mut waiter, &waker), Poll::Ready(Ok(_))));
}

/// **The gate wakes nothing itself**: a transition returns the wakers, so a caller
/// can wake them after releasing its own locks; a waker run inside a transition
/// that took one of those locks would deadlock.
#[test]
fn a_transition_wakes_nothing_until_its_wakers_are_woken() {
    let gate = Arc::new(StartGate::new());
    let mut waiter = gate.start(begin(&gate));
    let (count, waker) = counting();
    assert!(poll_with(&mut waiter, &waker).is_pending());
    let wakers = gate.ended(false);
    assert_eq!(woken(&count), 0, "not woken inside the transition");
    wakers.wake();
    assert_eq!(woken(&count), 1);
}

/// A waker that calls back into the gate when it is woken completes: the wakers are
/// woken with the gate's own lock released.
#[test]
fn a_waker_that_re_enters_the_gate_does_not_deadlock() {
    struct Reenter(Arc<StartGate>, std::sync::mpsc::Sender<()>);
    impl Wake for Reenter {
        fn wake(self: Arc<Self>) {
            let _ = self.0.begin(false);
            let _ = self.1.send(());
        }
    }
    let gate = Arc::new(StartGate::new());
    let mut waiter = gate.start(begin(&gate));
    let (done, finished) = std::sync::mpsc::channel();
    let waker = Waker::from(Arc::new(Reenter(Arc::clone(&gate), done)));
    assert!(poll_with(&mut waiter, &waker).is_pending());
    let worker = {
        let gate = Arc::clone(&gate);
        std::thread::spawn(move || gate.publish_ready().wake())
    };
    assert_eq!(
        finished.recv_timeout(Duration::from_secs(30)),
        Ok(()),
        "the waker re-entering the gate never finished"
    );
    worker.join().unwrap();
}
