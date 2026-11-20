use super::*;
use crate::{PriorityOutcome, PriorityRequest};

/// The nice value of the calling thread, from `/proc/thread-self/stat` (field 19;
/// `comm` is parenthesised and may hold spaces).
fn nice() -> i64 {
    let stat = std::fs::read_to_string("/proc/thread-self/stat").unwrap();
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    rest.split_whitespace()
        .nth(19 - 3)
        .unwrap()
        .parse()
        .unwrap()
}

/// The request reaches the dedicated thread: granted, it is nice -10; refused, the
/// thread keeps the nice an unrequested dedicated thread has.
#[test]
#[cfg_attr(miri, ignore = "reads /proc and calls the OS scheduler")]
fn spawn_dedicated_passes_the_priority_request_to_its_thread() {
    let executor = Executor::new();
    let plain = wait(
        executor
            .spawn_dedicated("tutti-test-plain", None, async { nice() })
            .unwrap(),
    );
    let raised = wait(
        executor
            .spawn_dedicated("tutti-test-raised", Some(PriorityRequest::Raised), async {
                nice()
            })
            .unwrap(),
    );
    // What the OS grants is the host's choice (an unprivileged run is refused); the
    // thread's own outcome decides which arm holds.
    let outcome = thread::spawn(|| crate::request_priority(PriorityRequest::Raised))
        .join()
        .unwrap();
    match outcome {
        PriorityOutcome::Granted { .. } => assert_eq!(raised, -10),
        PriorityOutcome::Refused { .. } => assert_eq!(raised, plain),
    }
}

/// Counts the `WARN` events of the thread it is set on.
struct CountWarns(Arc<AtomicUsize>);

impl tracing::Subscriber for CountWarns {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::WARN {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// An OS that refuses the priority gets one warning, and the thread still runs and
/// `spawn_dedicated` still returns `Ok`; one that grants it logs nothing. An unprivileged
/// run is refused `Highest` (nice -20), so the refused arm is the usual one.
#[test]
#[cfg_attr(miri, ignore = "reads /proc and calls the OS scheduler")]
fn a_refused_dedicated_priority_warns_once_and_the_thread_runs() {
    let executor = Executor::new();
    let warns = Arc::new(AtomicUsize::new(0));
    let task = tracing::subscriber::with_default(CountWarns(Arc::clone(&warns)), || {
        executor.spawn_dedicated(
            "tutti-test-refused",
            Some(PriorityRequest::Highest),
            async { 7 },
        )
    });
    assert_eq!(wait(task.expect("a refused priority is not an error")), 7);
    let outcome = thread::spawn(|| crate::request_priority(PriorityRequest::Highest))
        .join()
        .unwrap();
    let expected = usize::from(!outcome.is_granted());
    assert_eq!(warns.load(Ordering::SeqCst), expected);
}
