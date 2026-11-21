use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::Duration;

use super::*;

/// Generous: only guards a hang.
const HANG_GUARD: Duration = Duration::from_secs(30);

fn _assert_send_sync<T: Send + Sync + Clone>() {}
const _: fn() = _assert_send_sync::<Executor>;

/// An executor and `n` threads that drive it.
fn with_threads(n: usize) -> (Executor, Threads) {
    let executor = Executor::new();
    let threads = executor
        .spawn_threads(NonZeroUsize::new(n).unwrap())
        .unwrap();
    (executor, threads)
}

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

/// Sends on a channel when dropped.
struct DropSignal(mpsc::Sender<()>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Blocks the test thread on `task`, failing after a hang guard.
fn wait<T>(task: Task<T>) -> T {
    struct Notify(Mutex<mpsc::Sender<()>>);
    impl Wake for Notify {
        fn wake(self: Arc<Self>) {
            let _ = lock(&self.0).send(());
        }
    }
    let (tx, rx) = mpsc::channel();
    let waker = Waker::from(Arc::new(Notify(Mutex::new(tx))));
    let mut cx = Context::from_waker(&waker);
    let mut task = task;
    loop {
        if let Poll::Ready(output) = Pin::new(&mut task).poll(&mut cx) {
            return output;
        }
        rx.recv_timeout(HANG_GUARD)
            .expect("the task never finished");
    }
}

/// A thread name as the running code sees it.
fn current_name() -> Option<String> {
    thread::current().name().map(str::to_owned)
}

#[test]
fn nothing_runs_until_driven_and_a_finished_task_is_not_polled_again() {
    let executor = Executor::new();
    let flag = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&flag);
    let task = executor.spawn(async move { f.store(true, Ordering::SeqCst) });
    assert!(
        !flag.load(Ordering::SeqCst),
        "spawn must not run the future"
    );
    assert!(!task.is_finished());
    assert_eq!(executor.run_until_stalled(), 1);
    assert!(flag.load(Ordering::SeqCst));
    assert!(task.is_finished());
    assert_eq!(
        executor.run_until_stalled(),
        0,
        "a finished task is not polled again"
    );
}

#[test]
fn a_pending_task_is_polled_again_only_after_its_wake() {
    let executor = Executor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let waker = Arc::new(Mutex::new(None));
    executor
        .spawn(pending_counted(Arc::clone(&polls), Arc::clone(&waker)))
        .detach();
    assert_eq!(executor.run_until_stalled(), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(executor.run_until_stalled(), 0, "unwoken: not polled");
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    lock(&waker).take().unwrap().wake();
    assert_eq!(executor.run_until_stalled(), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

#[test]
fn try_tick_runs_one_task_per_call_in_spawn_order_and_is_false_when_idle() {
    let executor = Executor::new();
    assert!(!executor.try_tick(), "nothing is ready on a new executor");
    let order = Arc::new(Mutex::new(Vec::new()));
    for i in 0..3 {
        let order = Arc::clone(&order);
        executor.spawn(async move { lock(&order).push(i) }).detach();
    }
    assert!(executor.try_tick());
    assert_eq!(*lock(&order), [0]);
    assert!(executor.try_tick());
    assert_eq!(*lock(&order), [0, 1]);
    assert!(executor.try_tick());
    assert_eq!(*lock(&order), [0, 1, 2]);
    assert!(!executor.try_tick(), "all three ran");
}

#[test]
fn try_tick_fires_a_due_sleep_before_it_runs_a_task() {
    let executor = Executor::new();
    let deadline = Instant::now() + Duration::from_millis(2);
    let sleep = executor.sleep_until(deadline);
    let task = executor.spawn(sleep);
    assert!(executor.try_tick(), "the task polls and arms its sleep");
    assert!(!task.is_finished());
    // The clock passing the deadline is the event: no thread stands in for it.
    while Instant::now() < deadline {
        thread::yield_now();
    }
    assert!(
        executor.try_tick(),
        "the due sleep woke its task, which ran"
    );
    assert!(task.is_finished());
    assert!(!executor.try_tick());
}

#[test]
fn a_self_waking_task_runs_once_per_try_tick() {
    let executor = Executor::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let p = Arc::clone(&polls);
    executor
        .spawn(std::future::poll_fn(move |cx| {
            p.fetch_add(1, Ordering::SeqCst);
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        }))
        .detach();
    for expected in 1..=3 {
        assert!(executor.try_tick());
        assert_eq!(polls.load(Ordering::SeqCst), expected);
    }
}

#[test]
fn a_task_that_panics_counts_as_run_and_the_drive_carries_on() {
    let executor = Executor::new();
    let flag = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&flag);
    executor
        .spawn(async { panic!("a task's own panic") })
        .detach();
    executor
        .spawn(async move { f.store(true, Ordering::SeqCst) })
        .detach();
    assert_eq!(executor.run_until_stalled(), 2);
    assert!(flag.load(Ordering::SeqCst));
}

#[test]
fn a_dropped_task_cancels_its_future() {
    let executor = Executor::new();
    let marker = Arc::new(());
    let held = Arc::clone(&marker);
    let task = executor.spawn(async move {
        let _held = held;
        std::future::pending::<()>().await;
    });
    executor.run_until_stalled();
    assert_eq!(Arc::strong_count(&marker), 2, "the pending future holds it");
    drop(task);
    executor.run_until_stalled();
    assert_eq!(Arc::strong_count(&marker), 1, "the future is dropped");
}

#[test]
fn a_detached_task_runs_to_completion() {
    let executor = Executor::new();
    let flag = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&flag);
    executor
        .spawn(async move { f.store(true, Ordering::SeqCst) })
        .detach();
    executor.run_until_stalled();
    assert!(flag.load(Ordering::SeqCst));
}

#[test]
fn a_sleep_with_a_past_deadline_resolves_within_one_drive() {
    let executor = Executor::new();
    let task = executor.spawn(executor.sleep_until(Instant::now()));
    executor.run_until_stalled();
    assert!(task.is_finished());
}

#[test]
fn a_sleep_with_a_far_deadline_stays_pending_across_drives() {
    let executor = Executor::new();
    let task = executor.spawn(executor.sleep(Duration::from_secs(3600 * 24)));
    for _ in 0..3 {
        executor.run_until_stalled();
    }
    assert!(!task.is_finished());
}

#[test]
fn dropping_an_undriven_executor_drops_its_pending_futures() {
    let executor = Executor::new();
    let marker = Arc::new(());
    let held = Arc::clone(&marker);
    executor
        .spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        })
        .detach();
    executor.run_until_stalled();
    assert_eq!(Arc::strong_count(&marker), 2);
    drop(executor);
    assert_eq!(Arc::strong_count(&marker), 1);
}

#[test]
fn dropping_an_executor_drops_the_futures_that_never_ran() {
    let executor = Executor::new();
    let marker = Arc::new(());
    let held = Arc::clone(&marker);
    let task = executor.spawn(async move { drop(held) });
    assert_eq!(Arc::strong_count(&marker), 2, "queued, not run");
    drop(executor);
    assert_eq!(Arc::strong_count(&marker), 1, "dropped with the executor");
    drop(task);
}

#[test]
fn a_thread_runs_a_task_and_its_output_resolves() {
    let (executor, _threads) = with_threads(2);
    assert_eq!(wait(executor.spawn(async { 6 * 7 })), 42);
}

#[test]
fn a_dropped_task_cancels_on_a_thread() {
    let (executor, _threads) = with_threads(1);
    let (tx, rx) = mpsc::channel();
    let (armed_tx, armed_rx) = mpsc::channel();
    let signal = DropSignal(tx);
    let task = executor.spawn(async move {
        let _signal = signal;
        armed_tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    armed_rx.recv_timeout(HANG_GUARD).unwrap();
    assert!(
        rx.try_recv().is_err(),
        "the future is alive while its task is"
    );
    drop(task);
    rx.recv_timeout(HANG_GUARD).expect("the future is dropped");
}

#[test]
fn a_detached_task_runs_on_a_thread() {
    let (executor, _threads) = with_threads(1);
    let (tx, rx) = mpsc::channel();
    executor
        .spawn(async move { tx.send("ran").unwrap() })
        .detach();
    assert_eq!(rx.recv_timeout(HANG_GUARD), Ok("ran"));
}

#[test]
fn a_sleep_wakes_a_parked_thread_at_its_deadline() {
    let (executor, _threads) = with_threads(1);
    let sleep = executor.sleep(Duration::from_millis(5));
    let (tx, rx) = mpsc::channel();
    executor
        .spawn(async move {
            assert_eq!(sleep.await, Ok(()));
            tx.send("due").unwrap();
        })
        .detach();
    assert_eq!(rx.recv_timeout(HANG_GUARD), Ok("due"));
}

#[test]
fn a_near_sleep_spawned_after_a_far_one_wakes_a_parked_thread() {
    let (executor, _threads) = with_threads(1);
    let mut far = executor.sleep(Duration::from_secs(3600 * 24));
    let (armed_tx, armed_rx) = mpsc::channel();
    executor
        .spawn(std::future::poll_fn(move |cx| {
            let polled = Pin::new(&mut far).poll(cx);
            let _ = armed_tx.send(());
            polled
        }))
        .detach();
    armed_rx.recv_timeout(HANG_GUARD).unwrap();
    let near = executor.sleep(Duration::from_millis(5));
    let (tx, rx) = mpsc::channel();
    executor
        .spawn(async move {
            assert_eq!(near.await, Ok(()));
            tx.send("near").unwrap();
        })
        .detach();
    assert_eq!(rx.recv_timeout(HANG_GUARD), Ok("near"));
}

/// Several threads drive one queue: tasks that wait on each other all finish only if more
/// than one thread runs.
#[test]
fn n_threads_drive_the_same_queue() {
    let (executor, _threads) = with_threads(2);
    let (a_tx, a_rx) = mpsc::sync_channel::<()>(0);
    let (b_tx, b_rx) = mpsc::sync_channel::<()>(0);
    let first = executor.spawn(async move {
        a_tx.send(()).unwrap();
        b_rx.recv().unwrap();
    });
    let second = executor.spawn(async move {
        a_rx.recv().unwrap();
        b_tx.send(()).unwrap();
    });
    wait(first);
    wait(second);
}

/// A thread's exit runs its thread-local destructors, which the drop's join waits for: the
/// signal is sent by the time `drop` returns.
#[test]
fn dropping_threads_joins_them() {
    thread_local! {
        static EXIT: std::cell::RefCell<Option<DropSignal>> = const { std::cell::RefCell::new(None) };
    }
    let (executor, threads) = with_threads(2);
    let (tx, rx) = mpsc::channel();
    let (armed_tx, armed_rx) = mpsc::channel();
    executor
        .spawn(async move {
            EXIT.with(|exit| *exit.borrow_mut() = Some(DropSignal(tx)));
            armed_tx.send(()).unwrap();
        })
        .detach();
    armed_rx.recv_timeout(HANG_GUARD).unwrap();
    assert!(rx.try_recv().is_err(), "the thread is still running");
    drop(threads);
    assert!(
        rx.try_recv().is_ok(),
        "the drop returned before the thread ended"
    );
    drop(executor);
}

#[test]
fn dropping_threads_leaves_the_queue_to_the_caller() {
    let (executor, threads) = with_threads(1);
    drop(threads);
    let task = executor.spawn(async { 5 });
    assert!(!task.is_finished(), "no thread drives it any more");
    assert_eq!(executor.run_until_stalled(), 1);
    assert_eq!(wait(task), 5);
}

#[test]
fn the_last_drop_drops_the_pending_futures() {
    let (executor, threads) = with_threads(2);
    let marker = Arc::new(());
    let held = Arc::clone(&marker);
    let (armed_tx, armed_rx) = mpsc::channel();
    executor
        .spawn(async move {
            let _held = held;
            armed_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        })
        .detach();
    armed_rx.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!(Arc::strong_count(&marker), 2);
    drop(threads);
    drop(executor);
    assert_eq!(
        Arc::strong_count(&marker),
        1,
        "the threads ended and the future dropped before the drops returned"
    );
}

/// A waker that a thread wakes can hold the `Threads`; its drop then runs on that thread,
/// which must not join itself.
#[test]
fn threads_dropped_on_one_of_their_own_do_not_join_themselves() {
    struct Holder {
        threads: Mutex<Option<Threads>>,
        dropped: Mutex<Option<mpsc::Sender<()>>>,
    }
    impl Wake for Holder {
        fn wake(self: Arc<Self>) {
            drop(lock(&self.threads).take());
            if let Some(tx) = lock(&self.dropped).take() {
                let _ = tx.send(());
            }
        }
    }

    let (executor, threads) = with_threads(1);
    let mut sleep = executor.sleep(Duration::from_millis(5));
    let (tx, rx) = mpsc::channel();
    let holder = Arc::new(Holder {
        threads: Mutex::new(Some(threads)),
        dropped: Mutex::new(Some(tx)),
    });
    let waker = Waker::from(Arc::clone(&holder));
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
    drop(waker);
    drop(holder);
    // A hang guard, not a speed bound: a self-join never sends.
    assert_eq!(rx.recv_timeout(HANG_GUARD), Ok(()));
}

#[test]
fn a_pending_sleep_resolves_once_its_executor_drops() {
    let executor = Executor::new();
    let mut sleep = executor.sleep(Duration::from_secs(3600 * 24));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
    drop(executor);
    assert_eq!(
        Pin::new(&mut sleep).poll(&mut cx),
        Poll::Ready(Err(Shutdown))
    );
}

#[test]
fn a_timer_keeps_no_executor_alive() {
    let (executor, threads) = with_threads(2);
    let timer = executor.timer();
    let marker = Arc::new(());
    let held = Arc::clone(&marker);
    let (armed_tx, armed_rx) = mpsc::channel();
    executor
        .spawn(async move {
            let _held = held;
            armed_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        })
        .detach();
    armed_rx.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!(Arc::strong_count(&marker), 2);
    drop(threads);
    drop(executor);
    assert_eq!(
        Arc::strong_count(&marker),
        1,
        "the executor shut down although a timer is held"
    );
    drop(timer);
}

#[test]
fn a_weak_executor_keeps_no_executor_alive_and_upgrades_while_one_lives() {
    let (executor, threads) = with_threads(1);
    let weak = executor.downgrade();
    let marker = Arc::new(());
    let held = Arc::clone(&marker);
    let (armed_tx, armed_rx) = mpsc::channel();
    executor
        .spawn(async move {
            let _held = held;
            armed_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        })
        .detach();
    armed_rx.recv_timeout(HANG_GUARD).unwrap();
    assert!(weak.is_alive(), "a clone lives");
    let upgraded = weak.upgrade().expect("a clone lives, so it upgrades");
    assert_eq!(
        wait(upgraded.spawn(async { 6 * 7 })),
        42,
        "the upgraded clone runs on the same executor's threads"
    );
    drop(upgraded);
    drop(threads);
    drop(executor);
    assert!(!weak.is_alive(), "no clone lives");
    assert_eq!(
        Arc::strong_count(&marker),
        1,
        "the executor shut down although a weak handle is held"
    );
    assert!(
        weak.upgrade().is_none(),
        "a shut-down executor does not upgrade"
    );
}

#[test]
fn a_timer_sleep_fires_when_due_and_something_drives() {
    let executor = Executor::new();
    let timer = executor.timer();
    let task = executor.spawn(timer.sleep_until(Instant::now()));
    assert!(!task.is_finished(), "nothing has driven yet");
    executor.run_until_stalled();
    assert!(task.is_finished());
    assert_eq!(wait(task), Ok(()));
    let far = executor.spawn(timer.sleep(Duration::from_secs(3600 * 24)));
    executor.run_until_stalled();
    assert!(!far.is_finished(), "its deadline is not due");
}

#[test]
fn a_timer_sleep_pending_on_another_thread_resolves_when_the_executor_drops() {
    let executor = Executor::new();
    let sleep = executor.timer().sleep(Duration::from_secs(3600 * 24));
    let (armed_tx, armed_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let awaiting = thread::spawn(move || {
        let mut sleep = sleep;
        let mut armed = Some(armed_tx);
        let waiter = Executor::new();
        let task = waiter.spawn(std::future::poll_fn(move |cx| {
            let poll = Pin::new(&mut sleep).poll(cx);
            if let Some(tx) = armed.take() {
                tx.send(poll.is_ready()).unwrap();
            }
            poll
        }));
        waiter.run_until_stalled();
        done_tx.send(()).unwrap();
        // The sleep woke the waiter's task; it finishes on the next drive.
        (waiter, task)
    });
    done_rx.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!(
        armed_rx.recv_timeout(HANG_GUARD),
        Ok(false),
        "pending first"
    );
    let (waiter, task) = awaiting.join().unwrap();
    assert!(!task.is_finished());
    drop(executor);
    waiter.run_until_stalled();
    assert!(task.is_finished(), "the shutdown woke the waiting task");
    assert_eq!(wait(task), Err(Shutdown));
}

#[test]
fn a_timer_sleep_made_after_the_executor_dropped_resolves_at_once() {
    let executor = Executor::new();
    let timer = executor.timer();
    drop(executor);
    let mut sleep = timer.sleep(Duration::from_secs(3600 * 24));
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(
        Pin::new(&mut sleep).poll(&mut cx),
        Poll::Ready(Err(Shutdown))
    );
}

#[test]
fn unblock_runs_off_the_calling_thread_and_its_output_resolves() {
    let executor = Executor::new();
    let caller = thread::current().id();
    let (ran_on, name, value) = wait(
        executor
            .unblock(move || (thread::current().id(), current_name(), 6 * 7))
            .expect("a pool thread"),
    );
    assert_ne!(ran_on, caller);
    assert!(
        name.as_deref()
            .is_some_and(|n| n.starts_with("tutti-blocking-")),
        "ran on {name:?}"
    );
    assert_eq!(value, 42);
}

/// A task that awaits an `unblock` finishes in the caller's drive: the closure runs on the
/// pool, and the wake it ends with schedules the awaiter on the queue for the caller.
#[test]
fn a_task_awaiting_unblock_finishes_in_the_callers_drive() {
    let executor = Executor::new();
    let exec = executor.clone();
    let awaiting =
        executor.spawn(async move { exec.unblock(|| 40 + 2).expect("a pool thread").await });
    // A caller's drive is a loop; the guard only stops a hang.
    let give_up = Instant::now() + HANG_GUARD;
    while !awaiting.is_finished() {
        executor.run_until_stalled();
        assert!(Instant::now() < give_up, "the awaiter never finished");
        thread::yield_now();
    }
    assert_eq!(wait(awaiting), 42);
}

#[test]
fn unblock_runs_many_closures_that_wait_on_each_other() {
    let executor = Executor::new();
    let (a_tx, a_rx) = mpsc::sync_channel::<()>(0);
    let (b_tx, b_rx) = mpsc::sync_channel::<()>(0);
    let first = executor
        .unblock(move || {
            a_tx.send(()).unwrap();
            b_rx.recv().unwrap();
        })
        .expect("a pool thread");
    let second = executor
        .unblock(move || {
            a_rx.recv().unwrap();
            b_tx.send(()).unwrap();
        })
        .expect("a pool thread");
    wait(first);
    wait(second);
}

#[test]
fn a_panicking_unblock_closure_leaves_the_pool_running() {
    let executor = Executor::new();
    let task = executor
        .unblock(|| -> u8 { panic!("a closure's own panic") })
        .expect("a pool thread");
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| wait(task)));
    assert!(outcome.is_err(), "awaiting the panicked task panics");
    assert_eq!(wait(executor.unblock(|| 9).expect("a pool thread")), 9);
}

#[test]
fn spawn_dedicated_runs_on_a_thread_of_its_own() {
    let (executor, _threads) = with_threads(1);
    let pooled = wait(executor.spawn(async { current_name() }));
    let dedicated = executor
        .spawn_dedicated("tutti-test-ded", None, async { current_name() })
        .unwrap();
    assert_eq!(wait(dedicated).as_deref(), Some("tutti-test-ded"));
    assert_eq!(pooled.as_deref(), Some("tutti-exec-0"));
}

#[test]
fn a_dedicated_future_runs_with_no_threads_and_no_drive() {
    let executor = Executor::new();
    let task = executor
        .spawn_dedicated("tutti-test-solo", None, async { 7 })
        .unwrap();
    assert_eq!(wait(task), 7);
}

#[test]
fn a_dedicated_future_fires_the_executors_sleeps() {
    let executor = Executor::new();
    let sleep = executor.sleep(Duration::from_millis(5));
    let task = executor
        .spawn_dedicated("tutti-test-sleeper", None, async move {
            assert_eq!(sleep.await, Ok(()));
            "due"
        })
        .unwrap();
    assert_eq!(wait(task), "due");
}

#[test]
fn a_dropped_dedicated_task_cancels_its_future_and_ends_the_thread() {
    let executor = Executor::new();
    let (tx, rx) = mpsc::channel();
    let (armed_tx, armed_rx) = mpsc::channel();
    let signal = DropSignal(tx);
    let task = executor
        .spawn_dedicated("tutti-test-cancel", None, async move {
            let _signal = signal;
            armed_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        })
        .unwrap();
    armed_rx.recv_timeout(HANG_GUARD).unwrap();
    drop(task);
    rx.recv_timeout(HANG_GUARD).expect("the future is dropped");
}

#[test]
fn the_last_drop_stops_and_joins_a_pending_dedicated_thread() {
    let executor = Executor::new();
    let marker = Arc::new(());
    let held = Arc::clone(&marker);
    let (armed_tx, armed_rx) = mpsc::channel();
    executor
        .spawn_dedicated("tutti-test-forever", None, async move {
            let _held = held;
            armed_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        })
        .unwrap()
        .detach();
    armed_rx.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!(Arc::strong_count(&marker), 2);
    drop(executor);
    assert_eq!(Arc::strong_count(&marker), 1);
}

/// **The executor's drop returns while dedicated futures are wedged in a call their wake
/// cannot reach, and each detached thread ends when its future does.** Three futures block
/// inside their polls on channels only the test sends on; the drop stops them, waits out
/// the shared bound, detaches them and returns. Each send then lets its thread finish.
#[test]
fn dedicated_threads_wedged_past_the_bound_are_detached_and_end_with_their_futures() {
    let executor = Executor::new();
    let (finished, ended) = mpsc::channel();
    let mut releases = Vec::new();
    for n in 0..3 {
        let (release, wedged) = mpsc::channel::<()>();
        let (entered, in_poll) = mpsc::channel();
        let finished = finished.clone();
        let _task = executor
            .spawn_dedicated(&format!("wedged-{n}"), None, async move {
                entered.send(()).unwrap();
                let _ = wedged.recv();
                let _ = finished.send(n);
            })
            .unwrap();
        in_poll.recv_timeout(HANG_GUARD).unwrap();
        releases.push(release);
    }
    let (dropped, returned) = mpsc::channel();
    let dropper = thread::spawn(move || {
        drop(executor);
        let _ = dropped.send(());
    });
    assert!(
        returned.recv_timeout(HANG_GUARD).is_ok(),
        "the drop returns past wedged dedicated threads"
    );
    dropper.join().unwrap();
    for release in releases {
        release.send(()).unwrap();
    }
    let mut done: Vec<_> = (0..3)
        .map(|_| ended.recv_timeout(HANG_GUARD).unwrap())
        .collect();
    done.sort_unstable();
    assert_eq!(
        done,
        [0, 1, 2],
        "every detached thread ends with its future"
    );
}

/// **A stop's joins share one deadline.** What is left is the bound minus the time
/// spent, never the bound again, and zero once it has passed: so `n` joins wait the bound
/// in all.
#[test]
fn a_stop_deadline_is_shared_by_every_join() {
    let t0 = Instant::now();
    let deadline = os::StopDeadline::after(t0);
    assert_eq!(deadline.left(t0), os::STOP_BOUND);
    let spent = os::STOP_BOUND / 4;
    assert_eq!(deadline.left(t0 + spent), os::STOP_BOUND - spent);
    assert_eq!(deadline.left(t0 + os::STOP_BOUND), Duration::ZERO);
    assert_eq!(deadline.left(t0 + os::STOP_BOUND * 3), Duration::ZERO);
}

/// **`join` waits for a task that runs on another thread and returns its output.** The
/// dedicated future ends only once the test sends, which it does after the waiter says it
/// is about to call `join`, so the output comes back through the wait.
#[test]
fn join_waits_for_a_task_run_elsewhere_and_returns_its_output() {
    let executor = Executor::new();
    let (go, wait) = mpsc::channel::<u32>();
    let task = executor
        .spawn_dedicated("joined", None, async move { wait.recv().unwrap() * 2 })
        .unwrap();
    let (ready, joining) = mpsc::channel();
    let (out, joined) = mpsc::channel();
    let waiter = thread::spawn(move || {
        ready.send(()).unwrap();
        let _ = out.send(join(task));
    });
    joining.recv_timeout(HANG_GUARD).unwrap();
    go.send(21).unwrap();
    assert_eq!(joined.recv_timeout(HANG_GUARD).unwrap(), Some(42));
    waiter.join().unwrap();
}

/// `join(task)` on a thread of its own, its outcome or `None` past the hang guard.
fn joined_off_thread<T: Send + 'static>(task: Task<T>) -> Option<T> {
    let (out, joined) = mpsc::channel();
    thread::spawn(move || {
        let _ = out.send(join(task));
    });
    joined
        .recv_timeout(HANG_GUARD)
        .expect("join returns: the hang guard passed")
}

/// **A task the executor drops instead of finishing joins as `None`**: a queued task no
/// driver ran, dropped at shutdown; a dedicated future still pending when its executor
/// drops; and a future that panicked.
#[test]
fn join_of_a_task_the_executor_dropped_is_none() {
    let executor = Executor::new();
    let queued = executor.spawn(async { 7 });
    let pending = executor
        .spawn_dedicated("pending", None, std::future::pending::<u32>())
        .unwrap();
    drop(executor);
    assert_eq!(joined_off_thread(queued), None);
    assert_eq!(joined_off_thread(pending), None);

    let executor = Executor::new();
    let panicked = executor
        .spawn_dedicated("panics", None, async { panic!("the future panicked") })
        .unwrap();
    assert_eq!(joined_off_thread::<()>(panicked), None);
    drop(executor);
}

/// **A task whose thread was detached at shutdown still joins with its output** once its
/// future ends: the wedged future is released after the executor's drop returned.
#[test]
fn join_of_a_task_detached_at_shutdown_returns_its_output_when_it_ends() {
    let executor = Executor::new();
    let (release, wedged) = mpsc::channel::<u32>();
    let (entered, in_poll) = mpsc::channel();
    let task = executor
        .spawn_dedicated("wedged", None, async move {
            entered.send(()).unwrap();
            wedged.recv().unwrap()
        })
        .unwrap();
    in_poll.recv_timeout(HANG_GUARD).unwrap();
    drop(executor);
    release.send(5).unwrap();
    assert_eq!(joined_off_thread(task), Some(5));
}

/// **`join` refuses a thread that drives an executor**: on a dedicated thread it panics
/// instead of stopping the drive, so the joining task ends as `None`.
#[test]
fn join_on_a_driving_thread_panics_instead_of_waiting() {
    let executor = Executor::new();
    let never = executor.spawn(std::future::pending::<()>());
    let joiner = executor
        .spawn_dedicated("joiner", None, async move { join(never) })
        .unwrap();
    assert_eq!(joined_off_thread(joiner), None);
}

/// **`join_within` hands back a task that has not ended within the bound, and the task
/// still runs**: a zero bound on a pending task returns it, and a later `join` gets its
/// output once the test lets it end.
#[test]
fn join_within_hands_back_a_running_task_past_the_bound() {
    let executor = Executor::new();
    let (go, wait) = mpsc::channel::<u32>();
    let task = executor
        .spawn_dedicated("bounded", None, async move { wait.recv().unwrap() })
        .unwrap();
    let task = join_within(task, Duration::ZERO).expect_err("the task has not ended");
    go.send(3).unwrap();
    assert_eq!(joined_off_thread(task), Some(3));
}

/// **`join_within` returns a task's outcome once it ends within the bound**: its output,
/// or `None` for a task its executor dropped.
#[test]
fn join_within_returns_the_outcome_of_a_task_that_ends() {
    let executor = Executor::new();
    let (go, wait) = mpsc::channel::<u32>();
    let task = executor
        .spawn_dedicated("ends", None, async move { wait.recv().unwrap() })
        .unwrap();
    go.send(8).unwrap();
    assert_eq!(join_within(task, HANG_GUARD).ok(), Some(Some(8)));

    let queued = executor.spawn(async { 1 });
    drop(executor);
    assert_eq!(join_within(queued, HANG_GUARD).ok(), Some(None));
}

/// Counts its wakes.
struct CountWakes(AtomicUsize);

impl Wake for CountWakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// **`yield_now` is pending once, having woken its task, then ready**: the first poll
/// returns to the executor and asks to run again, the second completes.
#[test]
fn yield_now_is_pending_once_after_waking_itself_then_ready() {
    let wakes = Arc::new(CountWakes(AtomicUsize::new(0)));
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    let mut yielding = Box::pin(yield_now());
    assert!(yielding.as_mut().poll(&mut cx).is_pending());
    assert_eq!(
        wakes.0.load(Ordering::SeqCst),
        1,
        "the yield wakes its task once"
    );
    assert!(yielding.as_mut().poll(&mut cx).is_ready());
    assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
}

/// **A task cancelled while it yields ends at the yield**: a dedicated loop of yields with
/// no stop of its own ends when its `Task` drops, which the executor sees between slices.
#[test]
fn a_loop_of_yields_ends_when_its_task_is_dropped() {
    let executor = Executor::new();
    let (ended, done) = mpsc::channel::<()>();
    let (entered, looping) = mpsc::channel();
    let task = executor
        .spawn_dedicated("yields", None, async move {
            let _ended = DropSignal(ended);
            entered.send(()).unwrap();
            loop {
                yield_now().await;
            }
        })
        .unwrap();
    looping.recv_timeout(HANG_GUARD).unwrap();
    drop(task);
    assert!(
        done.recv_timeout(HANG_GUARD).is_ok(),
        "the cancelled loop drops its future"
    );
    drop(executor);
}

/// **A local dedicated future runs on its own thread, built there, and its output comes
/// back through its `Task`.** The future holds an `Rc` (not `Send`) across an await and
/// records the thread it runs on; `make` runs on that same thread, which is the dedicated
/// one, not the test's.
#[test]
fn a_local_dedicated_future_is_built_and_run_on_its_own_thread() {
    let executor = Executor::new();
    let (go, wait) = mpsc::channel::<()>();
    let caller = thread::current().id();
    let task = executor
        .spawn_dedicated_local("local", None, move || {
            let made_on = thread::current().id();
            let held = std::rc::Rc::new(made_on);
            async move {
                yield_now().await;
                wait.recv().unwrap();
                (*held, thread::current().id())
            }
        })
        .unwrap();
    go.send(()).unwrap();
    let (made_on, ran_on) = joined_off_thread(task).expect("the future ends");
    assert_eq!(made_on, ran_on, "the future runs where it was built");
    assert_ne!(ran_on, caller, "on the dedicated thread, not the caller's");
}

/// **A local dedicated future is dropped on its own thread when its `Task` drops**: the
/// drop signal it holds reports the thread it was dropped on.
#[test]
fn a_cancelled_local_dedicated_future_is_dropped_on_its_own_thread() {
    struct DroppedOn {
        to: mpsc::Sender<thread::ThreadId>,
        _not_send: std::rc::Rc<()>,
    }
    impl Drop for DroppedOn {
        fn drop(&mut self) {
            let _ = self.to.send(thread::current().id());
        }
    }
    let executor = Executor::new();
    let (dropped, on) = mpsc::channel();
    let (entered, running) = mpsc::channel();
    let task = executor
        .spawn_dedicated_local("local-cancel", None, move || {
            let held = DroppedOn {
                to: dropped,
                _not_send: std::rc::Rc::new(()),
            };
            async move {
                let _held = held;
                entered.send(thread::current().id()).unwrap();
                std::future::pending::<()>().await;
            }
        })
        .unwrap();
    let ran_on = running.recv_timeout(HANG_GUARD).unwrap();
    drop(task);
    assert_eq!(on.recv_timeout(HANG_GUARD).unwrap(), ran_on);
    drop(executor);
}

/// **A local dedicated future whose `make` panics ends its task as `None`**, as a
/// panicking dedicated future does: `make` runs inside the task on its thread, so a failed
/// build (a stream that cannot open, say) is the task's panic, and `join` reads `None`.
#[test]
fn a_local_dedicated_make_that_panics_joins_as_none() {
    let executor = Executor::new();
    let task = executor
        .spawn_dedicated_local("local-panics", None, || -> std::future::Ready<u8> {
            panic!("the stream did not open")
        })
        .unwrap();
    assert_eq!(joined_off_thread(task), None);
}

/// **The executor's shutdown ends a local dedicated thread too**: a pending local future
/// is dropped and its task joins as `None`.
#[test]
fn shutdown_ends_a_local_dedicated_future() {
    let executor = Executor::new();
    let task = executor
        .spawn_dedicated_local("local-shutdown", None, std::future::pending::<u8>)
        .unwrap();
    drop(executor);
    assert_eq!(joined_off_thread(task), None);
}

/// **`is_driving` is true on a thread that drives an executor and false elsewhere**: a
/// dedicated future and a task on a `Threads` driver read `true`, the test's own thread
/// `false`.
#[test]
fn is_driving_is_true_only_on_a_driving_thread() {
    assert!(!is_driving(), "the test's thread drives nothing");
    let executor = Executor::new();
    let task = executor
        .spawn_dedicated("drives", None, async { is_driving() })
        .unwrap();
    assert_eq!(joined_off_thread(task), Some(true));
    let _threads = executor.spawn_threads(NonZeroUsize::MIN).unwrap();
    let on_driver = executor.spawn(async { is_driving() });
    assert_eq!(
        joined_off_thread(on_driver),
        Some(true),
        "a `Threads` driver drives"
    );
}

#[cfg(target_os = "linux")]
mod linux;
