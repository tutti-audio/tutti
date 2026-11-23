//! The nodes at the graph's MIDI border:
//! [`MidiInputNode`] sends the events of its device inlets out of their channels' ports,
//! translated; an event tap hands what a port carries back to a control
//! thread; [`SyncOut`] sends
//! Beat Clock, timecode and Machine Control on the frames the transport reaches, a
//! start inside a block included.

use tutti_buffers::MaxBlock;

use crate::common;
use tutti_graph::{Stamp, UmpAt};

use std::sync::Arc;
use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::{
    Editor, EventCapacity, EventEdge, EventInletNode, EventInputPort, EventOutputPort, Executor,
    ForkTarget, Offset, Prepare, TransportChanges, TransportPosition,
};
use tutti_midi_protocol::{
    MpeMode, MpeZoneConfig, MpeZones, MtcRate, PitchBendSensitivity, Recipient, channel_voice,
};
use tutti_midi_runtime::{
    DeviceGeneration, MIDI_INPUT_EVENT_CAPACITY, MachineControlEvent, MachineControlFollower,
    MidiInputNode, MidiInputPorts, MidiOutNode, MpeTransformNode, SyncMessages, SyncOut,
};
use tutti_rt::Publisher;
use tutti_ump::{
    Address, CCNumber, Message, MidiChannel, MidiGroup, MpeChannels, MpeZoneSpan, NoteKeyEvent,
    NoteNumber, NoteVelocity, System, U7, U14, Ump, Voice2,
};
use tutti_units::{Beat, Bpm, Frame, SampleRate, Samples};

/// The note a channel voice message addresses, either protocol.
fn note_of(ump: Ump) -> Option<u8> {
    channel_voice(ump)?.1.note().map(NoteNumber::get)
}

const G: MidiGroup = MidiGroup::FIRST;

/// The layout of an input node that hears the first Group.
const FIRST: MidiInputPorts = MidiInputPorts::FIRST_GROUP;

/// The input node's key in [`rig`].
const SOURCE: NodeKey = NodeKey(1);

const RATE: f64 = 48_000.0;

use tutti_fixtures::{EventRig, Seen};

/// A sync node on the first Group, sending `messages`.
fn sync(messages: SyncMessages) -> SyncOut {
    SyncOut::new(G).sending(messages)
}

/// A source at key 1 whose `ports` event outputs each feed the sink's input
/// of the same number, blocks of at most `max`.
fn rig<N: tutti_graph::IntoNode>(
    source: N,
    ports: u16,
    max: usize,
) -> (Editor, Executor, N::Controls, Seen) {
    rig_with(source, ports, max, |_| {})
}

/// [`rig`], and `extra` adds what feeds the source before the commit.
fn rig_with<N: tutti_graph::IntoNode>(
    source: N,
    ports: u16,
    max: usize,
    extra: impl FnOnce(&mut Editor),
) -> (Editor, Executor, N::Controls, Seen) {
    let mut rig = EventRig::new(RATE, max, ports);
    let controls = rig.editor.insert(NodeKey(1), "source", source).controls;
    extra(&mut rig.editor);
    for p in 0..ports {
        rig.feed(NodeKey(1), p, p);
    }
    rig.commit();
    (rig.editor, rig.exec, controls, rig.seen)
}

/// An input node and the device inlet that feeds it (key 3), over
/// `ports` sink inputs.
fn input(
    input: Input,
    ports: u16,
    max: usize,
) -> (
    Editor,
    Executor,
    tutti_midi_runtime::MidiInputControls,
    Seen,
) {
    let Input { node, inlet } = input;
    rig_with(node, ports, max, |ed| {
        let key = NodeKey(3);
        ed.insert(key, "inlet", inlet);
        ed.spec_mut().connect_events(
            EventInputPort::new(NodeKey(1), 0),
            EventEdge::Direct(EventOutputPort::new(key, 0)),
        );
    })
}

fn block(exec: &mut Executor, len: usize) {
    let mut out = vec![0.0f32; len];
    exec.process(len, &TransportPosition::default(), &[], &mut [&mut out[..]]);
}

/// What a device sends, one batch for each block: each event on its offset in
/// its block (the blocks are 512 frames).
struct Script(Vec<Vec<UmpAt>>);

impl Script {
    fn new(batches: Vec<Vec<UmpAt>>) -> Self {
        Self(batches)
    }
}

/// An input node, and the inlet its device's thread has pushed the script
/// into, every event for its frame; an inlet holds it until its block.
struct Input {
    node: MidiInputNode,
    inlet: EventInletNode,
}

impl Input {
    fn new(script: Script) -> Self {
        let (inlet, mut writer) = EventInletNode::pair(EventCapacity::of(4096));
        for (block, batch) in script.0.iter().enumerate() {
            for &(offset, ump) in batch {
                let frame = Frame::new(512 * block as u64 + u64::from(offset.get()));
                writer.push(Stamp::Frame(frame), ump).expect("room");
            }
        }
        Self {
            node: MidiInputNode::new(),
            inlet,
        }
    }

    fn hearing(mut self, ports: MidiInputPorts) -> Self {
        self.node = self.node.hearing(ports);
        self
    }

    fn mpe(mut self, mode: &Publisher<MpeMode>) -> Self {
        self.node = self.node.mpe(mode);
        self
    }
}

/// `ump` at frame `offset`, which may lie past the 512-frame blocks these
/// tests run: it is built against a block long enough to hold it.
fn at(offset: usize, ump: Ump) -> UmpAt {
    (Offset::of(Samples::new(offset), Samples::new(1 << 20)), ump)
}

/// A note-on with 7-bit velocity 100, upscaled (M2-115), at `offset`.
fn note(channel: u8, note: u8, offset: usize) -> UmpAt {
    at(
        offset,
        midi::note_on(
            MidiChannel::of(channel),
            note,
            NoteVelocity::from(U7::of(100)).get(),
        ),
    )
}

/// A MIDI 1.0 CC.
fn cc_ev(channel: u8, control: CCNumber, value: u8) -> UmpAt {
    cc_at(0, channel, control, value)
}

/// A MIDI 1.0 CC at frame `offset`.
fn cc_at(offset: usize, channel: u8, control: CCNumber, value: u8) -> UmpAt {
    at(
        offset,
        midi::m1_cc(MidiChannel::of(channel), control, U7::of(value)),
    )
}

/// **Each event leaves on its channel's port, in offset order.** A note on
/// channel 9 before one on channel 0 in arrival order but after it in time,
/// and a Timing Clock (no channel): channel 0's on port 0 at 10, channel
/// 9's on port 9 at 40, the clock on the channelless port at 20.
#[test]
fn each_event_leaves_on_its_channels_port() {
    let clock = at(
        20,
        Ump::from(Message::System(MidiGroup::FIRST, System::TimingClock)),
    );
    let wire = Script::new(vec![vec![note(9, 64, 40), note(0, 60, 10), clock]]);
    let (_ed, mut exec, _c, seen) = input(Input::new(wire), FIRST.count(), 512);
    block(&mut exec, 512);
    let got: Vec<(EventOutputPort, u64)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| (EventOutputPort::new(SOURCE, s.0), s.1))
        .collect();
    let channel = |c: u8| FIRST.channel(SOURCE, G, MidiChannel::of(c)).unwrap();
    assert_eq!(
        got,
        vec![
            (channel(0), 10),
            (channel(9), 40),
            (FIRST.channelless(SOURCE, G).unwrap(), 20)
        ]
    );
}

/// **A Group's channels leave on ports of their own** (M2-104 section 2.1.2,
/// Appendix B.1): channel 1 of Group 1 and channel 1 of Group 2 arrive on
/// different ports, so a synth wired to one hears the other never; a Group
/// the node was not built to hear is not delivered; the fork of such a node
/// has the same ports.
/// Spec: m2-104/2.1.2#2
#[test]
fn a_groups_channels_leave_on_their_own_ports() {
    let group = |g: u8, channel: u8, n: u8| {
        at(
            0,
            midi::note_on(
                Address::new(MidiGroup::of(g), MidiChannel::of(channel)),
                n,
                NoteVelocity::from(U7::of(100)).get(),
            ),
        )
    };
    let ports = MidiInputPorts::through(MidiGroup::of(1));
    let wire = Script::new(vec![vec![
        group(0, 1, 60),
        group(1, 1, 61),
        group(2, 1, 62),
    ]]);
    let (live, mut exec, _c, seen) = input(Input::new(wire).hearing(ports), ports.count(), 512);
    block(&mut exec, 512);
    let got: Vec<(EventOutputPort, u8)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| (EventOutputPort::new(SOURCE, s.0), note_of(s.2).unwrap()))
        .collect();
    let channel_one = |g: u8| ports.channel(SOURCE, MidiGroup::of(g), MidiChannel::of(1));
    assert_eq!(
        got,
        vec![(channel_one(0).unwrap(), 60), (channel_one(1).unwrap(), 61)]
    );
    let prepare = *live.prepare();
    assert!(live.fork(ForkTarget::MainOutput, prepare).is_ok());
}

/// **An (N)RPN run split across blocks assembles into one controller.**
/// The parameter select arrives in one block, its Data Entry (CC 6, then the
/// CC 38 that sends it, M2-104 D.3.3) in the next: one Registered Controller
/// arrives, on channel 3's port.
#[test]
fn an_rpn_run_split_across_blocks_is_assembled() {
    let wire = Script::new(vec![
        vec![
            cc_ev(3, CCNumber::RPN_MSB, 0),
            cc_ev(3, CCNumber::RPN_LSB, 6),
        ],
        vec![
            cc_ev(3, CCNumber::DATA_ENTRY, 10),
            cc_ev(3, CCNumber::DATA_ENTRY_LSB, 0),
        ],
    ]);
    let (_ed, mut exec, _c, seen) = input(Input::new(wire), FIRST.count(), 512);
    block(&mut exec, 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let (port, frame, ev) = seen[0];
    assert_eq!((port, frame), (3, 512));
    // MIDI 2.0 channel voice (type 4), Registered Controller (status 0x2).
    assert!(
        matches!(
            ev.decode(),
            Ok(Message::Midi2(_, Voice2::Registered { .. }))
        ),
        "{ev:?}"
    );
}

/// A Registered Controller on channel 3, as a received `Voice2` says.
fn is_registered(ev: Ump) -> bool {
    matches!(
        ev.decode(),
        Ok(Message::Midi2(_, Voice2::Registered { .. }))
    )
}

/// **A device's (N)RPN select and Data Entry make a run** (M2-104 D.3.3: a Data
/// Entry completes the parameter its sender selected). One device selects RPN
/// 0/6 on channel 3 and sends the entry: one Registered Controller is made.
/// (A second device is a second node with a translator of its own, so its Data
/// Entry cannot complete this device's select.)
#[test]
fn a_devices_select_and_entry_make_a_run() {
    // One device sending both: the run completes.
    let together = vec![vec![
        cc_ev(3, CCNumber::RPN_MSB, 0),
        cc_ev(3, CCNumber::RPN_LSB, 6),
        cc_ev(3, CCNumber::DATA_ENTRY, 10),
        cc_ev(3, CCNumber::DATA_ENTRY_LSB, 0),
    ]];
    let (_ed, mut exec, _c, seen) = input(Input::new(Script::new(together)), FIRST.count(), 512);
    block(&mut exec, 512);
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|s| is_registered(s.2))
            .count(),
        1,
        "one device's select and entry make a run"
    );
}

/// **A departed device's generation is handled only once a block has run**: the node
/// pushes the release in a block, then says so. After `depart` nothing is
/// handled until that block has gone, and the block's first frame carries the
/// Note Off.
#[test]
fn a_departed_devices_generation_is_handled_only_after_a_block_has_run() {
    let (mut writer, generation) = DeviceGeneration::new();
    writer.arrive();
    let wire = Script::new(vec![vec![note(0, 60, 10)], vec![]]);
    let (_ed, mut exec, _c, seen) = input(following(wire, &generation), FIRST.count(), 512);
    block(&mut exec, 512);
    assert!(generation.is_idle(), "the block handled the arrival");
    assert_eq!(heard(&seen).len(), 1, "the key is down");
    writer.depart();
    assert!(!generation.is_idle(), "the departure is owed a block");
    block(&mut exec, 512);
    assert!(generation.is_idle(), "the block released and said so");
    assert_eq!(heard(&seen)[1], (512, "off 60".to_string()));
}

/// **A release that does not fit the block is owed, and the generation is handled only
/// when it has gone**: a device holds 800 keys, more than the port's note-off
/// room (584 at 512 events). Its departure's block sends what fits and the
/// generation stays unhandled; the next block's first frame sends the rest, and only
/// then is the generation handled.
#[test]
fn a_release_that_does_not_fit_is_owed_and_is_handled_when_it_has_gone() {
    let keys = |first: u8| -> Vec<UmpAt> {
        (0..400usize)
            .map(|i| note(first + (i / 100) as u8, (i % 100) as u8, i))
            .collect()
    };
    let (mut writer, generation) = DeviceGeneration::new();
    writer.arrive();
    let wire = Script::new(vec![keys(0), keys(4), vec![], vec![]]);
    let (_ed, mut exec, _c, seen) = input(following(wire, &generation), FIRST.count(), 512);
    let offs = |from: u64, to: u64| {
        heard(&seen)
            .iter()
            .filter(|(frame, what)| (from..to).contains(frame) && what.starts_with("off"))
            .count()
    };
    block(&mut exec, 512);
    block(&mut exec, 512);
    assert_eq!(heard(&seen).len(), 800, "every key is down");

    writer.depart();
    block(&mut exec, 512);
    assert_eq!(offs(1024, 1536), 584, "what the port has room for");
    assert!(!generation.is_idle(), "216 are still owed");
    block(&mut exec, 512);
    assert_eq!(
        offs(1536, 2048),
        216,
        "the rest, at the next block's first frame"
    );
    assert!(
        heard(&seen)
            .iter()
            .filter(|(frame, what)| *frame >= 1536 && what.starts_with("off"))
            .all(|(frame, _)| *frame == 1536),
        "on its first frame"
    );
    assert!(generation.is_idle(), "now the release has gone");
}

/// **The output node counts what it cannot pass**: two sources feed it 1,200
/// events in one block, past its room of `MIDI_OUT_EVENT_CAPACITY`; the ones it
/// cannot pass are counted as lost.
#[test]
fn the_midi_out_node_counts_the_events_it_cannot_pass() {
    let (_ed, mut exec, (), seen) =
        rig_with(tutti_graph::ForkByClone(MidiOutNode::new()), 1, 512, |ed| {
            for source in 0..2u64 {
                let (inlet, mut writer) = EventInletNode::pair(EventCapacity::of(4096));
                for i in 0..600u64 {
                    writer
                        .push(Stamp::Frame(Frame::new(i % 512)), note_on_ump(i))
                        .expect("room");
                }
                let key = NodeKey(3 + source);
                ed.insert(key, "source", inlet);
                ed.spec_mut().connect_events(
                    EventInputPort::new(NodeKey(1), 0),
                    EventEdge::Direct(EventOutputPort::new(key, 0)),
                );
            }
        });
    block(&mut exec, 512);
    let passed = seen.lock().unwrap().len() as u64;
    let lost = exec.dropped_event_count();
    assert_eq!(
        passed + lost,
        1_200,
        "every event passed or counted: {passed} + {lost}"
    );
    assert!(lost >= 1_200 - tutti_midi_runtime::MIDI_OUT_EVENT_CAPACITY as u64);
}

/// **A following output node handles a generation two blocks after the block
/// that read it**: the first block reads the departure, and only the block after
/// that one has begun is the node known to have run, sources to taps, after the
/// departure; the generation is idle at the start of the second block after it.
#[test]
fn a_following_midi_out_node_handles_a_generation_two_blocks_after_reading_it() {
    let (mut writer, generation) = DeviceGeneration::new();
    writer.arrive();
    let (_ed, mut exec, (), _seen) = rig_with(
        tutti_graph::ForkByClone(MidiOutNode::new().following(&generation)),
        1,
        512,
        |_| {},
    );
    block(&mut exec, 512);
    block(&mut exec, 512);
    assert!(generation.is_idle(), "the arrival is handled");
    writer.depart();
    assert!(!generation.is_idle(), "the departure is owed blocks");
    block(&mut exec, 512);
    assert!(
        !generation.is_idle(),
        "the block that read it is not enough"
    );
    block(&mut exec, 512);
    assert!(!generation.is_idle(), "one block after it is not enough");
    block(&mut exec, 512);
    assert!(
        generation.is_idle(),
        "two blocks after the one that read it"
    );
}

/// `node` declaring a heavy cost, so the plan spreads its lanes over a pool.
struct Heavy<N>(N);

impl<N: tutti_graph::Node> tutti_graph::Node for Heavy<N> {
    fn shape(&self) -> tutti_graph::Shape {
        self.0.shape().cost(tutti_graph::Cost::Heavy)
    }
    fn prepare(&mut self, prepare: &tutti_graph::Prepare) {
        self.0.prepare(prepare);
    }
    fn process(
        &mut self,
        cx: &tutti_graph::Cx<'_>,
        io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        self.0.process(cx, io)
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

/// `lanes` retiring output nodes, each fed by an inlet of its own into one input
/// of the sink (lane `k` is sink input `k`), on a pool of four workers when
/// `pooled`: the inlets' writers and the generations' writers.
fn retiring_lanes(
    lanes: u16,
    pooled: bool,
) -> (
    EventRig,
    Vec<(
        tutti_graph::EventInletWriter,
        tutti_midi_runtime::GenerationWriter,
    )>,
    Vec<DeviceGeneration>,
) {
    let mut rig = EventRig::new(RATE, 512, lanes);
    if pooled {
        rig.exec.set_pool(Some(Arc::new(
            tutti_graph::WorkerPool::builder()
                .num_threads(4)
                .build()
                .expect("a worker pool"),
        )));
    }
    let mut writers = Vec::new();
    let mut generations = Vec::new();
    for lane in 0..lanes {
        let (mut generation_writer, generation) = DeviceGeneration::new();
        generation_writer.arrive();
        let (inlet, writer) = EventInletNode::pair(EventCapacity::of(1024));
        rig.editor
            .insert(NodeKey(10 + u64::from(lane)), "source", inlet);
        rig.editor.insert(
            NodeKey(100 + u64::from(lane)),
            "out",
            tutti_graph::Unforkable(Heavy(MidiOutNode::new().following(&generation))),
        );
        rig.editor.spec_mut().connect_events(
            EventInputPort::new(NodeKey(100 + u64::from(lane)), 0),
            EventEdge::Direct(EventOutputPort::new(NodeKey(10 + u64::from(lane)), 0)),
        );
        rig.feed(NodeKey(100 + u64::from(lane)), 0, lane);
        writers.push((writer, generation_writer));
        generations.push(generation);
    }
    rig.commit();
    (rig, writers, generations)
}

/// What sink input `lane` received: `(note, is_off)` of each note message.
fn notes_at(seen: &Seen, lane: u16) -> Vec<(u8, bool)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(port, ..)| *port == lane)
        .filter_map(|(_, _, ump)| {
            let note = note_of(*ump)?;
            Some((
                note,
                matches!(ump.note_key_event(), Some(NoteKeyEvent::Off(_))),
            ))
        })
        .collect()
}

/// A Note On of `note` on the first Group and Channel at `frame`.
fn on_at(writer: &mut tutti_graph::EventInletWriter, frame: u64, note: u8) {
    writer
        .push(
            Stamp::Frame(Frame::new(frame)),
            midi::note_on(MidiChannel::FIRST, note, 0x8000),
        )
        .expect("room");
}

/// **A retiring output node ends the notes it passed, in the last block that
/// passes events, and then passes nothing**: a note started before the
/// departure, one in the block that read it and one in the block after are each
/// ended by an explicit Note Off in that last block; a note sent after is
/// dropped and counted.
#[test]
fn a_retiring_out_node_ends_the_notes_it_passed_and_then_passes_nothing() {
    let (mut rig, mut lanes, _) = retiring_lanes(1, false);
    let (writer, departure) = &mut lanes[0];
    on_at(writer, 10, 60);
    rig.block(512);
    departure.depart();
    on_at(writer, 512 + 10, 61);
    rig.block(512);
    on_at(writer, 1024 + 10, 62);
    rig.block(512);
    let passed = notes_at(&rig.seen, 0);
    for note in [60, 61, 62] {
        assert!(
            passed.contains(&(note, false)),
            "{note} started: {passed:?}"
        );
        assert!(passed.contains(&(note, true)), "{note} ended: {passed:?}");
    }
    on_at(writer, 1536 + 10, 63);
    rig.block(512);
    assert_eq!(passed, notes_at(&rig.seen, 0), "nothing passes once closed");
    assert_eq!(rig.exec.dropped_event_count(), 1, "the note it dropped");
}

/// **The same on a worker pool, with a source feeding each node in the block**:
/// every lane's note sent in the block after the departure is ended, whichever
/// thread ran the lane, and no lane is idle early.
#[test]
#[cfg_attr(miri, ignore = "spawns a worker pool")]
fn a_retiring_out_node_ends_the_notes_it_passed_on_a_pool() {
    const LANES: u16 = 8;
    let (mut rig, mut lanes, _) = retiring_lanes(LANES, true);
    rig.block(512);
    assert!(
        rig.exec.plan().expect("a plan").schedule(4).wake > 1,
        "the lanes are spread over the pool"
    );
    for (_, departure) in &mut lanes {
        departure.depart();
    }
    rig.block(512);
    for (lane, (writer, _)) in lanes.iter_mut().enumerate() {
        on_at(writer, 1024 + 10, 40 + lane as u8);
    }
    rig.block(512);
    rig.block(512);
    for lane in 0..LANES {
        let note = 40 + lane as u8;
        let passed = notes_at(&rig.seen, lane);
        assert_eq!(passed, [(note, false), (note, true)], "lane {lane}");
    }
}

/// **A retiring out node whose port has no room for every off keeps ending notes
/// until none is left, and is idle only then**: 2 500 notes are held, more than
/// a block's room for Note Offs, so the offs take several blocks, every one
/// reaches the sink, and the generation is not handled before the last.
#[test]
fn a_retiring_out_node_ends_every_held_note_across_blocks_and_is_idle_after() {
    const HELD: u64 = 2_500;
    let (mut rig, mut lanes, generations) = retiring_lanes(1, false);
    let (writer, departure) = &mut lanes[0];
    for block in 0..3u64 {
        for i in (block * 900)..((block + 1) * 900).min(HELD) {
            let address = Address::new(
                MidiGroup::all().nth((i / 2048) as usize).unwrap(),
                MidiChannel::of(((i / 128) % 16) as u8),
            );
            writer
                .push(
                    Stamp::Frame(Frame::new(block * 512 + i % 512)),
                    midi::note_on(address, (i % 128) as u8, 0x8000),
                )
                .expect("room");
        }
        rig.block(512);
    }
    departure.depart();
    // The block that reads it, and the one that ends what fits.
    rig.block(512);
    rig.block(512);
    let ended = |rig: &EventRig| {
        rig.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, ump)| matches!(ump.note_key_event(), Some(NoteKeyEvent::Off(_))))
            .count()
    };
    let first = ended(&rig);
    assert!(0 < first && first < HELD as usize, "some offs fit: {first}");
    rig.block(512);
    assert!(!generations[0].is_idle(), "offs are still owed");
    for _ in 0..4 {
        rig.block(512);
    }
    assert_eq!(ended(&rig), HELD as usize, "every note was ended");
    // A refused off is deferred, so each refusal is given back and nothing is
    // lost: the counts match, not merely lost = dropped - recovered saturating at 0.
    assert_eq!(
        rig.exec.dropped_event_count(),
        rig.exec.recovered_event_count(),
        "every refusal was given back, and no more"
    );
    assert_eq!(rig.exec.lost_event_count(), 0);
    rig.block(512);
    assert!(generations[0].is_idle(), "nothing is owed");
}

/// **A MIDI 1.0 Note On at velocity 0 ends the note the node tracks**: no off is
/// owed for it when the node retires.
#[test]
fn a_velocity_zero_note_on_is_not_held_by_a_retiring_out_node() {
    let (mut rig, mut lanes, _) = retiring_lanes(1, false);
    let (writer, departure) = &mut lanes[0];
    let on = |note: u8, velocity: u8| -> Ump {
        Message::midi1(
            Address::first(MidiChannel::FIRST),
            tutti_ump::Voice1::note_on(NoteNumber::of(note), U7::new(velocity).unwrap()),
        )
        .into()
    };
    for (at, ump) in [(10, on(70, 100)), (11, on(70, 0)), (12, on(71, 100))] {
        writer
            .push(Stamp::Frame(Frame::new(at)), ump)
            .expect("room");
    }
    rig.block(512);
    departure.depart();
    for _ in 0..4 {
        rig.block(512);
    }
    let offs: Vec<u8> = notes_at(&rig.seen, 0)
        .into_iter()
        .filter(|(_, off)| *off)
        .map(|(note, _)| note)
        .collect();
    // The one off of 70 is the velocity-0 message itself, passed on; the node adds none.
    assert_eq!(offs.iter().filter(|n| **n == 70).count(), 1, "{offs:?}");
    assert_eq!(offs.iter().filter(|n| **n == 71).count(), 1, "{offs:?}");
}

fn note_on_ump(i: u64) -> Ump {
    midi::note_on(MidiChannel::of((i % 16) as u8), (i % 100) as u8, 0x8000)
}

/// **An input a new device takes starts empty**: device 0 selects an RPN in
/// block 0, a new device then arrives at the slot (the generation move on, which
/// the node follows with no call from the owner), and the Data Entry in block 1
/// finds no select to complete, so no Registered Controller is made; with no
/// new device the same Data Entry completes the run.
#[test]
fn a_slot_a_new_device_takes_forgets_the_previous_devices_half_sent_run() {
    let script = || {
        Script::new(vec![
            vec![
                cc_ev(3, CCNumber::RPN_MSB, 0),
                cc_ev(3, CCNumber::RPN_LSB, 6),
            ],
            vec![
                cc_ev(3, CCNumber::DATA_ENTRY, 10),
                cc_ev(3, CCNumber::DATA_ENTRY_LSB, 0),
            ],
        ])
    };
    for reopened in [false, true] {
        let (mut writer, generation) = DeviceGeneration::new();
        let Input { node, inlet } = Input::new(script());
        let node = node.following(&generation);
        let (_ed, mut exec, _controls, seen) = input(Input { node, inlet }, FIRST.count(), 512);
        block(&mut exec, 512);
        if reopened {
            writer.arrive();
        }
        block(&mut exec, 512);
        let made = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| is_registered(s.2))
            .count();
        assert_eq!(made, usize::from(!reopened), "reopened: {reopened}");
    }
}

/// An input node that follows `generation`, over the script's device.
fn following(script: Script, generation: &DeviceGeneration) -> Input {
    let Input { node, inlet } = Input::new(script);
    Input {
        node: node.following(generation),
        inlet,
    }
}

/// A System message of `group`.
fn system(group: MidiGroup, message: tutti_ump::System) -> UmpAt {
    at(0, Ump::from(Message::System(group, message)))
}

/// A short description of what a sink heard, for an assertion that reads:
/// `(frame, what)`, `what` being `on N`, `off N`, `cc C V` or `bend V` of a
/// MIDI 2.0 channel voice message, `other` for the rest.
fn heard(seen: &Seen) -> Vec<(u64, String)> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|&(_, frame, ump)| {
            let what = match (ump.note_key_event(), ump.decode()) {
                (Some(NoteKeyEvent::On(k)), _) => format!("on {}", k.note().get()),
                (Some(NoteKeyEvent::Off(k)), _) => format!("off {}", k.note().get()),
                (_, Ok(Message::Midi2(_, Voice2::ControlChange { controller, value }))) => {
                    format!("cc {} {:#x}", controller.get(), value.get())
                }
                (_, Ok(Message::Midi2(_, Voice2::PitchBend(v)))) => format!("bend {:#x}", v.get()),
                _ => "other".to_string(),
            };
            (frame, what)
        })
        .collect()
}

/// **A device that lets go leaves nothing held downstream** (MIDI 1.0
/// Detailed Specification: p. 24, all notes are turned off by individual Note
/// Offs; A-5, a pedal down defeats them; p. 31 and RP-015, the pedals and the
/// pitch wheel return to normal). Device 0 holds a key, the sustain pedal and
/// a bend off centre, then drops its input: the next block's first frame
/// carries the Note Off, the pedal-up and the centred bend, in that order,
/// and its later events are not heard. When another device takes the slot,
/// the same release comes first and then the new device is heard.
#[test]
fn a_device_that_lets_go_has_what_it_held_released() {
    for replaced in [false, true] {
        let (mut writer, generation) = DeviceGeneration::new();
        writer.arrive();
        let bend = at(
            30,
            midi::pitch_bend(Address::new(G, MidiChannel::of(0)), 0xC000_0000),
        );
        let wire = Script::new(vec![
            vec![note(0, 60, 10), cc_at(20, 0, CCNumber::SUSTAIN, 127), bend],
            vec![note(0, 61, 5)],
        ]);
        let (_ed, mut exec, _c, seen) = input(following(wire, &generation), FIRST.count(), 512);
        block(&mut exec, 512);
        assert_eq!(heard(&seen).len(), 3, "the key, the pedal, the bend");
        seen.lock().unwrap().clear();
        writer.depart();
        if replaced {
            writer.arrive();
        }
        block(&mut exec, 512);
        let mut expected = vec![
            (512, "off 60".to_string()),
            (512, "cc 64 0x0".to_string()),
            (512, "bend 0x80000000".to_string()),
        ];
        if replaced {
            expected.push((517, "on 61".to_string()));
        }
        assert_eq!(heard(&seen), expected, "replaced: {replaced}");
    }
}

/// **Active Sensing: once an FE is seen, a message is expected within 330 ms
/// and the cable's notes end when none comes** (MIDI 1.0 Detailed
/// Specification p. 32). At 48 kHz the limit is 15,840 frames: an FE and a key
/// at frame 0, silence after, and the Note Off is at frame 15,840 exactly (the
/// frame the time ran out at, inside the block), not a block earlier; the FE
/// itself is not sent on. A message at frame 15,000 moves the lapse a limit on
/// from it.
#[test]
fn a_cable_that_goes_quiet_after_active_sensing_has_its_notes_ended() {
    let sense = system(G, tutti_ump::System::ActiveSensing);
    let blocks = |last: u64| (last / 512 + 2) as usize;
    for (extra, ran_out) in [(None, 15_840u64), (Some(15_000usize), 30_840)] {
        let mut batch = vec![sense, note(0, 60, 0)];
        batch.extend(extra.map(|f| cc_at(f, 0, CCNumber::of(1), 1)));
        let (_ed, mut exec, _c, seen) =
            input(Input::new(Script::new(vec![batch])), FIRST.count(), 512);
        for _ in 0..blocks(ran_out) {
            block(&mut exec, 512);
        }
        let got = heard(&seen);
        let off = got.iter().find(|(_, w)| w == "off 60");
        assert_eq!(off.map(|o| o.0), Some(ran_out), "{got:?}");
        assert_eq!(
            got.iter().filter(|(_, w)| w == "other").count(),
            0,
            "the FE is not sent on: {got:?}"
        );
    }
}

/// **A device that never sends an Active Sensing is never judged** (p. 32:
/// a device that never receives Active Sensing operates normally):
/// the same key and silence, no FE, and the key is still held after 40
/// blocks.
#[test]
fn a_cable_with_no_active_sensing_is_never_judged_gone() {
    let (_ed, mut exec, _c, seen) = input(
        Input::new(Script::new(vec![vec![note(0, 60, 0)]])),
        FIRST.count(),
        512,
    );
    for _ in 0..40 {
        block(&mut exec, 512);
    }
    assert_eq!(heard(&seen), vec![(0, "on 60".to_string())]);
}

/// **System Reset ends what the cable held, forgets its half-sent runs and is
/// not echoed** (p. 33: turn voices off, reset all controllers, clear running
/// status; the message is never echoed). A key, the
/// sustain pedal and an RPN select on Group 1 and a key on Group 2, then an FF
/// on Group 1 at frame 100: Group 1's key ends and its pedal lifts at frame
/// 100, the FF is sent on to no one, the Data Entry after it completes no
/// RPN, and Group 2's key is not touched.
#[test]
fn a_system_reset_ends_its_cables_notes_and_runs_and_is_not_echoed() {
    let g2 = MidiGroup::of(1);
    let two = MidiInputPorts::through(g2);
    let reset = at(100, Ump::from(Message::System(G, tutti_ump::System::Reset)));
    let other_note = at(
        15,
        midi::note_on(
            Address::new(g2, MidiChannel::of(0)),
            70,
            NoteVelocity::from(U7::of(100)).get(),
        ),
    );
    let wire = Script::new(vec![vec![
        note(0, 60, 10),
        other_note,
        cc_at(20, 0, CCNumber::SUSTAIN, 127),
        cc_at(30, 0, CCNumber::RPN_MSB, 0),
        cc_at(31, 0, CCNumber::RPN_LSB, 6),
        reset,
        cc_at(200, 0, CCNumber::DATA_ENTRY, 10),
        cc_at(201, 0, CCNumber::DATA_ENTRY_LSB, 0),
    ]]);
    let (_ed, mut exec, _c, seen) = input(Input::new(wire).hearing(two), two.count(), 512);
    block(&mut exec, 512);
    let got = heard(&seen);
    let after: Vec<_> = got.iter().filter(|(f, _)| *f >= 100).cloned().collect();
    assert_eq!(
        after,
        vec![(100, "off 60".to_string()), (100, "cc 64 0x0".to_string())],
        "{got:?}"
    );
    assert!(
        got.contains(&(15, "on 70".to_string())),
        "Group 2 untouched"
    );
    assert!(
        !seen.lock().unwrap().iter().any(|s| is_registered(s.2)),
        "the select did not survive the reset"
    );
}

/// **A message with no Channel field applies to every Channel of its Group**
/// (M2-104 section 2.1.2): a System Reset names a Group and no Channel, and the
/// keys held on Channels 0, 5 and 15 of that Group all end at its frame, while
/// the key held on Channel 5 of the other Group is not touched.
/// Spec: m2-104/2.1.2#4
#[test]
fn a_system_reset_reaches_every_channel_of_its_group() {
    let g2 = MidiGroup::of(1);
    let two = MidiInputPorts::through(g2);
    let reset = at(100, Ump::from(Message::System(G, tutti_ump::System::Reset)));
    let other_group = at(
        15,
        midi::note_on(
            Address::new(g2, MidiChannel::of(5)),
            70,
            NoteVelocity::from(U7::of(100)).get(),
        ),
    );
    let wire = Script::new(vec![vec![
        note(0, 60, 10),
        note(5, 61, 11),
        note(15, 62, 12),
        other_group,
        reset,
    ]]);
    let (_ed, mut exec, _c, seen) = input(Input::new(wire).hearing(two), two.count(), 512);
    block(&mut exec, 512);
    let got = heard(&seen);
    let mut after: Vec<_> = got.iter().filter(|(f, _)| *f >= 100).cloned().collect();
    after.sort();
    assert_eq!(
        after,
        vec![
            (100, "off 60".to_string()),
            (100, "off 61".to_string()),
            (100, "off 62".to_string()),
        ],
        "{got:?}"
    );
}

/// **A requested MPE mode is adopted once and holds.** With the lower zone
/// requested, a note on member channel 1 binds it; a channel bend there in
/// each of the next two blocks folds into a per-note bend on the note. The
/// request latches, so it is read every block. Adopting it sends the zone's
/// default bend ranges once, in the first block (thirteen registered
/// controllers: the master's range, and on each of six members the channel's
/// and the per-note one), after the All Sound Off and Reset All
/// Controllers of the seven channels that entered MPE control (M1-100
/// section 2.2.3).
#[test]
fn a_requested_mpe_mode_is_adopted_once() {
    let member = MidiChannel::of(1);
    let bend = at(0, midi::pitch_bend(member, 0xC000_0000));
    let wire = Script::new(vec![
        vec![at(
            0,
            midi::note_on(member, 60, NoteVelocity::from(U7::of(100)).get()),
        )],
        vec![bend],
        vec![bend],
    ]);
    let cell = Publisher::new(MpeMode::Disabled);
    let (_ed, mut exec, _controls, seen) = input(Input::new(wire).mpe(&cell), FIRST.count(), 512);
    cell.publish(midi::lower_zone(6));
    for _ in 0..3 {
        block(&mut exec, 512);
    }
    let seen = seen.lock().unwrap();
    // MIDI 2.0 channel voice (type 4), Registered Controller (status 0x2).
    let is_rpn = |ev: &Ump| {
        matches!(
            (*ev).decode(),
            Ok(Message::Midi2(_, Voice2::Registered { .. }))
        )
    };
    let is_stop = |ev: &Ump| {
        matches!(
            (*ev).decode(),
            Ok(Message::Midi2(_, Voice2::ControlChange { controller, .. }))
                if controller == CCNumber::ALL_SOUND_OFF || controller == CCNumber::RESET_ALL
        )
    };
    let resets: Vec<u64> = seen.iter().filter(|s| is_rpn(&s.2)).map(|s| s.1).collect();
    assert_eq!(
        resets,
        vec![0; 13],
        "the ranges, once, at frame 0: {seen:?}"
    );
    let stops: Vec<u64> = seen.iter().filter(|s| is_stop(&s.2)).map(|s| s.1).collect();
    assert_eq!(stops, vec![0; 14], "the stops, once, at frame 0: {seen:?}");
    let music: Vec<_> = seen
        .iter()
        .filter(|s| !is_rpn(&s.2) && !is_stop(&s.2))
        .collect();
    let folded: Vec<Option<u8>> = music[1..].iter().map(|s| note_of(s.2)).collect();
    assert_eq!(folded, vec![Some(60), Some(60)], "{seen:?}");
}

/// **A device's MPE Profile zones switch its rules** (M2-120 section 3.3,
/// Table 15). The device sends Reset All Controllers on member channel 2 in
/// three blocks. The Profile is enabled before block 1 and disabled before
/// block 2: the Profile makes Reset All Controllers on a Member Channel
/// prohibited, so the device's is ignored in block 1 alone and heard in the
/// others. Enabling sends the zone's channels All Sound Off and Reset All
/// Controllers once, ahead of the block's events.
#[test]
fn a_devices_profile_zones_switch_its_rules() {
    let reset = |frame: usize| cc_at(frame, 2, CCNumber::RESET_ALL, 0);
    let (mut writer, generation) = DeviceGeneration::new();
    writer.arrive();
    let node = following(
        Script::new(vec![vec![reset(0)], vec![reset(1)], vec![reset(2)]]),
        &generation,
    );
    let (_ed, mut exec, controls, seen) = input(node, FIRST.count(), 512);
    let zone = MpeZones::single(MpeZoneConfig::profile(
        MpeZoneSpan::new(MidiChannel::of(0), MpeChannels::of(4)).unwrap(),
        PitchBendSensitivity::MPE_PROFILE_DEFAULT,
    ));
    let group = MidiGroup::FIRST;
    block(&mut exec, 512);
    controls.set_mpe_profile(group, Some(zone));
    block(&mut exec, 512);
    controls.set_mpe_profile(group, None);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let is_reset = |ev: &Ump| {
        matches!(
            (*ev).decode(),
            Ok(Message::Midi2(_, Voice2::ControlChange { controller, .. }))
                if controller == CCNumber::RESET_ALL
        )
    };
    // Block 1's frames are 512 to 1023: the Reset All Controllers of the
    // zone's four channels at its first frame, and the device's own (frame 513)
    // ignored.
    let block_one: Vec<(u16, u64)> = seen
        .iter()
        .filter(|s| (512..1024).contains(&s.1) && is_reset(&s.2))
        .map(|s| (s.0, s.1))
        .collect();
    assert_eq!(
        block_one,
        [(0, 512), (1, 512), (2, 512), (3, 512)],
        "{seen:?}"
    );
    let heard = |first: u64| {
        seen.iter()
            .filter(|s| (first..first + 512).contains(&s.1) && s.0 == 2 && is_reset(&s.2))
            .count()
    };
    assert_eq!(heard(0), 1, "the device's own, before the Profile");
    // The device's own, and the Reset All Controllers that leaving the Profile
    // sends to the channel.
    assert_eq!(heard(1024), 2);
}

/// **A Profile zone is started once, by the publish that enabled it**: the
/// blocks after it, with no further request, send no more of its resets.
#[test]
fn a_profile_zone_starts_once_per_request() {
    let (mut writer, generation) = DeviceGeneration::new();
    writer.arrive();
    let (_ed, mut exec, controls, seen) = input(
        following(
            Script::new(vec![vec![], vec![], vec![], vec![]]),
            &generation,
        ),
        FIRST.count(),
        512,
    );
    let zone = MpeZones::single(MpeZoneConfig::profile(
        MpeZoneSpan::new(MidiChannel::of(0), MpeChannels::of(4)).unwrap(),
        PitchBendSensitivity::MPE_PROFILE_DEFAULT,
    ));
    // The first block takes the device's slot over; the zones come after it.
    block(&mut exec, 512);
    controls.set_mpe_profile(MidiGroup::FIRST, Some(zone));
    for _ in 0..3 {
        block(&mut exec, 512);
    }
    let resets = |range: std::ops::Range<u64>| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|s| {
                range.contains(&s.1)
                    && matches!(
                        s.2.decode(),
                        Ok(Message::Midi2(_, Voice2::ControlChange { controller, .. }))
                            if controller == CCNumber::RESET_ALL
                    )
            })
            .count()
    };
    assert_eq!(resets(512..1024), 4, "read on the block after the publish");
    assert_eq!(resets(1024..2048), 0, "not again without a publish");
}

/// Clearing the device's zones sets each of its Groups to none: the device's
/// Reset All Controllers on a Member Channel, ignored under the Profile, is
/// heard again.
#[test]
fn clearing_a_devices_profiles_returns_it_to_the_rules_it_had() {
    let (mut writer, generation) = DeviceGeneration::new();
    writer.arrive();
    let reset = cc_at(1, 2, CCNumber::RESET_ALL, 0);
    let (_ed, mut exec, controls, seen) = input(
        following(
            Script::new(vec![vec![], vec![reset], vec![reset]]),
            &generation,
        ),
        FIRST.count(),
        512,
    );
    let zone = MpeZones::single(MpeZoneConfig::profile(
        MpeZoneSpan::new(MidiChannel::of(0), MpeChannels::of(4)).unwrap(),
        PitchBendSensitivity::MPE_PROFILE_DEFAULT,
    ));
    // The first block takes the device's slot over; the zones come after it.
    block(&mut exec, 512);
    controls.set_mpe_profile(MidiGroup::FIRST, Some(zone));
    block(&mut exec, 512);
    controls.clear_mpe_profiles();
    block(&mut exec, 512);
    let heard_at = |frame: u64| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|s| {
                s.1 == frame
                    && matches!(
                        s.2.decode(),
                        Ok(Message::Midi2(_, Voice2::ControlChange { controller, .. }))
                            if controller == CCNumber::RESET_ALL
                    )
            })
            .count()
    };
    assert_eq!(heard_at(513), 0, "ignored while the Profile is enabled");
    assert_eq!(heard_at(1025), 1, "heard once the zones are cleared");
}

/// **A host-set mode's reset goes ahead of the block's own events.** The
/// lower zone of six members is requested, and the adopting block polls a
/// bend range (RPN #00/00, 12 semitones) on member 2 at frame 0. Both the
/// reset (±48 per member) and the wire's range (±12 per member) land on
/// frame 0; a receiver reading each member's port in order must end at the
/// wire's ±12, which came after the mode it was sent in.
#[test]
fn a_host_set_modes_reset_goes_ahead_of_the_blocks_events() {
    use tutti_fixtures::midi::BendRanges;
    use tutti_ump::{RegisteredData, Rpn, U7};
    let range = at(
        0,
        midi::registered(
            MidiChannel::of(2),
            Rpn::PITCH_BEND_SENSITIVITY,
            RegisteredData::new(U7::of(12), U7::of(0)).to_word(),
        ),
    );
    let wire = Script::new(vec![vec![range]]);
    let cell = Publisher::new(MpeMode::Disabled);
    let (_ed, mut exec, _controls, seen) = input(Input::new(wire).mpe(&cell), FIRST.count(), 512);
    cell.publish(midi::lower_zone(6));
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let mut ranges = BendRanges::default();
    let mut per_member = 0;
    for (_, frame, ev) in seen.iter() {
        if ranges.observe(ev) {
            assert_eq!(*frame, 0);
            per_member += 1;
        }
    }
    assert_eq!(
        per_member, 12,
        "the reset's six and the wire's six: {seen:?}"
    );
    for channel in 1..=6 {
        assert_eq!(
            ranges.semitones(tutti_ump::MidiChannel::of(channel)),
            tutti_units::Semitones::new(12.0),
            "channel {channel}"
        );
    }
}

/// **A member channel's bend range reaches every member's port as a per-note
/// range, without losing the events polled after it.** With a lower zone of
/// seven members, a MIDI 1.0 RPN 0 run (12 semitones) on member 2 and then a
/// note on member 1: ports 1 to 7 each receive RPN #00/07 at 12 semitones,
/// and port 1 the note too.
#[test]
fn a_member_bend_range_fans_out_over_the_zones_ports() {
    use tutti_fixtures::midi::BendRanges;
    let wire = Script::new(vec![vec![
        cc_ev(2, CCNumber::RPN_MSB, 0),
        cc_ev(2, CCNumber::RPN_LSB, 0),
        cc_ev(2, CCNumber::DATA_ENTRY, 12),
        cc_ev(2, CCNumber::DATA_ENTRY_LSB, 0),
        note(1, 60, 5),
    ]]);
    let node = Input::new(wire).mpe(&Publisher::new(midi::lower_zone(7)));
    let (_ed, mut exec, _c, seen) = input(node, FIRST.count(), 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let mut ranges = BendRanges::default();
    let mut range_ports = Vec::new();
    for (port, _, ev) in seen.iter() {
        if ranges.observe(ev) {
            assert_eq!(ev.channel().map(u8::from), Some(*port as u8));
            range_ports.push(*port);
        }
    }
    assert_eq!(range_ports, (1..=7).collect::<Vec<_>>(), "{seen:?}");
    for channel in 1..=7 {
        assert_eq!(
            ranges.semitones(tutti_ump::MidiChannel::of(channel)),
            tutti_units::Semitones::new(12.0),
            "channel {channel}"
        );
    }
    let notes: Vec<(u16, u64)> = seen
        .iter()
        .filter(|s| matches!(s.2.note_key_event(), Some(NoteKeyEvent::On(_))))
        .map(|s| (s.0, s.1))
        .collect();
    assert_eq!(notes, vec![(1, 5)]);
}

/// **A translation with no room is dropped whole, and counted.** With a
/// lower zone of seven members, a block polls 507 notes on the master, a
/// MIDI 1.0 RPN 0 run (12 semitones, sent at its CC 38) on member 2, and a
/// note on member 1: 512 polled events. The run fans out to seven, which do
/// not fit after the 507 notes (512 slots), so all seven are dropped and
/// counted, and the note after them still goes out: 508 events, no per-note
/// range.
#[test]
fn a_fan_out_with_no_room_is_dropped_whole_and_counted() {
    use tutti_fixtures::midi::BendRanges;
    // The run is sent after the notes: an inlet delivers in time order.
    let mut batch: Vec<UmpAt> = (0..507).map(|i| note(0, 60, i)).collect();
    batch.extend([
        cc_at(508, 2, CCNumber::RPN_MSB, 0),
        cc_at(508, 2, CCNumber::RPN_LSB, 0),
        cc_at(508, 2, CCNumber::DATA_ENTRY, 12),
        cc_at(508, 2, CCNumber::DATA_ENTRY_LSB, 0),
        note(1, 61, 509),
    ]);
    assert_eq!(batch.len(), MIDI_INPUT_EVENT_CAPACITY);
    let node = Input::new(Script::new(vec![batch])).mpe(&Publisher::new(midi::lower_zone(7)));
    let (_ed, mut exec, controls, seen) = input(node, FIRST.count(), 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let mut ranges = BendRanges::default();
    assert!(
        !seen.iter().any(|s| ranges.observe(&s.2)),
        "no range went out"
    );
    assert_eq!(seen.len(), 508);
    assert!(seen.iter().any(|s| s.0 == 1 && note_of(s.2) == Some(61)));
    assert_eq!(controls.losses().refused, 7);
}

/// **A note-off has room when a block is full of other messages.** 504
/// notes on the master, a bend-range run on member 2 (sent at its CC 38)
/// that fans out to seven (504 + 7 leave one of the 512 the other messages
/// may use) and four note-offs on
/// member 1: all four go out, in the room past the capacity, and nothing is
/// dropped or counted. A note-on in their place would be dropped.
#[test]
fn a_note_off_has_room_past_the_capacity() {
    let mut batch: Vec<UmpAt> = (0..504).map(|i| note(0, 60, i)).collect();
    batch.extend([
        cc_ev(2, CCNumber::RPN_MSB, 0),
        cc_ev(2, CCNumber::RPN_LSB, 0),
        cc_ev(2, CCNumber::DATA_ENTRY, 12),
        cc_ev(2, CCNumber::DATA_ENTRY_LSB, 0),
    ]);
    batch.extend((0..4).map(|k| at(508 + k, midi::note_off(MidiChannel::of(1), 61 + k as u8, 0))));
    assert_eq!(batch.len(), MIDI_INPUT_EVENT_CAPACITY);
    let node = Input::new(Script::new(vec![batch])).mpe(&Publisher::new(midi::lower_zone(7)));
    let (_ed, mut exec, controls, seen) = input(node, FIRST.count(), 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.iter().filter(|s| s.2.is_note_off()).count(), 4);
    // The run went out too: the block is full of other messages.
    assert_eq!(seen.len(), 504 + 7 + 4);
    assert_eq!(controls.losses().refused, 0);
}

/// **An input node's fork reads no wire.**
#[test]
fn an_input_nodes_fork_is_silent() {
    let wire = Script::new(vec![vec![note(0, 60, 0)]]);
    let (live, _exec, _c, seen) = input(Input::new(wire), FIRST.count(), 512);
    let prepare = *live.prepare();
    let (_fork, mut fork_exec) = live.fork(ForkTarget::MainOutput, prepare).expect("forks");
    block(&mut fork_exec, 512);
    assert!(seen.lock().unwrap().is_empty());
}

/// **An event tap hands MIDI back on the graph's clock, and counts what a
/// full ring drops.** Five blocks of 256 queued notes into a ring of 1 024
/// nobody drains: 256 dropped; what is drained is the first 1 024, the
/// first on its frame (block 0, offset 7) and the last of them on block 3's
/// (frame 1 536 plus offset 8).
#[test]
fn an_event_tap_hands_midi_back_and_counts_drops() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    let (inlet, mut keys) = EventInletNode::pair(EventCapacity::of(256));
    ed.insert(NodeKey(1), "keys", inlet);
    let mut out = common::sink(&mut ed, 1, &[(1, 0)]);
    ed.commit().unwrap();
    for k in 0..5u64 {
        // The inlet holds each event for its frame: offset 7, then 8, in
        // block `k`.
        for i in 0..256u64 {
            let frame = Frame::new(512 * k + 7 + i / 128);
            keys.push(Stamp::Frame(frame), note(0, 60, 0).1).unwrap();
        }
        exec.process(512, &TransportPosition::default(), &[], &mut []);
    }
    assert_eq!(out.dropped(), 256);
    let mut buf = vec![(0, Ump([0; 4])); 2048];
    assert_eq!(out.poll_into(&mut buf), 1024);
    assert_eq!(buf[0].0, 7);
    assert_eq!(buf[1023].0, 3 * 512 + 8);
}

/// **A start inside a block clocks from its frame.** Stopped at beat 0 for
/// 100 frames, then rolling at 120 BPM: Start and the first tick at frame
/// 100, the next tick 1 000 frames on (a 24-PPQN tick at 48 kHz, the rate
/// `prepare` gave the master).
#[test]
fn a_start_inside_a_block_clocks_from_its_frame() {
    let (_ed, mut exec, _clock, seen) = rig(
        sync(SyncMessages {
            clock: true,
            mtc: false,
            ..SyncMessages::OFF
        }),
        1,
        2048,
    );
    let stopped = TransportPosition::new(false, Bpm::of(120.0), Beat::new(0.0), None);
    let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
    let mut changes = TransportChanges::NONE;
    changes
        .push(Offset::of(Samples::new(100), Samples::new(2048)), rolling)
        .unwrap();
    let mut out = vec![0.0f32; 2048];
    exec.process_with_changes(2048, &stopped, &changes, &[], &mut [&mut out[..]]);
    let got: Vec<(u64, Ump)> = seen.lock().unwrap().iter().map(|s| (s.1, s.2)).collect();
    assert_eq!(
        got,
        vec![
            (100, Ump::from(Message::System(G, System::Start))),
            (100, Ump::from(Message::System(G, System::TimingClock))),
            (1_100, Ump::from(Message::System(G, System::TimingClock)))
        ]
    );
}

/// What a [`MachineControlFollower`] makes of `seen`, each event on its frame.
fn followed(seen: &Seen) -> Vec<(u64, MachineControlEvent)> {
    let mut follower = MachineControlFollower::new(tutti_midi_protocol::SysexDeviceId::of(1));
    let mut got = Vec::new();
    for (frame, ump) in tutti_fixtures::heard(seen) {
        follower.step(ump, &mut |event| got.push((frame, event)));
    }
    got
}

/// **A Machine Control message is on the frame its transport change happens
/// (RP-013 pp.20 to 28)**: a start on frame 100 of a block is PLAY at offset
/// 100, a stop on frame 300 of the next is STOP at 300, and a jump on 700 of
/// a third is LOCATE with DEFERRED PLAY at 700 (one message, whose packets
/// follow each other). Heard through the follower, every message is whole.
#[test]
fn a_machine_control_message_is_on_the_frame_of_its_transport_change() {
    let (_ed, mut exec, _controls, seen) = rig(
        sync(SyncMessages {
            mmc: Some(Recipient::AllCall),
            rate: MtcRate::Fps30,
            ..SyncMessages::OFF
        }),
        1,
        2048,
    );
    let at_beat = |frames: f64| Beat::new(frames / 24_000.0);
    let stopped = |beat: Beat| TransportPosition::new(false, Bpm::of(120.0), beat, None);
    let rolling = |beat: Beat| TransportPosition::new(true, Bpm::of(120.0), beat, None);
    let mut out = vec![0.0f32; 2048];
    let mut run = |exec: &mut Executor,
                   start: &TransportPosition,
                   change: Option<(usize, TransportPosition)>| {
        let mut changes = TransportChanges::NONE;
        if let Some((frame, to)) = change {
            changes
                .push(Offset::of(Samples::new(frame), Samples::new(2048)), to)
                .unwrap();
        }
        seen.lock().unwrap().clear();
        exec.process_with_changes(2048, start, &changes, &[], &mut [&mut out[..]]);
    };
    // The first block only looks.
    run(&mut exec, &stopped(Beat::new(0.0)), None);
    assert!(followed(&seen).is_empty());
    // Start on frame 100 (at beat 0); the rest of the block rolls 1 948 frames.
    run(
        &mut exec,
        &stopped(Beat::new(0.0)),
        Some((100, rolling(Beat::new(0.0)))),
    );
    let block_start = |n: u64| n * 2048;
    assert_eq!(
        followed(&seen),
        [(block_start(1) + 100, MachineControlEvent::Play)]
    );
    // The next block continues from there and stops on frame 300.
    let resumed = at_beat(1948.0);
    run(
        &mut exec,
        &rolling(resumed),
        Some((300, stopped(at_beat(1948.0 + 300.0)))),
    );
    assert_eq!(
        followed(&seen),
        [(block_start(2) + 300, MachineControlEvent::Stop)]
    );
    // Stopped, it jumps to beat 2 (one second) on frame 700 and rolls from there.
    run(
        &mut exec,
        &stopped(at_beat(1948.0 + 300.0)),
        Some((700, rolling(Beat::new(2.0)))),
    );
    let got = followed(&seen);
    assert_eq!(got.len(), 2, "a locate and a deferred play: {got:?}");
    for (frame, _) in &got {
        assert_eq!(*frame, block_start(3) + 700);
    }
    assert_eq!(
        got[0].1,
        MachineControlEvent::Locate(tutti_units::Seconds::new(1.0))
    );
    assert_eq!(got[1].1, MachineControlEvent::Play);
}

/// **A transport no time code names is counted, and sends nothing.**
#[test]
fn a_machine_control_locate_to_no_time_code_is_counted() {
    let (_ed, mut exec, controls, seen) = rig(
        sync(SyncMessages {
            mmc: Some(Recipient::AllCall),
            rate: MtcRate::Fps30,
            ..SyncMessages::OFF
        }),
        1,
        512,
    );
    let mut out = vec![0.0f32; 512];
    for beat in [0.0, -4.0] {
        exec.process(
            512,
            &TransportPosition::new(false, Bpm::of(120.0), Beat::new(beat), None),
            &[],
            &mut [&mut out[..]],
        );
    }
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(controls.losses().refused, 1);
}

/// **A sync node's fork runs**: a bounce's events are data. The fork holds
/// what the live node sent at the fork (clock and Machine Control here) and
/// sends it for the transport it is given: Start and a tick at beat 0, and PLAY
/// on the second look.
#[test]
fn a_sync_nodes_fork_runs() {
    let (live, _exec, _controls, seen) = rig(
        sync(SyncMessages {
            clock: true,
            mmc: Some(Recipient::AllCall),
            rate: MtcRate::Fps30,
            ..SyncMessages::OFF
        }),
        1,
        512,
    );
    let prepare = *live.prepare();
    let (_fork, mut fork_exec) = live.fork(ForkTarget::MainOutput, prepare).expect("forks");
    let mut out = vec![0.0f32; 512];
    for playing in [false, true] {
        fork_exec.process(
            512,
            &TransportPosition::new(playing, Bpm::of(120.0), Beat::new(0.0), None),
            &[],
            &mut [&mut out[..]],
        );
    }
    let got: Vec<Ump> = seen.lock().unwrap().iter().map(|s| s.2).collect();
    assert!(got.contains(&Ump::from(Message::System(G, System::Start))));
    assert!(
        followed(&seen).contains(&(512, MachineControlEvent::Play)),
        "{:?}",
        followed(&seen)
    );
}

/// **A block with more clock events than the port holds sends the first
/// [`SYNC_OUT_EVENT_CAPACITY`](tutti_midi_runtime::SYNC_OUT_EVENT_CAPACITY) and
/// counts the rest.**
///
/// 300 BPM with MTC on at 48 kHz is 120 ticks and 100 quarter-frames a
/// second, so one 65 536-frame block (1.37 s) produces more events than the
/// port's 256. The same span in four quarter-length blocks fits every event,
/// so it is the reference total: what one long block sent plus what it
/// counted must equal it, exactly.
#[test]
fn a_clock_flood_past_the_port_capacity_is_counted() {
    use tutti_midi_runtime::SYNC_OUT_EVENT_CAPACITY;
    const LONG: usize = 65_536;
    const BPM: f64 = 300.0;

    let run = |blocks: &[usize]| -> (usize, u64) {
        let (_ed, mut exec, clock, seen) = rig(
            sync(SyncMessages {
                clock: true,
                mtc: true,
                ..SyncMessages::OFF
            }),
            1,
            LONG,
        );
        let mut frame = 0usize;
        for &len in blocks {
            let beat = frame as f64 / RATE * BPM / 60.0;
            let rolling = TransportPosition::new(true, Bpm::of(BPM), Beat::new(beat), None);
            let mut out = vec![0.0f32; len];
            exec.process(len, &rolling, &[], &mut [&mut out[..]]);
            frame += len;
        }
        let sent = seen.lock().unwrap().len();
        (sent, clock.losses().refused)
    };

    let (whole, none) = run(&[LONG / 4; 4]);
    assert_eq!(none, 0, "a quarter-length block fits every event");
    assert!(
        whole > SYNC_OUT_EVENT_CAPACITY,
        "the span produces more than one block can carry ({whole})"
    );

    let (sent, dropped) = run(&[LONG]);
    assert_eq!(
        sent, SYNC_OUT_EVENT_CAPACITY,
        "one block sends up to the cap"
    );
    assert_eq!(
        sent as u64 + dropped,
        whole as u64,
        "every event past the cap is counted"
    );
}

/// **A transport change on the frame of a loop wrap sends one Song Position,
/// on that frame.** Looping beats 1.0..1.1 at 120 BPM and 48 kHz (24 000
/// frames a beat), the loop wraps on frame 2 400; a change of the transport
/// (to the same rolling one, at the loop's start) cuts the block there. The
/// first piece ends on the wrap, the second starts on the wrapped beat and
/// has no wrap of its own. Block start: Song Position, Continue (beat 1.0 is
/// no Start) and the tick; ticks every 1 000 frames; then on 2 400 the
/// wrap's Song Position and the tick on the loop's start.
#[test]
fn a_segment_boundary_on_a_wrap_sends_the_song_position_once() {
    use tutti_music::LoopRange;
    // The loop starts at beat 1, a quarter note: 4 MIDI beats (sixteenths).
    const LOOP_START: u16 = 4;
    let (_ed, mut exec, _clock, seen) = rig(
        sync(SyncMessages {
            clock: true,
            mtc: false,
            ..SyncMessages::OFF
        }),
        1,
        4096,
    );
    let looping = Some(LoopRange::new(Beat::new(1.0), Beat::new(1.1)).expect("a loop"));
    let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat::new(1.0), looping);
    let mut changes = TransportChanges::NONE;
    changes
        .push(
            Offset::of(Samples::new(2_400), Samples::new(4_000)),
            rolling,
        )
        .unwrap();
    let mut out = vec![0.0f32; 4_000];
    exec.process_with_changes(4_000, &rolling, &changes, &[], &mut [&mut out[..]]);
    let got: Vec<(u64, Ump)> = seen.lock().unwrap().iter().map(|s| (s.1, s.2)).collect();
    assert_eq!(
        got,
        vec![
            (
                0,
                Ump::from(Message::System(
                    G,
                    System::SongPosition(U14::of(LOOP_START))
                ))
            ),
            (0, Ump::from(Message::System(G, System::Continue))),
            (0, Ump::from(Message::System(G, System::TimingClock))),
            (1_000, Ump::from(Message::System(G, System::TimingClock))),
            (2_000, Ump::from(Message::System(G, System::TimingClock))),
            (
                2_400,
                Ump::from(Message::System(
                    G,
                    System::SongPosition(U14::of(LOOP_START))
                ))
            ),
            (2_400, Ump::from(Message::System(G, System::TimingClock))),
            (3_400, Ump::from(Message::System(G, System::TimingClock))),
        ]
    );
}

/// **A short loop in a long block overflows the port, counted, and the
/// blocks it is cut into lose nothing.** A loop of 47.3 frames (beat 0 to
/// 47.3 / 24 000) at 120 BPM sends a Song Position and a tick on its start
/// at every wrap: about 340 wraps in 16 384 frames, so about 680 events
/// against the port's 256. One block sends the first 256 and counts the
/// rest, without panicking; the same span in blocks of 1 024 (each under the
/// cap) loses none, and what the one block sent plus what it counted is what
/// the blocks sent, exactly.
#[test]
fn a_short_loop_in_a_long_block_is_counted_past_the_cap() {
    use tutti_midi_runtime::SYNC_OUT_EVENT_CAPACITY;
    use tutti_music::FrameClock;
    use tutti_music::LoopRange;
    const TOTAL: usize = 16_384;
    let end = 47.3 / 24_000.0;

    let run = |len: usize| -> (usize, u64) {
        let (_ed, mut exec, clock, seen) = rig(
            sync(SyncMessages {
                clock: true,
                mtc: false,
                ..SyncMessages::OFF
            }),
            1,
            TOTAL,
        );
        let looping = Some(LoopRange::new(Beat::new(0.0), Beat::new(end)).expect("a loop"));
        let region = tutti_music::LoopRange::new(Beat::new(0.0), Beat::new(end));
        let mut host = FrameClock::new(Beat::new(0.0), Bpm::of(120.0), SampleRate::of(RATE));
        for _ in 0..TOTAL / len {
            let rolling = TransportPosition::new(true, Bpm::of(120.0), host.beat(), looping);
            let mut out = vec![0.0f32; len];
            exec.process(len, &rolling, &[], &mut [&mut out[..]]);
            host.advance(Samples::new(len), region);
        }
        let sent = seen.lock().unwrap().len();
        (sent, clock.losses().refused)
    };

    let (cut, none) = run(1_024);
    assert_eq!(none, 0, "blocks of 1 024 fit every event");
    assert!(cut > 2 * SYNC_OUT_EVENT_CAPACITY, "{cut} events in all");
    let (sent, dropped) = run(TOTAL);
    assert_eq!(sent, SYNC_OUT_EVENT_CAPACITY);
    assert_eq!(sent as u64 + dropped, cut as u64);
}

/// The MPE transform node over a script: one batch of MIDI 1.0 or 2.0 events a
/// block, pushed through an event inlet into the node's input, and a sink that
/// logs what the node sends.
fn transform(
    mode: &Publisher<MpeMode>,
    script: Script,
) -> (
    Editor,
    Executor,
    tutti_midi_runtime::MpeTransformControls,
    Seen,
) {
    transform_over(MpeTransformNode::new(mode), script)
}

/// [`transform`] over any node that stands where the transform does.
fn transform_over<N: tutti_graph::IntoNode>(
    node: N,
    script: Script,
) -> (Editor, Executor, N::Controls, Seen) {
    let (inlet, mut writer) = EventInletNode::pair(EventCapacity::of(4096));
    for (block, batch) in script.0.iter().enumerate() {
        for &(offset, ump) in batch {
            let frame = Frame::new(512 * block as u64 + u64::from(offset.get()));
            writer.push(Stamp::Frame(frame), ump).expect("room");
        }
    }
    rig_with(node, 1, 512, |ed| {
        ed.insert(NodeKey(3), "inlet", inlet);
        ed.spec_mut().connect_events(
            EventInputPort::new(NodeKey(1), 0),
            EventEdge::Direct(EventOutputPort::new(NodeKey(3), 0)),
        );
    })
}

/// What a sink saw, as `(channel, message)`.
fn voices(seen: &Seen) -> Vec<(u8, Voice2)> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|s| match s.2.decode() {
            Ok(Message::Midi2(address, voice)) => (address.channel.get(), voice),
            other => panic!("{other:?} is not MIDI 2.0 channel voice"),
        })
        .collect()
}

/// **A clip's stream gets the transform**: a note on member channel 1 and a bend
/// there (both MIDI 1.0, as a file's are) come out as the note and a per-note
/// bend on it; the manager's Damper Pedal comes out on the manager and on each
/// of the zone's member channels (M1-100 section 2.3.1).
#[test]
fn the_transform_node_folds_a_member_and_fans_out_a_managers_pedal() {
    let member = MidiChannel::of(1);
    let wire = Script::new(vec![vec![
        at(0, midi::m1_note_on(member, 60, U7::of(100))),
        at(
            10,
            tutti_ump::Message::midi1(
                member,
                tutti_ump::Voice1::PitchBend(U14::new(0x3FFF).unwrap()),
            )
            .into(),
        ),
        cc_at(20, 0, CCNumber::SUSTAIN, 127),
    ]]);
    let (_ed, mut exec, _c, seen) = transform(&Publisher::new(midi::lower_zone(3)), wire);
    block(&mut exec, 512);
    // The zone's default bend ranges go first, ahead of the Group's first event:
    // the master's, and on each of three members the channel's and the per-note one.
    let all = voices(&seen);
    let ranges = all
        .iter()
        .take_while(|(_, v)| matches!(v, Voice2::Registered { .. }))
        .count();
    assert_eq!(ranges, 7, "{all:?}");
    let got: Vec<_> = all
        .into_iter()
        .filter(|(_, v)| !matches!(v, Voice2::Registered { .. }))
        .collect();
    assert!(matches!(got[0], (1, Voice2::NoteOn { .. })), "{got:?}");
    assert!(
        matches!(got[1], (1, Voice2::PerNotePitchBend { note, .. }) if note.get() == 60),
        "{got:?}"
    );
    let pedals: Vec<u8> = got
        .iter()
        .filter(|(_, v)| matches!(v, Voice2::ControlChange { controller, .. } if *controller == CCNumber::SUSTAIN))
        .map(|(c, _)| *c)
        .collect();
    assert_eq!(pedals, [0, 1, 2, 3]);
}

/// **A mode published to the session's cell is adopted at the next block, once**: the
/// transform starts disabled, so a member's bend passes as a channel bend; the
/// lower zone then set, the zone's reset ranges go first (thirteen registered
/// controllers, as the input node sends), once however many blocks follow, and
/// the member's bend then folds per-note.
#[test]
fn a_mode_published_to_the_transforms_cell_is_adopted_once() {
    let member = MidiChannel::of(1);
    let bend = at(0, midi::pitch_bend(member, 0xC000_0000));
    let note = at(
        0,
        midi::note_on(member, 60, NoteVelocity::from(U7::of(100)).get()),
    );
    let cell = Publisher::new(MpeMode::Disabled);
    let (_ed, mut exec, _controls, seen) = transform(
        &cell,
        Script::new(vec![vec![bend], vec![note, bend], vec![bend]]),
    );
    block(&mut exec, 512);
    assert!(matches!(voices(&seen)[..], [(1, Voice2::PitchBend(_))]));
    seen.lock().unwrap().clear();
    cell.publish(midi::lower_zone(6));
    block(&mut exec, 512);
    block(&mut exec, 512);
    let got = voices(&seen);
    let registered = got
        .iter()
        .filter(|(_, v)| matches!(v, Voice2::Registered { .. }))
        .count();
    assert_eq!(registered, 13, "{got:?}");
    let per_note: Vec<u8> = got
        .iter()
        .filter(|(_, v)| matches!(v, Voice2::PerNotePitchBend { .. }))
        .map(|(c, _)| *c)
        .collect();
    assert_eq!(per_note, [1, 1], "{got:?}");
}

/// **A reset transform starts its Groups at the zone's ranges again**: a reset
/// receiver is back at its own defaults (M1-100 section 2.2.5's ranges are the
/// zone's, not the receiver's), so the transform's next event in a Group goes
/// behind the seven ranges of a three-member lower zone again, as its first did;
/// a second event with no reset between goes alone.
#[test]
fn a_reset_transform_sends_the_zones_ranges_again() {
    use tutti_graph::{Event, Offset, contract::Direct};
    let pedal = Event::midi(
        Offset::ZERO,
        midi::cc(MidiChannel::FIRST, CCNumber::SUSTAIN, 0),
    );
    let mut d = Direct::new(
        tutti_midi_runtime::MpeTransformNode::new(&Publisher::new(midi::lower_zone(3))),
        SampleRate::SR_48K,
        256,
    );
    let shape = tutti_graph::Node::<f32>::shape(&*d.node);
    let input = shape.event_input(0).expect("an event input");
    let output = shape.event_output(0).expect("an event output");
    let ranges = |d: &mut Direct<tutti_midi_runtime::MpeTransformNode>| {
        d.events(input, &[pedal]);
        let _ = d.block();
        d.event_output(output)
            .iter()
            .filter(|e| {
                matches!(e.kind, tutti_graph::EventKind::Midi(u)
                    if matches!(u.decode(), Ok(Message::Midi2(_, Voice2::Registered { .. }))))
            })
            .count()
    };
    assert_eq!(ranges(&mut d), 7, "the first event");
    assert_eq!(ranges(&mut d), 0, "told once");
    tutti_graph::Node::<f32>::reset(&mut *d.node);
    assert_eq!(ranges(&mut d), 7, "after the reset");
}

/// **A fork is a transform in the mode the cell held when it was forked, and
/// does not follow the cell after it**: a fork of a transform whose session
/// cell holds `Disabled`, the lower zone then written, still passes a member's
/// bend as a channel bend and sends no zone ranges, where the live transform
/// adopts the zone.
#[test]
fn a_fork_keeps_the_mode_it_was_forked_in() {
    use tutti_graph::{NodeParts, RenderMode, Unforkable};
    let member = MidiChannel::of(1);
    let bend = at(0, midi::pitch_bend(member, 0xC000_0000));
    let note = at(
        0,
        midi::note_on(member, 60, NoteVelocity::from(U7::of(100)).get()),
    );
    let script = || Script::new(vec![vec![note, bend], vec![bend]]);
    let cell = Publisher::new(MpeMode::Disabled);
    let NodeParts { fork, .. } =
        tutti_graph::IntoNode::<f32>::into_parts(MpeTransformNode::new(&cell));
    let forked = fork
        .expect("a transform forks")
        .fork(RenderMode::Realtime)
        .expect("forks");
    cell.publish(midi::lower_zone(6));

    let (_ed, mut exec, (), seen) = transform_over(Unforkable(forked.node), script());
    block(&mut exec, 512);
    block(&mut exec, 512);
    let got = voices(&seen);
    assert!(
        !got.iter().any(|(_, v)| matches!(
            v,
            Voice2::Registered { .. } | Voice2::PerNotePitchBend { .. }
        )),
        "the fork stays disabled: {got:?}"
    );
    assert_eq!(
        got.iter()
            .filter(|(_, v)| matches!(v, Voice2::PitchBend(_)))
            .count(),
        2,
        "{got:?}"
    );

    // The live transform over the same cell adopts the zone.
    let (_ed, mut exec, _c, seen) = transform_over(MpeTransformNode::new(&cell), script());
    block(&mut exec, 512);
    assert!(
        voices(&seen)
            .iter()
            .any(|(_, v)| matches!(v, Voice2::Registered { .. }))
    );
}
