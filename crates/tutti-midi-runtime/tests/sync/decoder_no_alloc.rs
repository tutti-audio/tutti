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
use tutti_units::Seconds;

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
    use tutti_units::{Frame, SampleRate};

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

/// **The follower steps segmented strings without allocating**, the hostile
/// paths included: a segmented LOCATE, a stray segment, a skipped segment, a
/// first inside a string, a segment followed by a command (RP-013 p.40 b), a
/// segment after a command, a string that ends inside a command, a segment
/// inside a string, and a full 64-segment string of 2880 commands.
#[test]
fn machine_control_follower_segments_are_allocation_free() {
    use tutti_midi_protocol::{
        MMC_MAX_SEGMENTED, MmcCommand, MmcCommands, MmcSegmentCount, MmcSegmentError, MmcSegments,
        Recipient, SysexDeviceId,
    };
    use tutti_midi_runtime::{MachineControlEvent, MachineControlFollower, SegmentFault};
    use tutti_ump::{MidiGroup, Sysex7Packets, Ump};

    let to = Recipient::Device(SysexDeviceId::of(1));
    let raw = |commands: &[u8]| {
        let mut payload = vec![0x7F, 0x01, 0x06];
        payload.extend_from_slice(commands);
        payload
    };
    let mut payloads = vec![
        // A LOCATE in two segments, then a stray last segment.
        raw(&[0x53, 0x05, 0x41, 0x44, 0x06, 0x01, 0x61]),
        raw(&[0x53, 0x05, 0x00, 0x02, 0x08, 0x14, 0x00]),
        raw(&[0x53, 0x02, 0x00, 0x02]),
        // A segment skipped: first with two to come, then the last.
        raw(&[0x53, 0x02, 0x42, 0x02]),
        raw(&[0x53, 0x02, 0x00, 0x02]),
        // A first inside a string, which starts its own (a STOP).
        raw(&[0x53, 0x02, 0x41, 0x02]),
        raw(&[0x53, 0x02, 0x40, 0x01]),
        // A segment followed by a command, and one after a command.
        raw(&[0x53, 0x02, 0x40, 0x02, 0x02]),
        raw(&[0x02, 0x53, 0x02, 0x40, 0x02]),
        // A string that ends inside a command.
        raw(&[0x53, 0x03, 0x41, 0x44, 0x06]),
        raw(&[0x53, 0x02, 0x00, 0x01]),
        // A segment inside a string, and a plain PLAY.
        raw(&[0x53, 0x06, 0x40, 0x02, 0x53, 0x02, 0x40, 0x02]),
        raw(&[0x02]),
    ];
    // The longest string: 64 segments, 2880 PLAYs.
    for segment in MmcSegments::new(&[0x02; MMC_MAX_SEGMENTED]).unwrap() {
        let mut commands = MmcCommands::new();
        commands.push(&MmcCommand::CommandSegment(segment)).unwrap();
        payloads.push(commands.payload(to).to_vec());
    }
    let messages: Vec<Vec<Ump>> = payloads
        .iter()
        .map(|payload| {
            Sysex7Packets::from_payload(MidiGroup::FIRST, payload)
                .unwrap()
                .collect()
        })
        .collect();
    let mut follower = MachineControlFollower::new(SysexDeviceId::of(1));
    let round = |follower: &mut MachineControlFollower| {
        let mut events = 0;
        for message in &messages {
            for &ump in message {
                follower.step(ump, &mut |_| events += 1);
            }
        }
        events
    };
    // Warm up outside the gate, and pin what a round makes, message by message
    // (RP-013 p.40): the LOCATE, a stray later segment, a skipped segment, a
    // first inside a string and the STOP of its own string, p.40 b, p.40 a, a
    // string cut inside a command, a PLAY and a nested segment, a plain PLAY,
    // and the 2880 PLAYs of the longest string.
    let mut heard = Vec::new();
    for message in &messages {
        for &ump in message {
            follower.step(ump, &mut |e| heard.push(e));
        }
    }
    let string = |e: MmcSegmentError| MachineControlEvent::Segmentation(SegmentFault::String(e));
    assert!(matches!(heard[0], MachineControlEvent::Locate(_)));
    assert_eq!(
        heard[1..10],
        [
            string(MmcSegmentError::MissingFirst),
            string(MmcSegmentError::OutOfOrder {
                expected: MmcSegmentCount::of(1),
                got: MmcSegmentCount::of(0)
            }),
            string(MmcSegmentError::FirstInsideString),
            MachineControlEvent::Stop,
            MachineControlEvent::Segmentation(SegmentFault::CountMismatch),
            MachineControlEvent::Segmentation(SegmentFault::NotFirst),
            string(MmcSegmentError::Malformed),
            MachineControlEvent::Play,
            MachineControlEvent::Segmentation(SegmentFault::Nested),
        ]
    );
    assert!(heard[10..].iter().all(|e| *e == MachineControlEvent::Play));
    assert_eq!(heard.len(), 11 + MMC_MAX_SEGMENTED);
    let per_round = heard.len();
    let mut events = 0;
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100 {
            events += round(&mut follower);
        }
    });
    assert_eq!(events, 100 * per_round);
}
