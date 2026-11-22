//! A sink is sent at the time each event's frame is heard, not when
//! the host happens to drain it: a session on `ManualStreamDriver`s, whose
//! callbacks report the audio output's latency as they are told to.
//!
//! Every figure is arithmetic on the MIDI spec's tick (24 per quarter note,
//! 1/48 s at 120 BPM, so 1 000 frames at 48 kHz) and on `Instant`s the test
//! reads around its own calls.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_platform::Instant;

use tutti_buffers::ChannelLayout;
use tutti_engine::{ManualStream, ManualStreamDriver, OutputSpec};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_midi_runtime::{MidiOut, Outgoing, OutputLatency, OutputOffset, SendAt, Taken};
use tutti_session::{EventInputPort, LiveSession, Session};
use tutti_ump::{Message, MidiGroup, System, Ump};
use tutti_ump::{MidiChannel, NoteNumber, NoteVelocity, Voice2};
use tutti_units::{Beat, SampleRate, Samples, Seconds};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 512;

/// Every event the output was told to send, with when.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<Outgoing>>>);

impl MidiOut for Wire {
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        self.0.lock().unwrap().extend_from_slice(events);
        Taken::within(events, events.len())
    }
}

impl Wire {
    fn take(&self) -> Vec<Outgoing> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    /// The send times of the ticks among `events`, in order; panics on a tick
    /// sent `Now`.
    fn tick_times(events: &[Outgoing]) -> Vec<Instant> {
        events
            .iter()
            .filter(|e| e.ump == Ump::from(Message::System(MidiGroup::FIRST, System::TimingClock)))
            .map(|e| match e.send {
                SendAt::At(at) => at,
                SendAt::Now => panic!("a tick was sent now"),
            })
            .collect()
    }
}

fn spec() -> OutputSpec {
    OutputSpec::new(
        SampleRate::of(RATE),
        ChannelLayout::STEREO,
        tutti_buffers::SampleFormat::F32,
    )
}

/// A session on a manual stream with its clock running into `wire`.
fn rig(wire: &Wire) -> (LiveSession, ManualStream, tutti_session::MidiSink) {
    let (driver, stream) = ManualStreamDriver::new();
    let mut s = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec(), driver)
        .expect("builds with no device");
    let _ = s.update();
    let sink = s.open_midi_sink(Arc::new(wire.clone())).open();
    let _ = s.update();
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    s.play().expect("room");
    (s, stream, sink)
}

/// Renders `blocks` blocks reporting `delay` as the audio output's latency, the
/// last one after reading `Instant::now()` into the return.
fn render(stream: &ManualStream, blocks: usize, delay: Option<Duration>) -> Instant {
    let one = || match delay {
        Some(delay) => stream.render_block_delayed(Samples::new(BLOCK), delay),
        None => stream.render_block(Samples::new(BLOCK)),
    };
    for _ in 1..blocks {
        one().expect("the stream is open");
    }
    let last = Instant::now();
    one().expect("the stream is open");
    last
}

/// **Events reach the output at the instants their frames are heard, spaced by
/// the frames.** With the stream reporting 1.5 s of audio latency and a second
/// of clock rendered, every tick is in the future, and ticks 1 000 frames apart
/// are 20.833 ms apart in `SendAt::At`, not all `Now`.
#[test]
fn the_clock_is_sent_at_the_time_of_its_frames() {
    let wire = Wire::default();
    let (mut s, stream, _sink) = rig(&wire);
    let latency = Duration::from_millis(1500);
    let last_block = render(&stream, 100, Some(latency));
    let _ = s.update();
    let sent = wire.take();
    let ticks = Wire::tick_times(&sent);
    assert!(ticks.len() > 20, "ticks to measure: {}", ticks.len());
    for pair in ticks.windows(2) {
        let gap = pair[1].duration_since(pair[0]).as_secs_f64();
        assert!((gap - 1_000.0 / RATE).abs() < 1e-5, "ticks {gap} s apart");
    }
    // The last block began at `last_block` (within the call's own few ms), and
    // every frame of it is heard its offset after, plus the latency.
    let newest = *ticks.last().unwrap();
    let expected = last_block + latency;
    assert!(
        newest >= expected && newest <= expected + Duration::from_millis(11 + 5),
        "the newest tick is {:?} after the last block began, plus the latency",
        newest.checked_duration_since(last_block)
    );
}

/// **An output's offset shifts every event by it.** The same clock in two
/// sessions, one with a +0.25 s offset: the newest tick leaves 250 ms later
/// relative to the instant its block began, which the test read the same way
/// for both.
#[test]
fn an_output_offset_shifts_the_send_times() {
    let latency = Some(Duration::from_millis(1500));
    let newest_after_block = |offset: f64| {
        let wire = Wire::default();
        let (mut s, stream, _sink) = rig(&wire);
        let target = s.midi().output_latency();
        s.set(
            target.to(OutputLatency::new(
                Seconds::new(0.0),
                OutputOffset::new(Seconds::new(offset)).unwrap(),
            )
            .unwrap()),
        )
        .unwrap();
        let last_block = render(&stream, 100, latency);
        let _ = s.update();
        let ticks = Wire::tick_times(&wire.take());
        ticks
            .last()
            .unwrap()
            .duration_since(last_block)
            .as_secs_f64()
    };
    let shift = newest_after_block(0.25) - newest_after_block(0.0);
    assert!((shift - 0.25).abs() < 0.02, "shifted by {shift} s");
}

/// **A written output latency is the terms' own on a device clock.** A device
/// session's terms start at the default (no latency, no offset); after a write
/// of 12 ms and a +3 ms offset and one update, `output_terms` reports exactly
/// that latency, beside the audio output's.
#[test]
fn a_written_output_latency_is_the_terms_own_on_a_device_clock() {
    let wire = Wire::default();
    let (mut s, stream, _sink) = rig(&wire);
    let before = s.midi().output_terms().expect("a device session has terms");
    assert_eq!(before.latency, OutputLatency::default());

    let written = OutputLatency::new(
        Seconds::new(0.012),
        OutputOffset::new(Seconds::new(0.003)).unwrap(),
    )
    .unwrap();
    let target = s.midi().output_latency();
    s.set(target.to(written)).unwrap();
    let _ = render(&stream, 2, Some(Duration::from_millis(40)));
    let _ = s.update();
    let terms = s.midi().output_terms().unwrap();
    assert_eq!(
        terms.latency, written,
        "the write is the terms' own latency"
    );
    assert_eq!(terms.audio_output, Some(Seconds::new(0.04)));
    assert!(terms.is_complete());
}

/// **While the audio latency is unreported, events are timed with the terms
/// known, and the terms say so.** Before the stream's first callback and after
/// one that reports nothing, `output_terms` is incomplete and ticks leave at
/// the instant their frames are rendered (none a second out); once a callback
/// reports 40 ms the term is in.
#[test]
fn an_unreported_audio_latency_is_left_out_and_reported() {
    let wire = Wire::default();
    let (mut s, stream, _sink) = rig(&wire);
    let terms = s.midi().output_terms().expect("a device session has terms");
    assert_eq!(terms.audio_output, None);
    assert!(!terms.is_complete());

    let last_block = render(&stream, 100, None);
    let _ = s.update();
    let sent = wire.take();
    assert!(!s.midi().output_terms().unwrap().is_complete());
    let scheduled: Vec<Instant> = sent
        .iter()
        .filter_map(|e| match e.send {
            SendAt::At(at) => Some(at),
            SendAt::Now => None,
        })
        .collect();
    assert!(!scheduled.is_empty(), "the newest block's events are ahead");
    assert!(
        scheduled
            .iter()
            .all(|at| *at <= last_block + Duration::from_millis(40)),
        "with no audio latency known, nothing is scheduled beyond its own frame"
    );

    let last_block = render(&stream, 2, Some(Duration::from_millis(40)));
    let terms = s.midi().output_terms().unwrap();
    assert_eq!(terms.audio_output, Some(Seconds::new(0.04)));
    assert!(terms.is_complete());
    let _ = s.update();
    let scheduled: Vec<Instant> = wire
        .take()
        .iter()
        .filter_map(|e| match e.send {
            SendAt::At(at) => Some(at),
            SendAt::Now => None,
        })
        .collect();
    assert!(
        scheduled
            .iter()
            .any(|at| *at >= last_block + Duration::from_millis(40)),
        "the reported 40 ms is in the send time"
    );
}

/// **A clip's notes reach a sink at the instants their frames are heard.** A
/// note from beat 1 for one beat at 120 BPM and 48 kHz is a note-on at frame
/// 24 000 and a note-off at 48 000, so the two leave exactly 0.5 s apart in
/// `SendAt::At`, and the note-on leaves at the time its frame is heard: the
/// last block's instant plus the 1.5 s the stream reports, less the 26 688
/// frames (0.556 s) between the note and that block's start, less the sink's
/// own 5 ms latency.
#[test]
fn a_clips_notes_are_sent_when_their_frames_are_heard() {
    let wire = Wire::default();
    let (mut s, stream, sink) = rig(&wire);
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages::OFF);
    let target = s.midi().output_latency();
    s.set(
        target.to(OutputLatency::new(
            Seconds::new(0.005),
            OutputOffset::new(Seconds::new(0.0)).unwrap(),
        )
        .unwrap()),
    )
    .unwrap();
    let note = |on: bool, at: Beat| {
        let voice = if on {
            Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0xC800))
        } else {
            Voice2::note_off(NoteNumber::of(60), NoteVelocity::MIN)
        };
        TimedMidiEvent::new(at, Message::midi2(MidiChannel::FIRST, voice)).unwrap()
    };
    let clip = s.graph().add(MidiClipNode::new([
        note(true, Beat::new(1.0)),
        note(false, Beat::new(2.0)),
    ]));
    s.graph()
        .set_sources(EventInputPort::new(sink.node(), 0), [clip.event_output(0)])
        .expect("the sink's node takes events");
    let _ = s.update();
    let latency = Duration::from_millis(1500);
    let last_block = render(&stream, 100, Some(latency));
    let _ = s.update();
    let sends: Vec<(bool, Instant)> = wire
        .take()
        .iter()
        .filter_map(|e| match (e.ump.note_key_event(), e.send) {
            (Some(tutti_ump::NoteKeyEvent::On(_)), SendAt::At(at)) => Some((true, at)),
            (Some(tutti_ump::NoteKeyEvent::Off(_)), SendAt::At(at)) => Some((false, at)),
            _ => None,
        })
        .collect();
    let [(true, on), (false, off)] = sends[..] else {
        panic!("a note-on then a note-off, scheduled: {sends:?}")
    };
    let gap = off.duration_since(on).as_secs_f64();
    assert!((gap - 0.5).abs() < 1e-6, "{gap}");
    // The last block began at frame 99 * 512 = 50 688.
    let heard = last_block + latency
        - Duration::from_secs_f64(f64::from(50_688 - 24_000) / RATE)
        - Duration::from_millis(5);
    assert!(
        on >= heard && on <= heard + Duration::from_millis(16),
        "the note-on is {:?} after the instant its frame is heard",
        on.checked_duration_since(heard)
    );
}
