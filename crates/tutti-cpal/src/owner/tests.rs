use super::*;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, ThreadId};
use tutti_platform::Executor;
use tutti_rt::Mutex;

/// A `!Send` value that records the thread it is created and dropped on.
struct Probe {
    _not_send: Rc<()>,
    dropped_on: Arc<Mutex<Option<ThreadId>>>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        // Slow, so a handle drop that does not wait is observable.
        thread::sleep(std::time::Duration::from_millis(100));
        *self.dropped_on.lock() = Some(thread::current().id());
    }
}

/// The value is created and dropped on one thread that is neither the
/// opener's nor the dropper's, and the drop has finished when the handle's
/// drop returns.
#[test]
fn the_value_is_dropped_on_its_creating_thread() {
    let executor = Executor::new();
    let created = Arc::new(Mutex::new(None));
    let dropped = Arc::new(Mutex::new(None));
    let (c, d) = (Arc::clone(&created), Arc::clone(&dropped));
    let owned = Owned::spawn(&executor, "probe", move || {
        *c.lock() = Some(thread::current().id());
        Ok(Probe {
            _not_send: Rc::new(()),
            dropped_on: d,
        })
    })
    .expect("opens");

    let from_other_thread = thread::spawn(move || drop(owned));
    from_other_thread.join().unwrap();

    let created = created.lock().expect("created");
    let dropped = dropped.lock().expect("dropped before drop returned");
    assert_eq!(created, dropped);
    assert_ne!(created, thread::current().id());
}

/// A failed build reports its error, and by the time `spawn` returns the
/// build closure (and what it captured) has been dropped on the owner thread.
#[test]
fn a_failed_build_is_reported() {
    let executor = Executor::new();
    struct Guard(Arc<Mutex<Option<ThreadId>>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            thread::sleep(std::time::Duration::from_millis(100));
            *self.0.lock() = Some(thread::current().id());
        }
    }
    let dropped = Arc::new(Mutex::new(None));
    let guard = Guard(Arc::clone(&dropped));
    let r = Owned::spawn::<()>(&executor, "probe", move || {
        let _guard = guard;
        Err(DeviceError::InvalidConfig("no".into()))
    });
    assert!(matches!(r, Err(DeviceError::InvalidConfig(_))));
    let id = dropped.lock().expect("closure dropped before return");
    assert_ne!(id, thread::current().id());
}

/// A panicking build is an error, not a hang.
#[test]
fn a_panicking_build_is_an_error() {
    let executor = Executor::new();
    let r = Owned::spawn::<()>(&executor, "probe", || panic!("boom"));
    assert!(matches!(r, Err(DeviceError::Io(_))));
}

/// A value is dropped even when its handle drops on a thread that drives an
/// executor: that drop detaches instead of joining (a join there panics), and
/// the owner thread still drops the value once it has the stop.
#[test]
fn a_drop_on_an_executor_thread_detaches_and_the_value_still_drops() {
    struct Notify(mpsc::Sender<()>);
    impl Drop for Notify {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    let executor = Executor::new();
    let (dropped, wait) = mpsc::channel();
    let owned = Owned::spawn(&executor, "probe", move || Ok(Notify(dropped))).expect("opens");
    let dropper = executor
        .spawn_dedicated("dropper", None, async move { drop(owned) })
        .expect("spawns");
    // A real-time limit that only guards a hang.
    wait.recv_timeout(std::time::Duration::from_secs(60))
        .expect("the owner thread dropped the value");
    tutti_platform::join_within(dropper, std::time::Duration::from_secs(60))
        .unwrap_or_else(|_| panic!("the dropping task ended"));
}
