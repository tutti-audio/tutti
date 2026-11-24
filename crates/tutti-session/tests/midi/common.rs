//! What the MIDI tests share: an offline session, and a [`Tap`] that stands in for a
//! synth, logging every MIDI event its event input receives with its absolute frame.

use std::sync::Arc;
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_fixtures::midi;
use tutti_graph::TransportPosition;
use tutti_graph::UmpAt;
use tutti_graph::{Cx, EventKind, IntoNode, Io, Node, NodeKey, NodeParts, Prepare, Shape, Status};
use tutti_midi_runtime::{ClipEvents, MidiOut, Outgoing, Taken, TimedMidiEvent};
use tutti_rt::Mutex;
use tutti_session::Session;
use tutti_ump::{Address, CCNumber, MidiGroup};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Ump, Voice2};
use tutti_units::Bpm;
use tutti_units::{Beat, BeatDuration};

/// The mezzo-forte default: MIDI 2.0's center velocity.
pub const MF: u16 = 0x8000;

/// `(absolute frame, event)` of every MIDI event a [`Tap`] received.
pub type Seen = tutti_fixtures::Record<(u64, Ump)>;

/// A stand-in synth: one MIDI event input, logged with each event's absolute frame.
pub struct Tap(pub Seen);

impl Tap {
    const SHAPE: Shape = Shape::EMPTY.events(1, 0);
    const IN: tutti_graph::InEvents = Self::SHAPE
        .event_input(0)
        .expect("one event input is declared");
}

impl Node for Tap {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(Self::IN) {
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

/// An offline session at 48 kHz, stereo: the builder's MIDI nodes in.
pub fn offline() -> Session {
    Session::builder().build().expect("builds")
}

/// A [`MidiOut`] that keeps every batch it is handed, in order.
#[derive(Clone, Default)]
pub struct Wire(Arc<Mutex<Vec<Vec<UmpAt>>>>);

impl MidiOut for Wire {
    fn queue_timed(&self, batch: &[Outgoing]) -> Taken {
        self.0
            .lock()
            .push(batch.iter().map(|e| (e.at, e.ump)).collect());
        Taken::within(batch, batch.len())
    }
}

impl Wire {
    /// The batches handed since the last call.
    pub fn batches(&self) -> Vec<Vec<UmpAt>> {
        std::mem::take(&mut *self.0.lock())
    }

    /// Every event handed so far, in order: it keeps them.
    pub fn events(&self) -> Vec<UmpAt> {
        self.0.lock().iter().flatten().copied().collect()
    }
}

/// A tap in `s`'s graph, and what it hears.
pub fn tap(s: &mut Session) -> (NodeKey, Seen) {
    let seen = tutti_fixtures::Record::new();
    let node = s.graph().add(Tap(seen.clone()));
    (node.erase(), seen)
}

/// Renders `n` 512-frame blocks.
pub fn blocks(s: &mut Session, n: usize) {
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

/// A session and the handle of its one input port, which keeps the port open
/// for as long as the session is used.
#[cfg(feature = "midi-hardware")]
pub struct Keyed {
    session: Session,
    keys: tutti_session::MidiInput,
}

#[cfg(feature = "midi-hardware")]
impl Keyed {
    /// A session over `session` whose input port is `keys`.
    pub fn new(session: Session, keys: tutti_session::MidiInput) -> Self {
        Self { session, keys }
    }

    /// The port's input node: what a route rule or a follower reads.
    pub fn input(&self) -> NodeKey {
        self.keys.node()
    }
}

#[cfg(feature = "midi-hardware")]
impl std::ops::Deref for Keyed {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}

#[cfg(feature = "midi-hardware")]
impl std::ops::DerefMut for Keyed {
    fn deref_mut(&mut self) -> &mut Session {
        &mut self.session
    }
}

/// The ports of `layout`'s input `input` that carry `channels` of `group`, and
/// the Group's channelless port.
#[cfg(feature = "midi-hardware")]
pub fn input_ports(
    layout: tutti_midi_runtime::MidiInputPorts,
    input: NodeKey,
    group: u8,
    channels: &[u8],
) -> std::collections::BTreeSet<tutti_graph::EventOutputPort> {
    use tutti_ump::{MidiChannel, MidiGroup};
    let group = MidiGroup::of(group);
    channels
        .iter()
        .map(|&c| layout.channel(input, group, MidiChannel::of(c)).unwrap())
        .chain(layout.channelless(input, group))
        .collect()
}

/// Every port of `layout`'s input `input`.
#[cfg(feature = "midi-hardware")]
pub fn every_input_port(
    layout: tutti_midi_runtime::MidiInputPorts,
    input: NodeKey,
) -> std::collections::BTreeSet<tutti_graph::EventOutputPort> {
    layout.listening(input, None, None).collect()
}

/// A session over a fake OS with one device, `Keys`, opened as an input port
/// (hearing Groups up to `groups`, the first alone with `None`): the port's
/// input node, which route rules wire from.
#[cfg(feature = "midi-hardware")]
pub fn with_input(groups: Option<tutti_ump::MidiGroup>) -> (Keyed, NodeKey) {
    use tutti_midi_hardware::test_support::{FakeBackend, persistent_endpoint};
    let (backend, _fake) =
        FakeBackend::build_with(vec![persistent_endpoint(1, "Keys")], vec![], usize::MAX);
    let mut builder = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new());
    if let Some(last) = groups {
        builder = builder.midi_groups(last);
    }
    let mut s = builder.build().expect("builds");
    let _ = s.update();
    let keys = s.midi().devices().remove(0);
    let port = s.open_input(keys).expect("opens");
    let node = port.node();
    (Keyed::new(s, port), node)
}

/// Opening a port the way an app does, over a fake OS whose start is ready at
/// once: begin the open, poll its wait, apply what it resolved to.
#[cfg(feature = "midi-hardware")]
pub trait OpenPorts {
    /// The input port of `saved`.
    fn open_input(
        &mut self,
        saved: tutti_midi_device::DeviceInfo,
    ) -> Result<tutti_session::MidiInput, tutti_session::MidiPortError>;

    /// The output port of `saved`.
    fn open_output(
        &mut self,
        saved: tutti_midi_device::DeviceInfo,
    ) -> Result<tutti_session::MidiOutput, tutti_session::MidiPortError>;

    /// The output port of `saved`, sending Active Sensing when `on`.
    fn open_sensing_output(
        &mut self,
        saved: tutti_midi_device::DeviceInfo,
        on: bool,
    ) -> Result<tutti_session::MidiOutput, tutti_session::MidiPortError>;
}

/// What `future` resolved to when polled once, or `None` while it is pending.
#[cfg(feature = "midi-hardware")]
pub fn polled<T>(future: impl std::future::Future<Output = T>) -> Option<T> {
    use std::task::{Context, Poll, Waker};
    match std::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(out) => Some(out),
        Poll::Pending => None,
    }
}

/// What `wait` resolved to on its first poll: a fake backend has nothing to
/// start.
#[cfg(feature = "midi-hardware")]
pub fn ready<T, E: std::fmt::Debug>(wait: impl std::future::Future<Output = Result<T, E>>) -> T {
    polled(wait)
        .expect("a fake backend's start is ready on the first poll")
        .expect("a fake backend starts at once")
}

#[cfg(feature = "midi-hardware")]
impl OpenPorts for Session {
    fn open_input(
        &mut self,
        saved: tutti_midi_device::DeviceInfo,
    ) -> Result<tutti_session::MidiInput, tutti_session::MidiPortError> {
        let opened = ready(self.open_midi_input(saved)?);
        self.apply(opened)
    }

    fn open_output(
        &mut self,
        saved: tutti_midi_device::DeviceInfo,
    ) -> Result<tutti_session::MidiOutput, tutti_session::MidiPortError> {
        self.open_sensing_output(saved, false)
    }

    fn open_sensing_output(
        &mut self,
        saved: tutti_midi_device::DeviceInfo,
        on: bool,
    ) -> Result<tutti_session::MidiOutput, tutti_session::MidiPortError> {
        let opened = ready(self.open_midi_output(saved)?.active_sensing(on));
        self.apply(opened)
    }
}

pub fn program(n: u8) -> Ump {
    midi::program_change(Address::new(MidiGroup::FIRST, MidiChannel::FIRST), n, None)
}

pub fn volume() -> Ump {
    midi::cc(
        Address::new(MidiGroup::FIRST, MidiChannel::FIRST),
        CCNumber::VOLUME,
        1 << 30,
    )
}

/// A clip with the header `[program 1, volume]`, a Program Change 5 at beat 0.001 (24 frames)
/// beat 0 and a note at beat 1 (24 000 frames at 120 BPM and 48 kHz).
pub fn headed_clip() -> ClipEvents {
    let events =
        std::iter::once(TimedMidiEvent::new(Beat::new(0.001), program(5)).expect("finite"))
            .chain(note(60, MF, Beat::new(1.0), BeatDuration::new(0.5)));
    let events: ClipEvents = events.collect();
    events.header(
        [program(1), volume()].map(|u| TimedMidiEvent::new(Beat::new(0.0), u).expect("finite")),
    )
}

/// One frame of the audio side, the `n`th, on a rolling transport that has
/// run `n` frames (a beat is 24 000 frames at 120 BPM) from beat 0.01, past the
/// clip's Program Change 5 at beat 0.001.
pub fn tick(side: &mut tutti_session::AudioSide, n: u32) {
    side.set_transport(TransportPosition::new(
        true,
        Bpm::of(120.0),
        Beat::new(0.01 + f64::from(n) / 24_000.0),
        None,
    ));
    let mut out = [0.0f32; 2];
    side.tick(InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
}
