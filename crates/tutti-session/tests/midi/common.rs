//! What the MIDI tests share: an offline session, and a [`Tap`] that stands in for a
//! synth, logging every MIDI event its event input receives with its absolute frame.

use std::sync::{Arc, Mutex};

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::{Cx, EventKind, IntoNode, Io, Node, NodeKey, NodeParts, Prepare, Shape, Status};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_session::{Session, SessionNode};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Ump, Voice2};
use tutti_units::{Beat, BeatDuration};

/// The mezzo-forte default: MIDI 2.0's center velocity.
pub const MF: u16 = 0x8000;

/// `(absolute frame, event)` of every MIDI event a [`Tap`] received.
pub type Seen = Arc<Mutex<Vec<(u64, Ump)>>>;

/// A stand-in synth: one MIDI event input, logged with each event's absolute frame.
pub struct Tap(pub Seen);

impl Node for Tap {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            if let EventKind::Midi(ump) = e.kind {
                seen.push((cx.env.frame.get() + u64::from(e.offset.get()), ump));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl IntoNode for Tap {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        IntoNode::into_parts(tutti_graph::Unforkable(self))
    }
}

impl SessionNode for Tap {
    fn reclaimer(_: &()) -> tutti_session::Reclaims {
        tutti_session::Reclaims::Nothing
    }
}

/// An offline session at 48 kHz, stereo: the builder's MIDI nodes in.
pub fn offline() -> (Session, tutti_session::Background) {
    Session::builder().build().expect("builds")
}

/// A tap in `s`'s graph, and what it hears.
pub fn tap(s: &mut Session) -> (NodeKey, Seen) {
    let seen = Seen::default();
    let (node, ()) = s.graph().add(Tap(Arc::clone(&seen)));
    (node.erase(), seen)
}

/// Renders `n` 512-frame blocks.
pub fn blocks(s: &Session, n: usize) {
    let mut buf = vec![0.0f32; 512 * 2];
    for _ in 0..n {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    }
}

/// A Note On at `velocity` on `start` and its Note Off `duration` later, as timed events.
pub fn note(number: u8, velocity: u16, start: Beat, duration: BeatDuration) -> [TimedMidiEvent; 2] {
    [
        TimedMidiEvent::new(
            start,
            Message::midi2(
                MidiChannel::FIRST,
                Voice2::note_on(NoteNumber::of(number), NoteVelocity::new(velocity)),
            ),
        )
        .unwrap(),
        TimedMidiEvent::new(
            start + duration,
            Message::midi2(
                MidiChannel::FIRST,
                Voice2::note_off(NoteNumber::of(number), NoteVelocity::MIN),
            ),
        )
        .unwrap(),
    ]
}

/// Starts the transport.
pub fn play(s: &Session) {
    s.play().expect("room");
}

/// A session over a fake OS with one device, `Keys`, declared as the input port
/// `"input"` and loaded (hearing Groups up to `groups`, the first alone with
/// `None`): the port's input node, which route rules wire from.
#[cfg(feature = "midi-hardware")]
pub fn with_input(groups: Option<tutti_ump::MidiGroup>) -> (Session, NodeKey) {
    use tutti_midi_hardware::test_support::{FakeBackend, persistent_endpoint};
    let (backend, _fake) =
        FakeBackend::build_with(vec![persistent_endpoint(1, "Keys")], vec![], usize::MAX);
    let mut builder = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new());
    if let Some(last) = groups {
        builder = builder.midi_groups(last);
    }
    let (mut s, _bg) = builder.build().expect("builds");
    let _ = s.update();
    let keys = s.midi().devices().remove(0);
    s.pool(tutti_session::midi_input("input", keys))
        .expect("declares");
    let _ = s.update();
    let node = s.midi_input(&"input").expect("loaded").node();
    (s, node)
}
