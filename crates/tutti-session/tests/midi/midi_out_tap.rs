//! A MIDI sink is an output node an event tap on its output is read from,
//! drained by `Phase::MidiOut` into the sink the host opened:
//! what reaches the wire, in what order and on which frames, what a full tap
//! drops, and the tap following the graph to another rate.
//!
//! Every figure is the MIDI spec's: a 24-PPQN tick at 120 BPM is 1/48 s, so
//! 1 000 frames at 48 kHz.

use std::sync::Arc;
use tutti_rt::Mutex;

use crate::common::{MF, note};
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::{EventCapacity, EventTapOptions, TapAlign, UmpAt};
use tutti_midi_runtime::MidiOut;
use tutti_ump::{MidiGroup, Ump};
use tutti_units::Losses;
use tutti_units::SampleRate;

use tutti_midi_runtime::MidiClipNode;
use tutti_session::{EventInputPort, EventOutputPort, MidiSink, Session, Stamp, Stamping};
use tutti_ump::{Message, System};
use tutti_ump::{MidiChannel, NoteKeyEvent, NoteNumber};
use tutti_units::{Beat, BeatDuration};

const RATE: f64 = 48_000.0;

/// Every batch the output was handed, in order.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<Vec<UmpAt>>>>);

impl MidiOut for Wire {
    fn queue_timed(&self, batch: &[tutti_midi_runtime::Outgoing]) -> tutti_midi_runtime::Taken {
        let events: Vec<tutti_graph::UmpAt> = batch.iter().map(|e| (e.at, e.ump)).collect();
        self.0.lock().push(events.to_vec());
        tutti_midi_runtime::Taken::within(batch, batch.len())
    }
}

impl Wire {
    fn batches(&self) -> Vec<Vec<UmpAt>> {
        std::mem::take(&mut *self.0.lock())
    }
}

/// A session with its clock enabled and a stamping-off sink on `wire`, with the
/// sink's handle, which keeps it open.
fn rig(wire: &Wire) -> (Session, MidiSink) {
    let mut s = Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    let _ = s.update();
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    let sink = s.open_midi_sink(Arc::new(wire.clone())).open();
    let _ = s.update();
    s.play().expect("room");
    (s, sink)
}

fn render(s: &mut Session, frames: usize) {
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..frames.div_ceil(512) {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    }
}

fn ticks(batch: &[UmpAt]) -> Vec<u32> {
    batch
        .iter()
        .filter(|(_, ump)| {
            *ump == Ump::from(Message::System(MidiGroup::FIRST, System::TimingClock))
        })
        .map(|(at, _)| at.get())
        .collect()
}

/// **The clock's events reach the output in order, 1 000 frames apart.** Half a
/// second at 120 BPM, then one update: the drain hands the output one batch
/// whose offsets, in order, are the frames' spacing: Start first, then every
/// 24-PPQN tick exactly one tick (1/48 s, 1 000 frames) after the last.
#[test]
fn the_clock_reaches_the_output_in_order_on_its_frames() {
    let wire = Wire::default();
    let (mut s, _sink) = rig(&wire);
    render(&mut s, 24_000);
    let _ = s.update();
    let batches = wire.batches();
    assert_eq!(batches.len(), 1, "one drain, one batch");
    let batch = &batches[0];
    assert!(
        batch.windows(2).all(|w| w[0].0.get() <= w[1].0.get()),
        "in frame order"
    );
    let ticks = ticks(batch);
    assert!(ticks.len() >= 24, "half a second of ticks: {}", ticks.len());
    assert!(
        ticks.windows(2).all(|w| w[1] - w[0] == 1_000),
        "{ticks:?}: a tick every 1/48 s"
    );
}

/// **A tap that fills drops what it cannot hold and says so.** The tap holds
/// 1 024 events between two updates; the clock is left to send about 1 100
/// ticks with no update. The output then gets exactly 1 024 (the tap's
/// capacity, a non-note-off being refused past it) and the drop count is the
/// remainder, so what was sent is what arrived plus what was counted: the
/// total is read from a second tap with room.
#[test]
fn a_full_tap_counts_what_it_drops() {
    let wire = Wire::default();
    let (mut s, sink) = rig(&wire);
    let clock = s.midi_nodes().unwrap().sync;
    let mut all = s
        .graph()
        .taps()
        .open_event(
            EventOutputPort::new(clock, 0),
            EventTapOptions::new(EventCapacity::of(8_192)).align(TapAlign::Raw),
        )
        .expect("the clock is in the graph");
    let _ = s.update(); // commits the second tap
    render(&mut s, 1_100_000);
    let mut total = 0;
    let mut chunk = [tutti_graph::TapEvent {
        frame: tutti_units::Frame::ZERO,
        ump: Ump([0; 4]),
    }; 256];
    loop {
        let n = all.pop_into(&mut chunk);
        total += n;
        if n < chunk.len() {
            break;
        }
    }
    assert_eq!(all.take_lost(), 0, "the second tap had room");
    assert!(
        total > 1_024,
        "the clock sent more than a tap holds: {total}"
    );

    let _ = s.update();
    let batches = wire.batches();
    assert_eq!(
        batches.len(),
        1,
        "one drain is one batch, whatever its chunks"
    );
    assert_eq!(
        batches[0].len(),
        1_024,
        "the tap's capacity reached the output"
    );
    let gaps: Vec<u32> = ticks(&batches[0]).windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        gaps.iter().all(|&gap| gap == 1_000),
        "the spacing holds across the drain's 256-event chunks"
    );
    assert_eq!(
        sink.losses(),
        Losses::lost((total - 1_024) as u64),
        "the rest is the sink's own: what its node's tap had no room for"
    );
}

/// **Reopening one tap supersedes that tap's reader and no other's.** Two taps
/// of a caller's on the clock; after the graph is re-rated, `reopen_event_tap`
/// of the first gives a reader on the new rate, and the second's reader is
/// still fed (`reopen_event_taps` would have replaced it).
#[test]
fn reopening_one_tap_leaves_the_others_fed() {
    let wire = Wire::default();
    let (mut s, _sink) = rig(&wire);
    let clock = s.midi_nodes().unwrap().sync;
    let mut first = s
        .graph()
        .taps()
        .open_event(
            EventOutputPort::new(clock, 0),
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .unwrap();
    let mut second = s
        .graph()
        .taps()
        .open_event(
            EventOutputPort::new(clock, 0),
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .unwrap();
    let _ = s.update();
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    let reopened = s
        .graph()
        .taps()
        .reopen_event(first.id())
        .expect("the tap is open");
    assert_eq!(reopened.rate(), SampleRate::SR_44K1);
    assert_eq!(reopened.id(), first.id());
    first = reopened;
    // The re-prepare's second half lands on a render, then the commit.
    let _ = s.update();
    render(&mut s, 4_096);
    let _ = s.update();
    render(&mut s, 4_096);
    let mut chunk = [tutti_graph::TapEvent {
        frame: tutti_units::Frame::ZERO,
        ump: Ump([0; 4]),
    }; 64];
    assert!(first.pop_into(&mut chunk) > 0, "the reopened tap is fed");
    assert!(second.pop_into(&mut chunk) > 0, "the other is still fed");
    assert_eq!(second.rate(), SampleRate::of(RATE), "and on its own rate");
}

/// **A clip fed to a sink's node arrives at the sink as its notes, on their
/// frames.** At 120 BPM and 48 kHz a beat is 24 000 frames: a note from beat 1
/// for one beat is a note-on at frame 24 000 and its note-off at 48 000, which a
/// sink sees as offsets 0 and 24 000 of one drain.
#[test]
fn a_clip_fed_to_a_sinks_node_arrives_on_its_frames() {
    let wire = Wire::default();
    let (mut s, out) = rig(&wire);
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages::OFF);
    let clip = s.graph().add(MidiClipNode::new(note(
        60,
        MF,
        Beat::new(1.0),
        BeatDuration::new(1.0),
    )));
    s.graph()
        .set_sources(EventInputPort::new(out.node(), 0), [clip.event_output(0)])
        .expect("the sink's node takes events");
    let _ = s.update();
    render(&mut s, 50_000);
    let _ = s.update();
    let sent: Vec<UmpAt> = wire.batches().concat();
    let kinds: Vec<(u32, bool)> = sent
        .iter()
        .filter_map(|(at, ump)| match ump.note_key_event() {
            Some(tutti_ump::NoteKeyEvent::On(_)) => Some((at.get(), true)),
            Some(tutti_ump::NoteKeyEvent::Off(_)) => Some((at.get(), false)),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, [(0, true), (24_000, false)], "{sent:?}");
}

/// **A sink is open while a handle is held**: its node is in the graph, a
/// clone keeps it open, and a second sink on the same `MidiOut` is a sink of
/// its own.
#[test]
fn a_sink_is_open_while_a_handle_is_held() {
    let first: Arc<dyn MidiOut> = Arc::new(Wire::default());
    let mut s = Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    let sink = s.open_midi_sink(Arc::clone(&first)).open();
    let _ = s.update();
    assert!(s.graph_ref().contains(sink.node()), "the sink's node");

    let node = sink.node();
    let clone = sink.clone();
    drop(sink);
    let _ = s.update();
    let _ = s.update();
    assert!(s.graph_ref().contains(node), "a clone keeps the sink open");

    let other = s.open_midi_sink(first).stamping(Stamping::Jr).open();
    assert_ne!(other.node(), clone.node(), "a sink of its own");
}

/// **Dropping the last handle ends the sink**: its node leaves the graph, and
/// a clip that was wired to it is heard by nobody.
#[test]
fn dropping_the_last_handle_ends_the_sink() {
    let wire = Wire::default();
    let (mut s, sink) = rig(&wire);
    render(&mut s, 4_096);
    let _ = s.update();
    assert!(!wire.batches().is_empty(), "the clock reached the sink");

    let node = sink.node();
    drop(sink);
    let _ = s.update();
    assert!(
        s.graph_ref().contains(node),
        "its node stays until the graph has rendered past the drop"
    );
    render(&mut s, 3 * 512);
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "its node left");
    let _ = wire.batches();
    render(&mut s, 4_096);
    let _ = s.update();
    assert!(wire.batches().is_empty(), "nothing more reaches it");
}

/// **A note off sent just before the last handle of a sink drops reaches the
/// sink**: the sink stays open, its tap drained into it, until the graph has
/// rendered past the drop.
#[test]
fn a_note_off_sent_just_before_the_last_handle_drops_reaches_the_sink() {
    let wire = Wire::default();
    let (mut s, sink) = rig(&wire);
    let node = sink.node();
    let mut keys = s.midi().input(node).expect("the node takes events");
    let _ = s.update();
    let _ = wire.batches();

    keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
        .expect("queued");
    drop(sink);
    let _ = s.update();
    assert!(s.graph_ref().contains(node), "no block has rendered it yet");
    render(&mut s, 3 * 512);
    let _ = s.update();
    assert!(!s.graph_ref().contains(node), "the sink retired");
    let off = wire.batches().iter().flatten().any(|(_, ump)| {
        matches!(ump.note_key_event(), Some(NoteKeyEvent::Off(k)) if k.note().get() == 60)
    });
    assert!(off, "the sink received the note off");
}

/// **A JR-stamped sink stamps at the graph's new rate after a re-rate.** At 44.1
/// kHz a note from beat 1 for one beat (120 BPM) is 22 050 frames long, half a
/// second, which is 15 625 JR ticks (31.25 kHz); a stream still counting at the
/// 48 kHz it was opened at would stamp the same frames 14 355 ticks apart.
#[test]
fn a_stamped_sink_stamps_at_the_new_rate_after_a_re_rate() {
    let wire = Wire::default();
    let mut s = Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    let out = s
        .open_midi_sink(Arc::new(wire.clone()))
        .stamping(Stamping::Jr)
        .open();
    let _ = s.update();
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    let _ = s.update();
    // The re-prepare's second half lands on a render, then the commit.
    render(&mut s, 4_096);
    let _ = s.update();
    render(&mut s, 512);
    let _ = s.update();
    let _ = wire.batches();

    let clip = s.graph().add(MidiClipNode::new(note(
        60,
        MF,
        Beat::new(1.0),
        BeatDuration::new(1.0),
    )));
    s.graph()
        .set_sources(EventInputPort::new(out.node(), 0), [clip.event_output(0)])
        .expect("the sink's node takes events");
    let _ = s.update();
    s.play().expect("room");
    render(&mut s, 50_000);
    let _ = s.update();

    let mut stamp = None;
    let mut notes = Vec::new();
    for (_, ump) in wire.batches().concat() {
        match ump.decode() {
            Ok(Message::Utility(tutti_ump::Utility::JrTimestamp(t))) => stamp = Some(t),
            _ if matches!(
                ump.note_key_event(),
                Some(tutti_ump::NoteKeyEvent::On(_) | tutti_ump::NoteKeyEvent::Off(_))
            ) =>
            {
                notes.push(stamp.expect("every event follows its stamp"));
            }
            _ => {}
        }
    }
    assert_eq!(notes.len(), 2, "{notes:?}");
    let ticks = notes[1].since(notes[0]);
    assert!((15_624..=15_626).contains(&ticks), "{ticks} ticks apart");
}
