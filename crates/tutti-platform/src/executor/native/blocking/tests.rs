use std::sync::mpsc;
use std::task::{Context, Poll, Wake, Waker};
use std::{future::Future, pin::Pin};

use super::*;

/// Generous: only guards a hang.
const HANG_GUARD: Duration = Duration::from_secs(30);

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

#[test]
fn an_idle_thread_ends_and_a_later_closure_starts_another() {
    let pool = Blocking::new(Duration::from_millis(1));
    assert_eq!(wait(pool.unblock(|| 1).expect("a pool thread")), 1);
    assert!(
        pool.wait_until_empty(HANG_GUARD),
        "the idle thread never ended"
    );
    assert_eq!(pool.thread_count(), 0);
    assert_eq!(wait(pool.unblock(|| 2).expect("a pool thread")), 2);
}

#[test]
fn a_closed_pool_ends_its_idle_threads_and_refuses_new_closures() {
    let pool = Blocking::new(Duration::from_secs(3600));
    assert_eq!(wait(pool.unblock(|| 1).expect("a pool thread")), 1);
    assert_eq!(pool.thread_count(), 1, "the thread idles for an hour");
    pool.close();
    assert!(pool.wait_until_empty(HANG_GUARD), "close woke no thread");
    assert!(pool.unblock(|| 3).is_err(), "refused, not queued");
}

#[test]
fn closures_past_the_cap_queue_and_all_run() {
    let pool = Blocking::new(Duration::from_millis(1));
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let (started_tx, started_rx) = mpsc::channel();
    let tasks: Vec<_> = (0..MAX_THREADS + 4)
        .map(|i| {
            let (release_rx, started_tx) = (Arc::clone(&release_rx), started_tx.clone());
            pool.unblock(move || {
                started_tx.send(()).unwrap();
                // Holds a thread until released: one message per closure.
                lock(&release_rx).recv().unwrap();
                i
            })
            .expect("a pool thread")
        })
        .collect();
    for _ in 0..MAX_THREADS {
        started_rx.recv_timeout(HANG_GUARD).unwrap();
    }
    assert_eq!(pool.thread_count(), MAX_THREADS, "the cap holds");
    for _ in 0..tasks.len() {
        release_tx.send(()).unwrap();
    }
    let mut done: Vec<_> = tasks.into_iter().map(wait).collect();
    done.sort_unstable();
    assert_eq!(done, (0..MAX_THREADS + 4).collect::<Vec<_>>());
}
