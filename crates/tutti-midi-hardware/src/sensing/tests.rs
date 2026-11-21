use std::time::Duration;
use tutti_platform::Instant;

use tutti_midi_runtime::ACTIVE_SENSING_INTERVAL;
use tutti_ump::{Message, MidiGroup, Protocol, System, Ump};

use std::sync::Arc;

use tutti_midi_runtime::{MidiOut, Outgoing, Taken};
use tutti_platform::Executor;

use super::{Heartbeat, Pulse, beat};
use crate::endpoints::DeviceOut;

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

/// An output that takes nothing and counts nothing: only its `Arc` is looked at.
struct Idle;

impl MidiOut for Idle {
    fn queue_timed(&self, _events: &[Outgoing]) -> Taken {
        Taken::NONE
    }
}

impl DeviceOut for Idle {}

/// Dropping a heartbeat cancels its task, which drops the clone of the sink the
/// task holds: once the executor has run the cancellation, the caller's `Arc` is
/// the only one. A heartbeat that kept running would still hold the sink.
#[test]
fn dropping_a_heartbeat_releases_the_sink_its_task_held() {
    let executor = Executor::new();
    let sink = Arc::new(Idle);
    let heartbeat = Heartbeat::start(
        &executor,
        Arc::clone(&sink) as Arc<dyn DeviceOut>,
        Protocol::Midi1,
    );
    executor.run_until_stalled();
    assert_eq!(
        Arc::strong_count(&sink),
        2,
        "the task holds the sink while it runs"
    );
    drop(heartbeat);
    executor.run_until_stalled();
    assert_eq!(
        Arc::strong_count(&sink),
        1,
        "the cancelled task released it"
    );
}

/// A heartbeat whose executor has shut down ends without sending: a `Timer`'s
/// sleep fails from then on, and a heartbeat that slept again would spin. The future is polled once by hand on a thread, under
/// a hang guard: one that loops never returns.
#[test]
fn a_heartbeat_ends_when_its_executor_has_shut_down() {
    let timer = Executor::new().timer();
    let pulse = Pulse::new(Protocol::Midi1, Instant::now());
    let sink: Arc<dyn DeviceOut> = Arc::new(Idle);
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut ended = std::pin::pin!(beat(&timer, &sink, &pulse));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let _ = done.send(ended.as_mut().poll(&mut cx).is_ready());
    });
    assert_eq!(
        finished.recv_timeout(Duration::from_secs(30)),
        Ok(true),
        "the heartbeat ends with its executor"
    );
}
