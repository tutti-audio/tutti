//! Regression gate: `MidiClockDecoder::tick` must not allocate.
//!
//! The decoder runs on whichever thread receives MIDI input — for hardware
//! clock messages that arrives on a high-priority I/O thread, and the
//! decoded position then flows into the audio callback. Either path is
//! RT-sensitive: an allocation here either drops MIDI sync ticks or
//! adds jitter to the derived tempo.
//!
//! Tick body is pure scalar state arithmetic + a fixed-size ring buffer
//! update.

use tutti_midi_runtime::MidiClockDecoder;
use tutti_types::Seconds;

fn us_per_tick(bpm: f64) -> Seconds {
    Seconds::new(
        (60_000_000.0 / (bpm * f64::from(tutti_ump::System::TIMING_CLOCKS_PER_QUARTER))).trunc()
            / 1e6,
    )
}

#[test]
fn midi_clock_decoder_tick_is_allocation_free() {
    let mut clock = MidiClockDecoder::new();
    clock.start_msg();

    // Warm up the interval ring buffer so subsequent calls take the
    // tempo-update branch every time.
    let interval = us_per_tick(120.0);
    let mut ts = Seconds::new(1.0);
    for _ in 0..48 {
        clock.tick(ts);
        ts += interval;
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            clock.tick(ts);
            ts += interval;
        }
    });
}

#[test]
fn midi_clock_decoder_transport_msgs_are_allocation_free() {
    let mut clock = MidiClockDecoder::new();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            clock.start_msg();
            clock.stop_msg();
            clock.continue_msg();
            clock.reset();
        }
    });
}

#[test]
fn midi_clock_decoder_queries_are_allocation_free() {
    let mut clock = MidiClockDecoder::new();
    clock.start_msg();
    let interval = us_per_tick(140.0);
    let mut ts = Seconds::new(1.0);
    for _ in 0..48 {
        clock.tick(ts);
        ts += interval;
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            let _ = clock.beat_position();
            let _ = clock.tempo_bpm();
            let _ = clock.transport_state();
        }
    });
}

/// **The Machine Control sender allocates nothing on the audio thread**: each
/// look at a transport that starts, jumps and stops builds its message inline
/// (`MmcPayload`): a PLAY, a STOP, and a LOCATE with DEFERRED PLAY.
#[test]
fn machine_control_sender_observe_is_allocation_free() {
    use tutti_midi_protocol::{MtcRate, Recipient};
    use tutti_midi_runtime::MachineControlSender;
    use tutti_types::{Frame, SampleRate};

    let rate = SampleRate::SR_48K;
    let mut sender = MachineControlSender::new(Recipient::AllCall, MtcRate::Fps25);
    // Warm up outside the gate: the first look only looks.
    sender
        .observe(false, Seconds::new(0.0), Frame::new(0), rate)
        .unwrap();
    let mut sent = 0;
    let mut frame = 0;
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000 {
            let at = f64::from(round % 50);
            // Standing, rolling from elsewhere (a locate and a deferred play), a
            // jump back while rolling, a stop.
            for (playing, seconds) in [(false, at), (true, at + 7.0), (true, at), (false, at)] {
                frame += 512;
                let message = sender
                    .observe(playing, Seconds::new(seconds), Frame::new(frame), rate)
                    .unwrap();
                sent += usize::from(message.is_some());
            }
        }
    });
    assert!(sent >= 3_000, "locates and stops were sent: {sent}");
}

/// **The follower steps a segmented string without allocating**: the segments
/// of a LOCATE across messages, a segment that does not belong, and a plain
/// message are each one `step`.
#[test]
fn machine_control_follower_segments_are_allocation_free() {
    use tutti_midi_protocol::SysexDeviceId;
    use tutti_midi_runtime::MachineControlFollower;
    use tutti_ump::{MidiGroup, Sysex7Packets};

    let mut follower = MachineControlFollower::new(SysexDeviceId::of(1));
    let messages: Vec<Vec<tutti_ump::Ump>> = [
        vec![0x7F, 0x01, 0x06, 0x53, 0x05, 0x41, 0x44, 0x06, 0x01, 0x61],
        vec![0x7F, 0x01, 0x06, 0x53, 0x05, 0x00, 0x02, 0x08, 0x14, 0x00],
        vec![0x7F, 0x01, 0x06, 0x53, 0x02, 0x00, 0x02],
        vec![0x7F, 0x01, 0x06, 0x02],
    ]
    .iter()
    .map(|payload| {
        Sysex7Packets::from_payload(MidiGroup::FIRST, payload)
            .unwrap()
            .collect()
    })
    .collect();
    let mut events = 0;
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            for message in &messages {
                for &ump in message {
                    follower.step(ump, &mut |_| events += 1);
                }
            }
        }
    });
    assert_eq!(
        events, 3_000,
        "a locate, a segmentation error and a play each round"
    );
}
