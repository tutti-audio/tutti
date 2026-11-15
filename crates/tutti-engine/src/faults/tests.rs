use super::*;

/// `record` must never wait on a reader holding `last`: it counts the fault
/// and drops the message.
#[test]
fn record_while_last_is_locked_counts_and_returns() {
    let faults = StreamFaults::new();
    let held = faults.last_guard();
    faults.record(BackendErrorKind::DeviceGone, &"device not available");
    drop(held);
    assert_eq!(faults.count(), 1);
    assert!(faults.is_disconnected());
    assert!(
        faults.last().is_none(),
        "the message was dropped, not waited for"
    );

    faults.record(BackendErrorKind::DeviceGone, &"device not available");
    assert_eq!(faults.count(), 2);
    assert_eq!(
        faults.last().map(|f| f.kind),
        Some(StreamFaultKind::Disconnected)
    );
}

fn pollerr() -> &'static str {
    "`alsa::poll()` returned POLLERR"
}

/// **A device lost on ALSA is detected though the backend never says the
/// device is gone there.**
///
/// ALSA's worker reports a lost device as a backend error "returned POLLERR",
/// repeated at spin rate with no data. This feeds that
/// stream of errors, one per millisecond, and expects a disconnect once
/// `STALL_GRACE` has passed, counted once at the start of the run and once at
/// the promotion rather than once per error.
#[test]
fn a_spinning_backend_error_with_no_audio_becomes_a_disconnect() {
    let faults = StreamFaults::new();
    let t0 = faults.epoch;
    let err = pollerr();
    let ms = Duration::from_millis;

    faults.record_at(BackendErrorKind::Other, &err, t0 + ms(1));
    assert!(!faults.is_disconnected());
    for i in 2..400 {
        faults.record_at(BackendErrorKind::Other, &err, t0 + ms(i));
    }
    assert!(!faults.is_disconnected(), "inside the grace period");
    for i in 400..700 {
        faults.record_at(BackendErrorKind::Other, &err, t0 + ms(i));
    }
    assert!(faults.is_disconnected());
    assert_eq!(faults.count(), 2);
    assert_eq!(
        faults.last().map(|f| f.kind),
        Some(StreamFaultKind::Disconnected)
    );
    assert!(faults.last().unwrap().message.contains("POLLERR"));

    // Further errors are ignored outright.
    faults.record_at(BackendErrorKind::Other, &err, t0 + ms(900));
    assert_eq!(faults.count(), 2);
}

/// **A transient xrun does not fail the stream.** Bursts of backend errors,
/// each followed by delivered audio, never accumulate into a disconnect, even
/// over far longer than the grace period.
#[test]
fn xruns_between_delivered_blocks_never_disconnect() {
    let faults = StreamFaults::new();
    let t0 = faults.epoch;
    let ms = Duration::from_millis;
    for burst in 0..20u64 {
        let base = burst * 300;
        for i in 0..5 {
            faults.record_at(BackendErrorKind::Other, &pollerr(), t0 + ms(base + i));
        }
        faults.delivered();
    }
    assert!(!faults.is_disconnected());
    assert!(faults.is_faulted());
}

/// `clear` forgets a run in progress, so a restart does not inherit it.
#[test]
fn clear_forgets_a_run_in_progress() {
    let faults = StreamFaults::new();
    let t0 = faults.epoch;
    faults.record_at(
        BackendErrorKind::Other,
        &pollerr(),
        t0 + Duration::from_millis(1),
    );
    faults.clear();
    faults.record_at(
        BackendErrorKind::Other,
        &pollerr(),
        t0 + Duration::from_millis(900),
    );
    assert!(!faults.is_disconnected(), "a fresh run starts at 900 ms");
    assert_eq!(faults.count(), 1);
}

/// **A route change and a refused real-time promotion are not faults**: the
/// stream keeps playing, so neither is counted nor starts a stall run.
#[test]
fn a_route_change_and_a_refused_promotion_record_nothing() {
    let faults = StreamFaults::new();
    faults.record(BackendErrorKind::Survivable, &"device changed");
    faults.record(BackendErrorKind::Survivable, &"real-time promotion denied");
    assert!(!faults.is_faulted());
    assert_eq!(faults.count(), 0);
}

/// **Each overrun is one count, and the stream stays up.** Overruns recorded
/// with no audio delivered between them are not a run of backend errors: they
/// never promote to a disconnect however long they go on, and none raises the
/// latch `is_running` reads.
#[test]
fn each_overrun_counts_once_and_never_disconnects() {
    let faults = StreamFaults::new();
    for expected in 1..=3 {
        faults.record_overrun("late");
        assert_eq!(faults.count(), expected);
    }
    assert!(!faults.is_disconnected());
    assert_eq!(
        faults.last().map(|f| f.kind),
        Some(StreamFaultKind::Overrun)
    );
}

/// **An overrun does not replace a latching fault's message**, and a panic
/// does replace a lesser one: the first cause of a stream's end is the one
/// `last` keeps from an overrun, and a panic is always the latest word.
#[test]
fn an_overrun_keeps_a_latching_message_and_a_panic_replaces_a_lesser_one() {
    let faults = StreamFaults::new();
    faults.record(BackendErrorKind::DeviceGone, &"unplugged");
    faults.record_overrun("late");
    let last = faults.last().expect("a fault");
    assert_eq!(last.kind, StreamFaultKind::Disconnected);
    assert_eq!(last.message, "unplugged");
    assert_eq!(faults.count(), 2, "the overrun still counts");

    let faults = StreamFaults::new();
    faults.record(BackendErrorKind::Other, &"xrun");
    faults.record_overrun("late");
    let kept = faults.last().expect("a fault");
    assert_eq!(kept.kind, StreamFaultKind::Backend);
    assert_eq!(
        kept.message, "xrun",
        "the overrun left the owned text alone"
    );
    faults.record_panic("panicked");
    assert_eq!(
        faults.last().map(|f| f.kind),
        Some(StreamFaultKind::CallbackPanicked)
    );
}

/// **A panic retires the owned message it replaces** instead of dropping it
/// on the render thread, and a control-side read frees it.
#[test]
fn a_panic_retires_an_owned_message_and_a_read_frees_it() {
    let faults = StreamFaults::new();
    faults.record(BackendErrorKind::Other, &"xrun");
    faults.record_panic("panicked");
    assert_eq!(
        faults
            .last_guard()
            .retired
            .as_ref()
            .map(|f| f.message.clone()),
        Some(Cow::Owned::<str>("xrun".to_string())),
        "the replaced owned message is held aside"
    );
    let last = faults.last().expect("a fault");
    assert_eq!(last.kind, StreamFaultKind::CallbackPanicked);
    assert_eq!(last.message, "panicked");
    assert!(faults.last_guard().retired.is_none(), "the read freed it");
    assert_eq!(
        faults.last().map(|f| f.kind),
        Some(StreamFaultKind::CallbackPanicked)
    );
}

/// **With the retired slot occupied, a second panic over an owned message
/// leaks that message rather than freeing it:** the slot keeps the first one.
#[test]
fn a_panic_over_an_owned_message_with_the_slot_taken_leaves_the_slot_alone() {
    let faults = StreamFaults::new();
    faults.record(BackendErrorKind::Other, &"xrun");
    faults.record_panic("first");
    faults.record(BackendErrorKind::DeviceGone, &"gone");
    faults.record_panic("second");
    assert_eq!(
        faults
            .last_guard()
            .retired
            .as_ref()
            .map(|f| f.message.to_string()),
        Some("xrun".to_string())
    );
    assert_eq!(
        faults.last().map(|f| f.message.to_string()),
        Some("second".to_string())
    );
    assert_eq!(faults.count(), 4);
}
