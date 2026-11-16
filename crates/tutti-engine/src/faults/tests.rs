use super::*;

/// `record` takes no lock: a reader holding the control side's slots neither
/// stalls it nor costs it the message.
#[test]
fn record_while_the_control_side_is_reading_keeps_the_message() {
    let faults = StreamFaults::new();
    let held = faults.last_guard();
    faults.record(BackendErrorKind::DeviceGone, &"device not available");
    drop(held);
    assert_eq!(faults.count(), 1);
    assert!(faults.is_disconnected());
    assert_eq!(
        faults.last().map(|f| f.message),
        Some("device not available".to_string())
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

/// **A panic after a backend fault is the latest word, and a later backend
/// fault outranks the panic:** the newest of the two is reported.
#[test]
fn the_newest_of_a_backend_fault_and_a_panic_is_reported() {
    let faults = StreamFaults::new();
    faults.record(BackendErrorKind::Other, &"xrun");
    faults.record_panic("panicked");
    let last = faults.last().expect("a fault");
    assert_eq!(last.kind, StreamFaultKind::CallbackPanicked);
    assert_eq!(last.message, "panicked");

    faults.record(BackendErrorKind::DeviceGone, &"gone");
    assert_eq!(faults.last().expect("a fault").message, "gone");
    faults.record_panic("second");
    assert_eq!(faults.last().expect("a fault").message, "second");
    assert_eq!(faults.count(), 4);
}

/// **A fault raised on the render side is read with its kind and text, however
/// often it was raised before the control side looked:** the latest text wins
/// and every raise is counted.
#[test]
fn a_render_side_fault_raised_repeatedly_is_read_with_its_latest_text() {
    let faults = StreamFaults::new();
    for _ in 0..1000 {
        faults.record_overrun("late");
    }
    faults.record_overrun("the last overrun");
    let last = faults.last().expect("a fault");
    assert_eq!(last.kind, StreamFaultKind::Overrun);
    assert_eq!(last.message, "the last overrun");
    assert_eq!(faults.count(), 1001);

    for _ in 0..1000 {
        faults.record_panic("panicked");
    }
    faults.record_panic("the last panic");
    let last = faults.last().expect("a fault");
    assert_eq!(last.kind, StreamFaultKind::CallbackPanicked);
    assert_eq!(last.message, "the last panic");
    assert_eq!(faults.count(), 2002);
    assert!(faults.is_disconnected(), "a panic latches the stream");
}

/// **A text longer than `RT_TEXT_BYTES` is cut at a character boundary**, never
/// in the middle of one, whether it is one piece or several.
#[test]
fn a_long_text_is_cut_at_a_character_boundary() {
    let faults = StreamFaults::new();
    // Ascii up to one byte short of the limit, then a 2-byte character
    // straddling it.
    let text = format!("{}é{}", "a".repeat(RT_TEXT_BYTES - 1), "z");
    faults.record(BackendErrorKind::Other, &text.as_str());
    let message = faults.last().expect("a fault").message;
    assert_eq!(message, "a".repeat(RT_TEXT_BYTES - 1));

    let faults = StreamFaults::new();
    faults.record(BackendErrorKind::DeviceGone, &format_args!("{text}{text}"));
    assert_eq!(
        faults.last().expect("a fault").message.len(),
        RT_TEXT_BYTES - 1
    );
}

/// **`take_last` shows each fault once, and an overrun it outranked does not
/// come back:** after the take there is nothing until the next fault.
#[test]
fn take_last_forgets_everything_recorded_so_far() {
    let faults = StreamFaults::new();
    faults.record(BackendErrorKind::Other, &"xrun");
    faults.record_overrun("late");
    assert_eq!(
        faults
            .take_last()
            .expect("not lagging")
            .expect("a fault")
            .message,
        "xrun"
    );
    assert!(faults.last().is_none(), "the overrun it outranked is gone");
    faults.record_overrun("later");
    assert_eq!(faults.last().expect("a fault").message, "later");
}

/// **A restart forgets render-side faults too:** `clear` leaves `last` empty
/// though the cells still hold the old text.
#[test]
fn clear_forgets_render_side_faults() {
    let faults = StreamFaults::new();
    faults.record_panic("panicked");
    faults.record_overrun("late");
    faults.clear();
    assert!(faults.last().is_none());
    assert_eq!(faults.count(), 0);
    faults.record_overrun("after");
    assert_eq!(faults.last().expect("a fault").message, "after");
}

/// **`take_last` forgets what existed when it began and no more:** a fault
/// recorded after its snapshot is reported by the next `last`, not swallowed,
/// and the overrun it outranked is gone with it.
#[test]
fn a_fault_after_take_lasts_snapshot_is_reported_by_the_next_last() {
    let faults = StreamFaults::new();
    faults.record_overrun("old");
    let snapshot = faults.order.load(Ordering::Acquire);
    faults.record_panic("late");
    assert_eq!(
        faults
            .take_up_to(snapshot)
            .expect("not lagging")
            .expect("a fault")
            .message,
        "old",
        "the later panic is past the snapshot"
    );
    let last = faults.last().expect("the late fault survives the take");
    assert_eq!(last.kind, StreamFaultKind::CallbackPanicked);
    assert_eq!(last.message, "late");
    assert_eq!(
        faults
            .take_last()
            .expect("not lagging")
            .expect("a fault")
            .message,
        "late"
    );
    assert!(faults.last().is_none());
}

/// **Real threads: render-side writers against a control reader lose no fault
/// and report none out of priority.** Two threads raise overruns and panics
/// while the control side calls `last`, `take_last` and `count`: the count only
/// grows and equals the raises, every message read is whole text one writer
/// wrote with its own kind, and once the writers stop a panic outranks a newer
/// overrun.
#[test]
fn render_side_writers_and_a_control_reader_agree_on_priority_and_lose_nothing() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    const RAISES: u64 = 20_000;
    let faults = Arc::new(StreamFaults::new());
    let stop = Arc::new(AtomicBool::new(false));

    let overruns = {
        let faults = Arc::clone(&faults);
        thread::spawn(move || {
            for _ in 0..RAISES {
                faults.record_overrun("an overrun");
            }
        })
    };
    let panics = {
        let faults = Arc::clone(&faults);
        thread::spawn(move || {
            for _ in 0..RAISES {
                faults.record_panic("a panic");
            }
        })
    };
    let reader = {
        let faults = Arc::clone(&faults);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut last_count = 0;
            while !stop.load(Ordering::Acquire) {
                let count = faults.count();
                assert!(count >= last_count, "the count only grows");
                last_count = count;
                if let Some(fault) = faults.last() {
                    match fault.kind {
                        StreamFaultKind::CallbackPanicked => {
                            assert_eq!(fault.message, "a panic");
                        }
                        StreamFaultKind::Overrun => {
                            assert_eq!(fault.message, "an overrun");
                        }
                        other => panic!("a kind nobody recorded: {other:?}"),
                    }
                }
                if let Ok(Some(fault)) = faults.take_last() {
                    let expected = match fault.kind {
                        StreamFaultKind::CallbackPanicked => "a panic",
                        StreamFaultKind::Overrun => "an overrun",
                        other => panic!("a kind nobody recorded: {other:?}"),
                    };
                    assert_eq!(fault.message, expected);
                }
            }
        })
    };
    overruns.join().expect("overrun writer");
    panics.join().expect("panic writer");
    stop.store(true, Ordering::Release);
    reader.join().expect("control reader");

    assert_eq!(faults.count(), 2 * RAISES, "every raise was counted");
    // One more of each, with no reader: the panic outranks the later overrun.
    let _ = faults.take_last();
    faults.record_panic("final panic");
    faults.record_overrun("final overrun");
    let last = faults.last().expect("a fault");
    assert_eq!(last.kind, StreamFaultKind::CallbackPanicked);
    assert_eq!(last.message, "final panic");
}

/// **Nothing to take is an answer, not a lag:** an empty record and a record
/// already taken both answer `Ok(None)`, so a caller that moves its seen count
/// on `Ok(None)` is not left retrying.
#[test]
fn take_last_with_nothing_newer_is_ok_none() {
    let faults = StreamFaults::new();
    assert_eq!(faults.take_last(), Ok(None));
    faults.record_overrun("late");
    assert!(faults.take_last().expect("not lagging").is_some());
    assert_eq!(faults.take_last(), Ok(None));
}

/// **Two writers on the same cell lose no count, and no text but a loser's:**
/// two threads raise panics at once (the `try_borrow_mut` loser path: a
/// collision drops its text, never its count) while a third raises overruns
/// and the control side calls `last`. Every message read is one writer's whole
/// text, the count is exact, and once a panic has been raised and returned an
/// overrun is never what `last` answers (it lags to `None` or reports a panic).
#[test]
fn two_writers_on_one_cell_keep_the_count_and_whole_texts() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    const RAISES: u64 = 20_000;
    let faults = Arc::new(StreamFaults::new());
    let panics_returned = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let panic_writer = |text: &'static str| {
        let faults = Arc::clone(&faults);
        let returned = Arc::clone(&panics_returned);
        thread::spawn(move || {
            for _ in 0..RAISES {
                faults.record_panic(text);
                returned.fetch_add(1, Ordering::Release);
            }
        })
    };
    let a = panic_writer("panic from a");
    let b = panic_writer("panic from b");
    let overruns = {
        let faults = Arc::clone(&faults);
        thread::spawn(move || {
            for _ in 0..RAISES {
                faults.record_overrun("an overrun");
            }
        })
    };
    let reader = {
        let faults = Arc::clone(&faults);
        let returned = Arc::clone(&panics_returned);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let panicked = returned.load(Ordering::Acquire) > 0;
                match faults.last() {
                    Some(fault) if fault.kind == StreamFaultKind::CallbackPanicked => {
                        assert!(
                            fault.message == "panic from a" || fault.message == "panic from b",
                            "a torn text: {:?}",
                            fault.message
                        );
                    }
                    Some(fault) => {
                        assert!(!panicked, "an overrun reported over a returned panic");
                        assert_eq!(fault.message, "an overrun");
                    }
                    None => {}
                }
            }
        })
    };
    a.join().expect("writer a");
    b.join().expect("writer b");
    overruns.join().expect("overrun writer");
    stop.store(true, Ordering::Release);
    reader.join().expect("control reader");

    assert_eq!(
        faults.count(),
        3 * RAISES,
        "a lost text never loses a count"
    );
    assert_eq!(
        faults.take_last().expect("not lagging").map(|f| f.kind),
        Some(StreamFaultKind::CallbackPanicked)
    );
}
