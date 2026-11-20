use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::Waker;
use std::time::Duration;

use super::*;

/// Generous: only guards a hang.
const HANG_GUARD: Duration = Duration::from_secs(30);

fn _assert_send_sync<T: Send + Sync + Clone>() {}
const _: fn() = _assert_send_sync::<Executor>;

/// A future that stores its waker, counts its polls and stays pending.
fn pending_counted(
    polls: Arc<AtomicUsize>,
    waker: Arc<Mutex<Option<Waker>>>,
) -> impl Future<Output = ()> + Send + 'static {
    std::future::poll_fn(move |cx| {
        polls.fetch_add(1, Ordering::SeqCst);
        *lock(&waker) = Some(cx.waker().clone());
        Poll::Pending
    })
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_polled_executor_runs_a_future_only_inside_poll() {
    let executor = Executor::polled();
    let flag = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&flag);
    executor
        .spawn(async move { f.store(true, Ordering::SeqCst) })
        .unwrap();
    assert!(
        !flag.load(Ordering::SeqCst),
        "spawn must not run the future"
    );
    assert_eq!(executor.poll(1), 1);
    assert!(flag.load(Ordering::SeqCst));
    assert_eq!(executor.poll(1), 0, "a finished future is not polled again");
}

#[test]
fn a_polled_executor_polls_a_pending_future_again_only_after_its_wake() {
    let executor = Executor::polled();
    let polls = Arc::new(AtomicUsize::new(0));
    let waker = Arc::new(Mutex::new(None));
    executor
        .spawn(pending_counted(Arc::clone(&polls), Arc::clone(&waker)))
        .unwrap();
    assert_eq!(executor.poll(1), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(executor.poll(1), 0, "unwoken: not polled");
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    lock(&waker).take().unwrap().wake();
    assert_eq!(executor.poll(1), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

#[test]
fn poll_runs_at_most_max_and_keeps_the_rest_queued_in_order() {
    let executor = Executor::polled();
    let order = Arc::new(Mutex::new(Vec::new()));
    for i in 0..3 {
        let order = Arc::clone(&order);
        executor.spawn(async move { lock(&order).push(i) }).unwrap();
    }
    assert_eq!(executor.poll(2), 2);
    assert_eq!(*lock(&order), [0, 1]);
    assert_eq!(executor.poll(2), 1);
    assert_eq!(*lock(&order), [0, 1, 2]);
}

#[test]
fn a_self_waking_future_is_polled_once_per_poll_call() {
    let executor = Executor::polled();
    let polls = Arc::new(AtomicUsize::new(0));
    let p = Arc::clone(&polls);
    executor
        .spawn(std::future::poll_fn(move |cx| {
            p.fetch_add(1, Ordering::SeqCst);
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        }))
        .unwrap();
    assert_eq!(executor.poll(10), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(executor.poll(10), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_threaded_executor_completes_a_future_without_poll() {
    let executor = Executor::threaded();
    let (tx, rx) = mpsc::channel();
    executor.spawn(async move { tx.send(7).unwrap() }).unwrap();
    assert_eq!(rx.recv_timeout(HANG_GUARD).unwrap(), 7);
    assert_eq!(
        executor.poll(1),
        0,
        "poll is a no-op on a threaded executor"
    );
}

#[test]
fn a_threaded_future_woken_from_outside_is_polled_again() {
    let executor = Executor::threaded();
    let (tx, rx) = mpsc::channel();
    let waker = Arc::new(Mutex::new(None::<Waker>));
    let w = Arc::clone(&waker);
    let mut first = true;
    executor
        .spawn(std::future::poll_fn(move |cx| {
            if first {
                first = false;
                *lock(&w) = Some(cx.waker().clone());
                tx.send("pending").unwrap();
                Poll::Pending
            } else {
                tx.send("done").unwrap();
                Poll::Ready(())
            }
        }))
        .unwrap();
    assert_eq!(rx.recv_timeout(HANG_GUARD).unwrap(), "pending");
    lock(&waker).take().unwrap().wake();
    assert_eq!(rx.recv_timeout(HANG_GUARD).unwrap(), "done");
}

#[test]
fn spawn_dedicated_runs_on_a_thread_with_the_given_name() {
    let executor = Executor::threaded();
    let (tx, rx) = mpsc::channel();
    executor
        .spawn_dedicated("tutti-test-ded", None, async move {
            tx.send(thread::current().name().map(str::to_owned))
                .unwrap();
        })
        .unwrap();
    assert_eq!(
        rx.recv_timeout(HANG_GUARD).unwrap().as_deref(),
        Some("tutti-test-ded")
    );
}

#[test]
fn a_host_executor_receives_every_spawned_future() {
    let received = Arc::new(Mutex::new(Vec::<BoxFuture>::new()));
    let r = Arc::clone(&received);
    let executor = Executor::new(move |future| lock(&r).push(future));
    let ran = Arc::new(AtomicUsize::new(0));
    for dedicated in [false, true] {
        let ran = Arc::clone(&ran);
        let future = async move {
            ran.fetch_add(1, Ordering::SeqCst);
        };
        if dedicated {
            executor.spawn_dedicated("ignored", None, future).unwrap();
        } else {
            executor.spawn(future).unwrap();
        }
    }
    assert_eq!(executor.poll(5), 0);
    let futures = mem::take(&mut *lock(&received));
    assert_eq!(futures.len(), 2);
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the host has not run them yet"
    );
    let mut cx = Context::from_waker(Waker::noop());
    for mut future in futures {
        assert!(future.as_mut().poll(&mut cx).is_ready());
    }
    assert_eq!(ran.load(Ordering::SeqCst), 2);
}

#[test]
fn dropping_a_threaded_executor_joins_and_drops_a_pending_pool_future() {
    let executor = Executor::threaded();
    let dropped = Arc::new(AtomicBool::new(false));
    let waker = Arc::new(Mutex::new(None::<Waker>));
    let (tx, rx) = mpsc::channel();
    let (guard, w) = (DropFlag(Arc::clone(&dropped)), Arc::clone(&waker));
    executor
        .spawn(std::future::poll_fn(move |cx| {
            let _ = &guard;
            *lock(&w) = Some(cx.waker().clone());
            tx.send(()).unwrap();
            Poll::<()>::Pending
        }))
        .unwrap();
    rx.recv_timeout(HANG_GUARD).unwrap();
    assert!(!dropped.load(Ordering::SeqCst));
    drop(executor);
    assert!(
        dropped.load(Ordering::SeqCst),
        "the pending future is dropped"
    );
}

#[test]
fn dropping_a_threaded_executor_joins_and_drops_a_pending_dedicated_future() {
    let executor = Executor::threaded();
    let dropped = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let guard = DropFlag(Arc::clone(&dropped));
    executor
        .spawn_dedicated(
            "tutti-test-forever",
            None,
            std::future::poll_fn(move |_| {
                let _ = &guard;
                tx.send(()).unwrap();
                Poll::<()>::Pending
            }),
        )
        .unwrap();
    rx.recv_timeout(HANG_GUARD).unwrap();
    drop(executor);
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn dropping_a_polled_executor_drops_its_futures() {
    let executor = Executor::polled();
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = DropFlag(Arc::clone(&dropped));
    executor
        .spawn(async move {
            let _guard = guard;
        })
        .unwrap();
    drop(executor);
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn a_polled_sleep_with_a_past_deadline_resolves_within_one_poll() {
    let executor = Executor::polled();
    let flag = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&flag);
    let sleep = executor.sleep_until(Instant::now()).unwrap();
    executor
        .spawn(async move {
            sleep.await;
            f.store(true, Ordering::SeqCst);
        })
        .unwrap();
    executor.poll(1);
    assert!(flag.load(Ordering::SeqCst));
}

#[test]
fn a_polled_future_awaiting_a_far_deadline_stays_pending_across_polls() {
    let executor = Executor::polled();
    let flag = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&flag);
    let sleep = executor.sleep(Duration::from_secs(3600 * 24)).unwrap();
    executor
        .spawn(async move {
            sleep.await;
            f.store(true, Ordering::SeqCst);
        })
        .unwrap();
    for _ in 0..5 {
        executor.poll(8);
    }
    assert!(!flag.load(Ordering::SeqCst));
}

#[test]
fn a_polled_poll_wakes_a_sleep_whose_deadline_passed_after_it_pended() {
    let executor = Executor::polled();
    let polls = Arc::new(AtomicUsize::new(0));
    let p = Arc::clone(&polls);
    let mut sleep = executor
        .sleep_until(Instant::now() + Duration::from_millis(20))
        .unwrap();
    executor
        .spawn(std::future::poll_fn(move |cx| {
            p.fetch_add(1, Ordering::SeqCst);
            Pin::new(&mut sleep).poll(cx)
        }))
        .unwrap();
    assert_eq!(executor.poll(8), 1);
    // The deadline is real time away: poll until the sleep fires, bounded only
    // against a hang.
    let end = Instant::now() + HANG_GUARD;
    while executor.poll(8) == 0 {
        assert!(Instant::now() < end, "the sleep never woke");
        std::thread::yield_now();
    }
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_threaded_sleep_with_a_past_deadline_resolves() {
    let executor = Executor::threaded();
    let (tx, rx) = mpsc::channel();
    let sleep = executor.sleep_until(Instant::now()).unwrap();
    executor
        .spawn(async move {
            sleep.await;
            tx.send(()).unwrap();
        })
        .unwrap();
    rx.recv_timeout(HANG_GUARD).unwrap();
}

#[test]
fn a_threaded_sleep_resolves_at_a_deadline_ahead() {
    let executor = Executor::threaded();
    let (tx, rx) = mpsc::channel();
    let deadline = Instant::now() + Duration::from_millis(10);
    let sleep = executor.sleep_until(deadline).unwrap();
    executor
        .spawn(async move {
            sleep.await;
            tx.send(Instant::now()).unwrap();
        })
        .unwrap();
    assert!(rx.recv_timeout(HANG_GUARD).unwrap() >= deadline);
}

#[test]
fn an_earlier_deadline_inserted_later_wakes_the_timer_thread_first() {
    let executor = Executor::threaded();
    let (tx, rx) = mpsc::channel();
    for (label, sleep) in [
        (
            "far",
            executor.sleep(Duration::from_secs(3600 * 24)).unwrap(),
        ),
        ("near", executor.sleep(Duration::from_millis(10)).unwrap()),
    ] {
        let tx = tx.clone();
        executor
            .spawn(async move {
                sleep.await;
                tx.send(label).unwrap();
            })
            .unwrap();
    }
    assert_eq!(rx.recv_timeout(HANG_GUARD).unwrap(), "near");
}

#[test]
fn a_host_executor_sleep_resolves_on_its_native_timer_thread() {
    let spawned = Arc::new(Mutex::new(Vec::<BoxFuture>::new()));
    let s = Arc::clone(&spawned);
    let executor = Executor::new(move |future| lock(&s).push(future));
    let flag = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&flag);
    let sleep = executor.sleep_until(Instant::now()).unwrap();
    executor
        .spawn(async move {
            sleep.await;
            f.store(true, Ordering::SeqCst);
        })
        .unwrap();
    let mut future = lock(&spawned).pop().unwrap();
    let waker = Waker::noop();
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(waker))
            .is_ready()
    );
    assert!(flag.load(Ordering::SeqCst));
}

#[test]
fn dropping_a_threaded_executor_joins_its_timer_thread_and_leaves_sleeps_pending() {
    let executor = Executor::threaded();
    let mut sleep = executor.sleep(Duration::from_secs(3600 * 24)).unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
    drop(executor);
    assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
}

/// A waker the timer thread wakes can hold the last clone of the executor; its drop
/// then runs on the timer thread, which must not join itself.
#[test]
fn the_last_executor_dropped_on_the_timer_thread_does_not_join_itself() {
    use std::task::Wake;

    struct Holder {
        executor: Mutex<Option<Executor>>,
        dropped: Mutex<Option<mpsc::Sender<()>>>,
    }
    impl Wake for Holder {
        fn wake(self: Arc<Self>) {
            drop(self.executor.lock().unwrap().take());
            if let Some(tx) = self.dropped.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }

    let executor = Executor::threaded();
    let mut sleep = executor.sleep(Duration::from_millis(5)).unwrap();
    let (tx, rx) = mpsc::channel();
    let holder = Arc::new(Holder {
        executor: Mutex::new(Some(executor)),
        dropped: Mutex::new(Some(tx)),
    });
    let waker = Waker::from(Arc::clone(&holder));
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
    drop(waker);
    drop(holder);
    // A hang guard, not a speed bound: a self-join never sends.
    assert_eq!(rx.recv_timeout(Duration::from_secs(30)), Ok(()));
}
