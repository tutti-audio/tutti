use super::*;
use crate::test_support::at;

use tutti_fixtures::midi;
use tutti_ump::{Message, MidiChannel, NoteKeyEvent, Utility};
use tutti_units::{Frame, Samples};

/// The sender-clock value if `ump` is a JR Clock (M2-104 section 7.2.2.1).
fn jr_clock_value(ump: &Ump) -> Option<u16> {
    match (*ump).decode() {
        Ok(Message::Utility(Utility::JrClock(t))) => Some(t),
        _ => None,
    }
}

/// The timestamp value if `ump` is a JR Timestamp (section 7.2.2.2).
fn jr_timestamp_value(ump: &Ump) -> Option<u16> {
    match (*ump).decode() {
        Ok(Message::Utility(Utility::JrTimestamp(t))) => Some(t),
        _ => None,
    }
}

#[test]
fn clock_maps_samples_to_ticks_monotonically() {
    let clock = JrClock::new(SampleRate::SR_48K);
    // 48000 samples = 1 s = 31250 ticks, which wraps: 31250 & 0xFFFF = 31250.
    assert_eq!(clock.ticks_at(Frame::new(0)), 0);
    assert_eq!(clock.ticks_at(Frame::new(48_000)), 31_250);
    // Half a second → half the ticks.
    assert_eq!(clock.ticks_at(Frame::new(24_000)), 15_625);
}

#[test]
fn clock_wraps_at_16_bits() {
    // At exactly the JR reference rate, one sample is one tick, so the wrap
    // boundary is integer-exact (no float rounding at the edge).
    let clock = JrClock::new(TICK_RATE);
    assert_eq!(clock.ticks_at(Frame::new(0xFFFF)), 0xFFFF);
    assert_eq!(
        clock.ticks_at(Frame::new(0x1_0000)),
        0,
        "wraps back to zero"
    );
    assert_eq!(clock.ticks_at(Frame::new(0x1_0001)), 1);
}

#[test]
fn the_first_clock_is_due_immediately() {
    // A receiver cannot use any stamp until it has a clock reference, so
    // waiting one interval before the first would leave the opening block's
    // timing unusable.
    let mut e = JrClockEmitter::new(SampleRate::SR_48K);
    let first = e.due(Frame::new(0)).expect("a clock is owed at once");
    assert_eq!(jr_clock_value(&first), Some(0));
}

/// A clock carries the sender time of the frame it is due at, in ticks, and a
/// cadence shorter than the 250 ms maximum is allowed.
/// Spec: m2-104/7.2.2.1#1, m2-104/7.2.2.1#5
#[test]
fn clocks_are_emitted_on_the_configured_cadence() {
    let mut e = JrClockEmitter::with_interval(SampleRate::SR_48K, Duration::from_millis(100));
    let interval = e.interval;
    assert_eq!(interval, Samples::new(4_800));
    let at = |frames: Samples| Frame::new(frames.get() as u64);

    assert!(e.due(Frame::new(0)).is_some(), "first");
    assert!(
        e.due(at(Samples::new(interval.get() - 1))).is_none(),
        "one sample short"
    );
    let second = e.due(at(interval)).expect("exactly due");
    assert_eq!(jr_clock_value(&second), Some(3_125), "100 ms is 3125 ticks");
    assert!(
        e.due(at(Samples::new(interval.get() + 1))).is_none(),
        "just emitted"
    );
    assert!(
        e.due(at(Samples::new(interval.get() * 2))).is_some(),
        "next period"
    );
}

/// Spec: m2-104/7.2.2.1#2, m2-104/7.2.2.1#7
#[test]
fn the_cadence_keeps_running_through_silence() {
    // The load-bearing case. §7.2.2.1: JR Clocks are independent of
    // any other message, and §7.2.2.3 makes a receiver discard
    // every stamp until it sees one. A cadence driven by outgoing traffic
    // would satisfy a test that sends notes and still emit nothing on an
    // idle stream — which is exactly when a receiver most needs the clock.
    let sample_rate = 48_000.0;
    let block = Samples::new(512);
    let mut stream = JrStream {
        emitter: Some(JrClockEmitter::with_interval(
            SampleRate::of(sample_rate),
            Duration::from_millis(100),
        )),
        ..JrStream::new(SampleRate::of(sample_rate))
    };

    let mut clocks = 0;
    // 1 second of entirely silent blocks.
    for _ in 0..(sample_rate as usize / block.get()) {
        let mut out = Vec::new();
        stream.stamp_span(&[], block, &mut out);
        clocks += out
            .iter()
            .filter(|e| jr_clock_value(&e.1).is_some())
            .count();
        assert!(
            out.iter().all(|e| jr_clock_value(&e.1).is_some()),
            "a silent block carries clocks and nothing else"
        );
    }
    // 100 ms cadence over ~1 s: the first plus one per interval.
    assert!(
        (10..=11).contains(&clocks),
        "expected ~10 clocks across a silent second, got {clocks}"
    );
}

#[test]
fn a_stamped_block_carries_its_clock_before_the_stamps() {
    let sample_rate = 48_000.0;
    let mut stream = JrStream::new(SampleRate::of(sample_rate)).clock(SampleRate::of(sample_rate));
    let events = [at(0, midi::note_on(MidiChannel::FIRST, 60, 0x8000))];
    let mut out = Vec::new();
    stream.stamp_span(&events, Samples::new(512), &mut out);
    // clock, then stamp, then the note.
    assert_eq!(out.len(), 3);
    assert!(jr_clock_value(&out[0].1).is_some(), "clock leads");
    assert!(jr_timestamp_value(&out[1].1).is_some(), "then the stamp");
    assert_eq!(out[2], events[0]);
}

#[test]
fn a_stream_without_a_clock_emits_none() {
    // `new` alone stamps only — the cadence is opt-in, so existing
    // stamp-only callers keep their exact output.
    let mut stream = JrStream::new(SampleRate::SR_48K);
    let mut out = Vec::new();
    stream.stamp_span(&[], Samples::new(512), &mut out);
    assert!(out.is_empty(), "no clock, no events, no output");
}

#[test]
fn stamp_advances_by_events_and_stamp_span_by_the_block() {
    // The distinction the cadence depends on: `stamp` cannot move time on an
    // empty block, `stamp_span` can.
    let mut by_events = JrStream::new(SampleRate::SR_48K);
    by_events.stamp(&[], &mut Vec::new());
    assert_eq!(
        by_events.origin(),
        Frame::new(0),
        "silence stalls the origin"
    );

    let mut by_block = JrStream::new(SampleRate::SR_48K);
    by_block.stamp_span(&[], Samples::new(512), &mut Vec::new());
    assert_eq!(by_block.origin(), Frame::new(512), "the block advanced it");
}

/// Spec: m2-104/7.2.2.1#2, m2-104/7.2.2.1#7
#[test]
#[should_panic(expected = "exceeds the §7.2.2.1 maximum")]
fn an_interval_over_the_spec_maximum_is_rejected() {
    // §7.2.2.1's 250 ms is a `shall`, so a longer interval is not a tuning
    // choice — it is a non-conformant stream.
    JrClockEmitter::with_interval(SampleRate::SR_48K, Duration::from_millis(251));
}

/// Spec: m2-104/7.2.2.1#2, m2-104/7.2.2.1#7
#[test]
fn the_default_interval_is_inside_the_spec_maximum() {
    assert!(JR_CLOCK_INTERVAL < JR_CLOCK_MAX_INTERVAL);
    assert_eq!(JR_CLOCK_MAX_INTERVAL, Duration::from_millis(250));
}

/// The span is one *past* the furthest offset, so the next block's origin
/// does not re-stamp the sample the last event sat on.
#[test]
fn a_blocks_span_is_one_past_its_furthest_offset() {
    let events = [
        at(0, midi::note_on(MidiChannel::FIRST, 60, 0x8000)),
        at(511, midi::note_off(MidiChannel::FIRST, 60, 0)),
    ];
    assert_eq!(JrStamper::block_span(&events), Samples::new(512));
    assert_eq!(
        JrStamper::block_span(&[]),
        Samples::ZERO,
        "an empty block spans nothing"
    );
}

/// A stream advances its own origin, so successive blocks keep climbing.
#[test]
fn a_stream_advances_its_origin_across_blocks() {
    let mut stream = JrStream::new(SampleRate::SR_48K);
    let events = [
        at(0, midi::note_on(MidiChannel::FIRST, 60, 0x8000)),
        at(24_000, midi::note_off(MidiChannel::FIRST, 60, 0)),
    ];

    let mut first = Vec::new();
    stream.stamp(&events, &mut first);
    assert_eq!(jr_timestamp_value(&first[0].1), Some(0));
    assert_eq!(stream.origin(), Frame::new(24_001));

    // The second block's first event is stamped from the new origin, not zero.
    let mut second = Vec::new();
    stream.stamp(&events, &mut second);
    assert_eq!(jr_timestamp_value(&second[0].1), Some(15_625));
    assert_eq!(stream.origin(), Frame::new(48_002));
}

/// The reason the origin lives on the stream: two producers feeding one wire
/// must share it. Stamping both through one `JrStream` keeps the sequence
/// climbing; a per-producer origin would restart each at zero and hand the
/// receiver stamps that walk backwards.
///
/// The events sit at a *block-sized* offset, not zero: a JR tick is 32 µs —
/// about 1.5 samples at 48 kHz — so a one-sample advance rounds to the same
/// tick and would prove nothing either way.
/// Spec: m2-104/7.2.2.2#4
#[test]
fn two_producers_on_one_stream_keep_stamps_monotonic() {
    let mut wire = JrStream::new(SampleRate::SR_48K);

    // One 512-frame block each, the event at the block's end.
    let clock_block = [at(511, midi::note_on(MidiChannel::FIRST, 60, 0x8000))];
    let track_block = [at(511, midi::note_on(MidiChannel::FIRST, 64, 0x8000))];

    let mut from_clock = Vec::new();
    wire.stamp(&clock_block, &mut from_clock);
    let mut from_track = Vec::new();
    wire.stamp(&track_block, &mut from_track);

    let first = jr_timestamp_value(&from_clock[0].1).unwrap();
    let second = jr_timestamp_value(&from_track[0].1).unwrap();
    assert!(
        second > first,
        "the second producer must stamp later than the first: {first} then {second}"
    );
    assert_eq!(wire.origin(), Frame::new(1024), "two 512-frame blocks");
}

/// Spec: m2-104/7.2.2.2#1
#[test]
fn stamp_block_prefixes_each_event() {
    let stamper = JrStamper::new(SampleRate::SR_48K);
    let events = [
        at(0, midi::note_on(MidiChannel::FIRST, 60, 0x8000)),
        at(24_000, midi::note_off(MidiChannel::FIRST, 60, 0)),
    ];
    let mut out = Vec::new();
    stamper.stamp_block(&events, Frame::new(0), &mut out);
    assert_eq!(out.len(), 4, "one timestamp + one event, twice");
    assert_eq!(jr_timestamp_value(&out[0].1), Some(0));
    assert!(matches!(
        out[1].1.note_key_event(),
        Some(NoteKeyEvent::On(_))
    ));
    assert_eq!(jr_timestamp_value(&out[2].1), Some(15_625));
    assert!(out[3].1.is_note_off());
}

/// **A re-rated stream keeps JR time.** One second at 48 kHz is 31 250 ticks
/// (31.25 kHz); re-rated to 44.1 kHz the origin is the same second, 44 100
/// frames, the next stamp continues from tick 31 250, and a half second after it
/// is 15 625 ticks on, where the old rate would read 22 050 frames as 14 355.
#[test]
fn a_re_rated_stream_continues_its_jr_time_at_the_new_rate() {
    let mut stream = JrStream::new(SampleRate::SR_48K).clock(SampleRate::SR_48K);
    let mut out = Vec::new();
    stream.stamp_span(&[], Samples::new(48_000), &mut out);
    assert_eq!(stream.origin(), Frame::new(48_000));
    let mut stream = stream.at_rate(SampleRate::SR_44K1);
    assert_eq!(stream.origin(), Frame::new(44_100), "the same second");
    let events = [
        at(0, midi::note_on(MidiChannel::FIRST, 60, 0x8000)),
        at(22_050, midi::note_off(MidiChannel::FIRST, 60, 0)),
    ];
    stream.stamp(&events, &mut out);
    let stamps: Vec<u16> = out
        .iter()
        .filter_map(|e| jr_timestamp_value(&e.1))
        .collect();
    assert_eq!(stamps, [31_250, 31_250 + 15_625]);
}

// At 48 kHz with the 100 ms default, two 4 800-frame spans clock at frames 0
// and 4 800. Re-rated to 44.1 kHz the last clock is at 4 410, the origin at
// 8 820 and the interval 4 410 frames, so the next clock is owed at 8 820
// exactly, one interval on, and reads 0.2 s: 6 250 ticks.
#[test]
fn a_re_rated_clock_keeps_its_cadence_at_the_new_rate() {
    let mut stream = JrStream::new(SampleRate::SR_48K).clock(SampleRate::SR_48K);
    let mut out = Vec::new();
    stream.stamp_span(&[], Samples::new(4_800), &mut out);
    stream.stamp_span(&[], Samples::new(4_800), &mut out);
    let stream = stream.at_rate(SampleRate::SR_44K1);
    assert_eq!(stream.origin(), Frame::new(8_820));
    let mut clock = stream.emitter.expect("the stream clocks");
    assert_eq!(clock.interval, Samples::new(4_410), "100 ms at 44.1 kHz");
    assert!(clock.due(Frame::new(8_819)).is_none(), "one frame short");
    let due = clock.due(Frame::new(8_820)).expect("one interval on");
    assert_eq!(jr_clock_value(&due), Some(6_250));
}
