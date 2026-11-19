use std::time::Duration;
use tutti_platform::Instant;

use tutti_midi_runtime::ACTIVE_SENSING_INTERVAL;
use tutti_ump::{Message, MidiGroup, Protocol, System, Ump};

use super::Pulse;

fn active_sensing() -> Ump {
    Ump::from(Message::System(MidiGroup::FIRST, System::ActiveSensing))
}

const MS: fn(u64) -> Duration = Duration::from_millis;

/// An output that sends every 10 ms for a second is never silent for an
/// interval, so no Active Sensing is due at any of those instants.
#[test]
fn traffic_defers_active_sensing() {
    let t0 = Instant::now();
    let pulse = Pulse::new(Protocol::Midi1, t0);
    for step in 1..=100 {
        let at = t0 + MS(10 * step);
        assert_eq!(pulse.due(at), None, "step {step}");
        pulse.sent(at);
    }
    assert_eq!(
        pulse.due(t0 + MS(1_000) + ACTIVE_SENSING_INTERVAL),
        Some(active_sensing()),
        "silence an interval past the last message sends one"
    );
}

/// An idle output with the option sends one Active Sensing per interval, and
/// goes on.
#[test]
fn an_idle_output_sends_active_sensing_every_interval() {
    let t0 = Instant::now();
    let pulse = Pulse::new(Protocol::Midi1, t0);
    assert_eq!(pulse.due(t0 + ACTIVE_SENSING_INTERVAL - MS(1)), None);
    for n in 1..=4 {
        assert_eq!(
            pulse.due(t0 + ACTIVE_SENSING_INTERVAL * n),
            Some(active_sensing()),
            "beat {n}"
        );
    }
}

/// A MIDI 2.0 Protocol output has no Active Sensing; it sends once the
/// Protocol is MIDI 1.0.
#[test]
fn a_midi2_protocol_output_sends_no_active_sensing_until_it_is_in_midi1() {
    let t0 = Instant::now();
    let pulse = Pulse::new(Protocol::Midi2, t0);
    assert_eq!(pulse.due(t0 + ACTIVE_SENSING_INTERVAL), None);
    assert_eq!(pulse.due(t0 + ACTIVE_SENSING_INTERVAL * 2), None);
    pulse.follow(Protocol::Midi1);
    assert_eq!(
        pulse.due(t0 + ACTIVE_SENSING_INTERVAL * 3),
        Some(active_sensing())
    );
    pulse.follow(Protocol::Midi2);
    assert_eq!(pulse.due(t0 + ACTIVE_SENSING_INTERVAL * 4), None);
}

/// The wake time is the schedule's deadline: an interval past the last message.
#[test]
fn the_deadline_is_an_interval_past_the_last_message() {
    let t0 = Instant::now();
    let pulse = Pulse::new(Protocol::Midi1, t0);
    assert_eq!(pulse.deadline(), t0 + ACTIVE_SENSING_INTERVAL);
    pulse.sent(t0 + MS(100));
    assert_eq!(pulse.deadline(), t0 + MS(100) + ACTIVE_SENSING_INTERVAL);
}
