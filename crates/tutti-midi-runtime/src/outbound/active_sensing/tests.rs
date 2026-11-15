use super::*;

fn sensing(group: MidiGroup) -> Ump {
    Ump::from(Message::System(group, System::ActiveSensing))
}

/// MIDI 1.0 p.32: a transmitter that is silent sends Active Sensing within
/// 300 ms; the first is due the interval after the output went quiet, and
/// none is due before.
#[test]
fn a_silent_output_is_sent_active_sensing_within_300_ms() {
    let t0 = Instant::now();
    let mut sender = ActiveSensingSender::new(MidiGroup::masked(3), t0);
    assert_eq!(sender.deadline(), t0 + ACTIVE_SENSING_INTERVAL);
    assert_eq!(
        sender.poll(t0 + ACTIVE_SENSING_INTERVAL - Duration::from_millis(1)),
        None
    );
    assert_eq!(
        sender.poll(t0 + ACTIVE_SENSING_INTERVAL),
        Some(sensing(MidiGroup::masked(3)))
    );
}

/// p.32: Active Sensing is sent when nothing else is, so any other message
/// moves the next one an interval past it.
#[test]
fn a_message_sent_defers_the_next_active_sensing() {
    let t0 = Instant::now();
    let mut sender = ActiveSensingSender::new(MidiGroup::FIRST, t0);
    let busy = t0 + Duration::from_millis(200);
    sender.sent(busy);
    assert_eq!(sender.deadline(), busy + ACTIVE_SENSING_INTERVAL);
    assert_eq!(sender.poll(t0 + ACTIVE_SENSING_INTERVAL), None);
    assert!(sender.poll(busy + ACTIVE_SENSING_INTERVAL).is_some());
}

/// Once started it goes on: every interval of silence has one, and its own
/// message counts as sent, so they come one per interval.
#[test]
fn an_idle_output_is_sent_one_active_sensing_every_interval() {
    let t0 = Instant::now();
    let mut sender = ActiveSensingSender::new(MidiGroup::FIRST, t0);
    let mut count = 0;
    for ms in 0..=2_000 {
        if sender.poll(t0 + Duration::from_millis(ms)).is_some() {
            count += 1;
        }
    }
    assert_eq!(count, 2_000 / ACTIVE_SENSING_INTERVAL.as_millis());
}

/// A wake that comes seconds late is one message, not one for each interval
/// missed, and the schedule resumes from it.
#[test]
fn a_late_wake_sends_one_and_resumes_from_it() {
    let t0 = Instant::now();
    let mut sender = ActiveSensingSender::new(MidiGroup::FIRST, t0);
    let late = t0 + Duration::from_secs(5);
    assert!(sender.poll(late).is_some());
    assert_eq!(sender.poll(late), None);
    assert_eq!(sender.deadline(), late + ACTIVE_SENSING_INTERVAL);
}

/// A time before the last message does not pull the schedule back.
#[test]
fn a_time_before_the_last_message_is_taken_as_the_last() {
    let t0 = Instant::now();
    let mut sender = ActiveSensingSender::new(MidiGroup::FIRST, t0 + Duration::from_secs(1));
    sender.sent(t0);
    assert_eq!(
        sender.deadline(),
        t0 + Duration::from_secs(1) + ACTIVE_SENSING_INTERVAL
    );
}
