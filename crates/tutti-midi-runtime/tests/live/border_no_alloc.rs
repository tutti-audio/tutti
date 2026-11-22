//! Regression gate for RT-safety: the MIDI border nodes process a block
//! without allocating, events flowing — a device inlet's through the input node
//! node's translation onto its ports, a control thread's through the event
//! inlet into the event tap's ring, and the sync node's clock, timecode and
//! Machine Control.

use tutti_buffers::MaxBlock;
use tutti_graph::{
    Editor, EventEdge, EventInletNode, EventInletWriter, EventInputPort, EventOutputPort, Harmony,
    Prepare, TransportPosition,
};
use tutti_graph::{ForkByClone, NodeKey, Stamp};
use tutti_midi_runtime::{
    DeviceGeneration, HarmonyChanges, HarmonyNode, LIVE_INLET_CAPACITY, MidiClipNode,
    MidiInputNode, MidiKeys, MidiOutNode, MpeTransformNode, SyncMessages, SyncOut, TimedHarmony,
    TimedMidiEvent,
};
use tutti_music::{FrameClock, LoopRange};
use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_ump::{MidiChannel, MidiGroup, NoteNumber, NoteVelocity, U7, Ump};
use tutti_units::{Beat, Bpm, Frame, Moment, SampleRate, Samples};

use crate::common;
use tutti_fixtures::midi;
use tutti_midi_protocol::{MpeMode, MtcRate, Recipient};
use tutti_rt::Publisher;
use tutti_ump::{Address, BendValue, ControlValue, Flex, Message, NoteKeyEvent, System, Voice2};
fn note_on(ch: u8, note: u8) -> Ump {
    midi::note_on(MidiChannel::of(ch), note, 0x8000)
}

/// A device's inlet (key `inlet`) feeding the first event input of the input node
/// node at key 1: what the device's thread pushes into.
fn device(ed: &mut Editor, inlet: u64) -> EventInletWriter {
    let (node, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    ed.insert(NodeKey(inlet), "device", node);
    ed.spec_mut().connect_events(
        EventInputPort {
            node: NodeKey(1),
            port: 0,
        },
        EventEdge::Direct(EventOutputPort {
            node: NodeKey(inlet),
            port: 0,
        }),
    );
    writer
}

/// The first frame of block `block` (blocks of 512).
fn frame_of(block: u64, offset: u64) -> Stamp {
    Stamp::Frame(Frame::new(512 * block + offset))
}

/// A fresh note on four channels for block `block`.
fn busy(device: &mut EventInletWriter, block: u64) {
    for i in 0..4u64 {
        device
            .push(
                Moment::new(Frame::new(512 * block + 40 - 10 * i), SampleRate::SR_48K),
                note_on(i as u8 * 3, 60),
            )
            .unwrap();
    }
}

#[test]
fn the_border_nodes_process_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "in", MidiInputNode::new());
    let mut dev = device(&mut ed, 4);
    let (inlet, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    ed.insert(NodeKey(2), "keys", inlet);
    let mut keys = MidiKeys::new(writer, MidiGroup::FIRST);
    let meter = tutti_timeline::ProjectMeter::default();
    let sync_meter = meter.clone();
    ed.insert(
        NodeKey(3),
        "sync",
        SyncOut::new(MidiGroup::FIRST)
            .meter(move || sync_meter.reader())
            .sending(SyncMessages {
                clock: true,
                mtc: true,
                mmc: Some(Recipient::AllCall),
                flex: true,
                metronome: true,
                rate: MtcRate::Fps30,
            }),
    );
    let mut out = common::sink(&mut ed, 1, &[(1, 0), (1, 3), (2, 0), (3, 0)]);
    ed.commit().unwrap();
    let t = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
    let mut drain = [(0, Ump([0; 4])); 256];
    // Warm up.
    busy(&mut dev, 0);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let (mut clocks, mut quarter_frames, mut sysex) = (0usize, 0usize, 0usize);
    let (mut tempos, mut meters) = (0usize, 0usize);
    for b in 1..200u32 {
        busy(&mut dev, u64::from(b));
        keys.note_on(
            Stamp::NextBlock,
            MidiChannel::FIRST,
            NoteNumber::of(60),
            NoteVelocity::new(0xC800),
        )
        .unwrap();
        // Every path of the sync output runs under the gate: a tempo that
        // changes each block (Set Tempo), a meter published off the grid
        // every 20th block (Set Time Signature on the new bars, the reader
        // lent over a changing meter), a stop and a start every 50th (Stop,
        // Start, Play, Stop) and a jump every 30th (Song Position, a Full
        // Message, a Locate).
        if b % 20 == 0 {
            let signature = if b % 40 == 0 { (4, 4) } else { (3, 4) };
            meter.set_meter(MeterMap::new([
                MeterChange::new(Beat::new(0.0), TimeSignature::of(4, 4)),
                MeterChange::new(Beat::new(0.1), TimeSignature::of(signature.0, signature.1)),
            ]));
            meter.reclaim();
        }
        let tempo = Bpm::of(120.0 + f64::from(b % 3) * 10.0);
        let at = Beat::new(f64::from(b) * 512.0 / 24_000.0 + if b % 30 == 0 { 40.0 } else { 0.0 });
        exec.process(
            512,
            &TransportPosition::new(b % 50 != 0, tempo, at, None),
            &[],
            &mut [],
        );
        let n = out.poll_into(&mut drain);
        for (_, ump) in &drain[..n] {
            match ump.decode() {
                Ok(Message::System(_, System::TimingClock)) => clocks += 1,
                Ok(Message::System(_, System::TimeCode(_))) => quarter_frames += 1,
                Ok(Message::Sysex7(..)) => sysex += 1,
                Ok(Message::Flex(_, Flex::SetTempo(_))) => tempos += 1,
                Ok(Message::Flex(_, Flex::SetTimeSignature(_))) => meters += 1,
                _ => {}
            }
        }
    }
    assert_eq!(out.dropped(), 0);
    // The emit paths ran under the gate: ticks, quarter-frames, and the Full
    // Messages and Machine Control commands (SysEx).
    assert!(
        clocks > 0 && quarter_frames > 0 && sysex > 0,
        "{clocks} ticks, {quarter_frames} quarter-frames, {sysex} SysEx packets"
    );
    // A Set Tempo for the changing tempo, a Set Time Signature for the meters
    // published mid-stream.
    assert!(tempos > 0 && meters > 0, "{tempos} tempos, {meters} meters");
}

/// **A device's departure and arrival process without allocating**: the input node
/// node that follows its generation releases what the departing device held,
/// restarts for the next, pays what it owes and settles, block after block.
#[test]
fn a_generation_change_processes_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    let (mut writer, generation) = DeviceGeneration::new();
    writer.arrive();
    ed.insert(
        NodeKey(1),
        "in",
        MidiInputNode::new().following(&generation),
    );
    let mut dev = device(&mut ed, 4);
    let mut out = common::sink(&mut ed, 1, &[(1, 0), (1, 3), (1, 6), (1, 9)]);
    ed.commit().unwrap();
    let t = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
    let mut drain = [(0, Ump([0; 4])); 256];
    // Warm up.
    busy(&mut dev, 0);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let mut offs = 0;
    for b in 1..200u32 {
        busy(&mut dev, u64::from(b));
        // A device goes every eighth block and another takes the input two
        // blocks on.
        match b % 8 {
            0 => writer.depart(),
            2 => writer.arrive(),
            _ => {}
        }
        exec.process(512, &t, &[], &mut []);
        let n = out.poll_into(&mut drain);
        offs += drain[..n]
            .iter()
            .filter(|(_, ump)| matches!(ump.note_key_event(), Some(NoteKeyEvent::Off(_))))
            .count();
    }
    assert!(offs > 0, "the departures released the keys they held");
    writer.depart();
    exec.process(512, &t, &[], &mut []);
    assert!(generation.is_idle(), "the last departure was handled");
}

/// **A device output's node passes events without allocating.**
#[test]
fn the_midi_out_node_passes_events_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "out", ForkByClone(MidiOutNode::new()));
    let mut dev = device(&mut ed, 4);
    let mut out = common::sink(&mut ed, 1, &[(1, 0)]);
    ed.commit().unwrap();
    let t = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
    let mut drain = [(0, Ump([0; 4])); 256];
    busy(&mut dev, 0);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let mut passed = 0;
    for b in 1..200u32 {
        busy(&mut dev, u64::from(b));
        exec.process(512, &t, &[], &mut []);
        passed += out.poll_into(&mut drain);
    }
    assert!(passed >= 199 * 4, "the events passed: {passed}");
    assert_eq!(out.dropped(), 0);
}

/// A bend range on MPE member channel 2, for the block starting at frame
/// `512 * block`.
fn bend_range(device: &mut EventInletWriter, block: u64) {
    device
        .push(
            frame_of(block, 0),
            midi::registered(
                MidiChannel::of(2),
                tutti_ump::Rpn::PITCH_BEND_SENSITIVITY,
                tutti_ump::RegisteredData::new(tutti_ump::U7::of(12), tutti_ump::U7::of(0))
                    .to_word(),
            ),
        )
        .unwrap();
}

/// The input node fans a member channel's bend range out over its zone
/// without allocating, and every member's port carries it.
#[test]
fn the_mpe_bend_range_fan_out_processes_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    let mode = Publisher::new(tutti_fixtures::midi::lower_zone(15));
    ed.insert(NodeKey(1), "in", MidiInputNode::new().mpe(&mode));
    let mut dev = device(&mut ed, 4);
    let mut out = common::sink(&mut ed, 1, &[(1, 15)]);
    ed.commit().unwrap();
    let t = TransportPosition::default();
    let mut drain = [(0, Ump([0; 4])); 256];
    bend_range(&mut dev, 0);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let mut reached = 0;
    for b in 1..=50 {
        bend_range(&mut dev, b);
        exec.process(512, &t, &[], &mut []);
        reached += out.poll_into(&mut drain);
    }
    assert_eq!(reached, 50, "the last member's port carries one per block");
}

/// One MPE gesture on member channel 2 of a lower zone for block `block`: a
/// bend, pressure and CC 74 before the Note On, the manager's pressure, the
/// manager's CC 74, then the Note Off.
fn gesture(device: &mut EventInletWriter, block: u64) {
    let (g, ch) = (MidiGroup::FIRST, MidiChannel::of(2));
    for ev in [
        midi::pitch_bend(Address::new(g, ch), 0xC000_0000),
        midi::channel_pressure(Address::new(g, ch), 0x4000_0000),
        midi::cc(
            Address::new(g, ch),
            tutti_ump::CCNumber::BRIGHTNESS,
            0x2000_0000,
        ),
        note_on(2, 60),
        midi::channel_pressure(Address::new(g, MidiChannel::FIRST), 0x6000_0000),
        midi::cc(
            Address::new(g, MidiChannel::FIRST),
            tutti_ump::CCNumber::BRIGHTNESS,
            7,
        ),
        midi::note_off(Address::new(g, ch), 60, 0),
    ] {
        device.push(frame_of(block, 0), ev).unwrap();
    }
}

/// The values a member was told before its Note On, the manager's pressure
/// and CC 74 combined into each held note, and a Note Off's release process
/// without allocating, block after block.
#[test]
fn the_mpe_expression_paths_process_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    let mode = Publisher::new(tutti_fixtures::midi::lower_zone(15));
    ed.insert(NodeKey(1), "in", MidiInputNode::new().mpe(&mode));
    let mut dev = device(&mut ed, 4);
    let mut out = common::sink(&mut ed, 1, &[(1, 2)]);
    ed.commit().unwrap();
    let t = TransportPosition::default();
    let mut drain = [(0, Ump([0; 4])); 256];
    gesture(&mut dev, 0);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let mut reached = 0;
    for b in 1..=50 {
        gesture(&mut dev, b);
        exec.process(512, &t, &[], &mut []);
        reached += out.poll_into(&mut drain);
    }
    assert!(reached > 0, "channel 2's port carries the gesture");
}

/// An upper-zone MPE Configuration Message for block `block` when `upper`, a
/// lower-zone one otherwise.
fn mcm(device: &mut EventInletWriter, block: u64, upper: bool) {
    use tutti_midi_protocol::{MpeZoneConfig, PitchBendSensitivity};
    let message = if upper {
        MpeZoneConfig::upper(
            3,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap()
        .to_mcm()
    } else {
        MpeZoneConfig::lower(
            3,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap()
        .to_mcm()
    };
    device.push(frame_of(block, 0), message).unwrap();
}

/// The input node adopts a host-set MPE mode, sends its zones' reset ranges
/// and applies an MCM from the wire (rebuilding the input node and sending that
/// zone's reset) without allocating: before each gated block the host asks
/// for a dual zone other than the one in force (outside the gate: a publish
/// is control-side), and the wire alternates upper and lower MCMs.
#[test]
fn an_mpe_mode_change_and_an_mcm_process_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    let cell = Publisher::new(MpeMode::Disabled);
    ed.insert(NodeKey(1), "in", MidiInputNode::new().mpe(&cell));
    let mut dev = device(&mut ed, 4);
    let mut out = common::sink(&mut ed, 1, &[(1, 15)]);
    ed.commit().unwrap();
    let t = TransportPosition::default();
    let mut drain = [(0, Ump([0; 4])); 256];
    mcm(&mut dev, 0, true);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let modes = [
        tutti_fixtures::midi::dual_zones(3, 3),
        tutti_fixtures::midi::dual_zones(5, 4),
    ];
    let mut reached = 0;
    for i in 0..50 {
        cell.publish(modes[i % 2]);
        // The device's thread pushes the block's message: upper on the odd
        // blocks, as the first (warm-up) block's was.
        mcm(&mut dev, i as u64 + 1, i % 2 == 1);
        exec.process(512, &t, &[], &mut []);
        reached += out.poll_into(&mut drain);
        cell.reclaim();
    }
    // Each block's adoption resets the upper master (channel 15), and every
    // other block's upper MCM resets it again.
    assert_eq!(reached, 75);
}

/// A loop of 64 frames walked in 512-frame blocks: each block is eight loop
/// passes of a clip (a note on and off in each, the held note ended at each
/// wrap, a sustain pedal and an off-centre bend returned at each) and of a harmony node (a chord change in each, the chord re-stated
/// at each wrap), all without allocating, block after block.
#[test]
fn a_block_of_many_loop_passes_walks_without_allocating() {
    const RATE: f64 = 48_000.0;
    let beat = |frame: f64| Beat::new(frame / 24_000.0);
    let (start, end) = (beat(1_000.0), beat(1_064.0));
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(512)),
    ));
    ed.insert(
        NodeKey(1),
        "clip",
        MidiClipNode::new([
            TimedMidiEvent::new(beat(1_010.0), note_on(0, 60)).unwrap(),
            TimedMidiEvent::new(beat(1_040.0), note_on(0, 64)).unwrap(),
            // Held across every wrap, so each wrap lifts the pedal and
            // centres the bend.
            TimedMidiEvent::new(
                beat(1_020.0),
                Message::midi2(
                    MidiChannel::FIRST,
                    Voice2::control_change(tutti_ump::CCNumber::SUSTAIN, ControlValue::MAX),
                ),
            )
            .unwrap(),
            TimedMidiEvent::new(
                beat(1_030.0),
                Message::midi2(
                    MidiChannel::FIRST,
                    Voice2::PitchBend(BendValue::new(0xA000_0000)),
                ),
            )
            .unwrap(),
        ]),
    );
    ed.insert(
        NodeKey(2),
        "harmony",
        HarmonyNode::new(
            HarmonyChanges::new([
                TimedHarmony::new(Beat::new(0.0), Harmony::chord(60, 60, 0b1001_0001)),
                TimedHarmony::new(beat(1_032.0), Harmony::chord(67, 67, 0b1001_0001)),
            ])
            .expect("two chords at distinct finite beats"),
        ),
    );
    let mut out = common::sink(&mut ed, 1, &[(1, 0), (2, 0)]);
    ed.commit().unwrap();
    let region = tutti_music::LoopRange::new(start, end).unwrap();
    let looping = Some(LoopRange::new(start, end).expect("a loop"));
    let mut clock = FrameClock::new(start, Bpm::of(120.0), SampleRate::of(RATE));
    let mut drain = [(0, Ump([0; 4])); 256];
    let mut block = |exec: &mut tutti_graph::Executor, clock: &mut FrameClock| {
        exec.process(
            512,
            &TransportPosition::new(true, Bpm::of(120.0), clock.beat(), looping),
            &[],
            &mut [],
        );
        clock.advance(Samples::new(512), Some(region));
        out.poll_into(&mut drain)
    };
    // Warm up; the clip sends its notes.
    assert!(
        block(&mut exec, &mut clock) >= 16,
        "eight passes of two notes"
    );

    let mut sent = 0;
    for _ in 0..100 {
        sent += block(&mut exec, &mut clock);
    }
    // Per pass the clip sends two note-ons, the pedal and the bend, then at the
    // wrap two note-offs, the pedal-up and the centred bend.
    assert!(sent >= 100 * 8 * 8, "every block walked its passes: {sent}");
    assert_eq!(out.dropped(), 0);
}

/// A clip that overflows its port, refuses note-offs and retries them, in
/// blocks the loss log and its ring are written in, without allocating: 300
/// notes on a 64-frame block's port of 256 (44 note-ons refused, their
/// note-offs skipped), then 200 notes ended and 200 started at once (104
/// note-offs refused, owed, retried on the next block).
#[test]
fn a_clip_overflowing_its_port_refuses_and_retries_without_allocating() {
    const RATE: f64 = 48_000.0;
    let beat = |frame: f64| Beat::new(frame / 24_000.0);
    let note = |i: u32| note_on((i / 128 % 16) as u8, (i % 128) as u8);
    let off = |i: u32| midi::note_off(MidiChannel::of((i / 128 % 16) as u8), (i % 128) as u8, 0);
    let mut events = Vec::new();
    for i in 0..300 {
        events.push(TimedMidiEvent::new(beat(10.0), note(i)).unwrap());
        events.push(TimedMidiEvent::new(beat(500.0), off(i)).unwrap());
    }
    // 200 notes that start where 200 others end, in one block.
    for i in 300..500 {
        events.push(TimedMidiEvent::new(beat(1_000.0), note(i - 300 + 1_000)).unwrap());
        events.push(TimedMidiEvent::new(beat(1_100.0), off(i - 300 + 1_000)).unwrap());
    }
    for i in 0..200 {
        events.push(TimedMidiEvent::new(beat(1_040.0), note(i + 2_000)).unwrap());
        events.push(TimedMidiEvent::new(beat(1_041.0), off(i + 2_000)).unwrap());
    }
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples::new(64)),
    ));
    ed.insert(NodeKey(1), "clip", MidiClipNode::new(events));
    let mut out = common::sink(&mut ed, 1, &[(1, 0)]);
    ed.commit().unwrap();
    let mut drain = [(0, Ump([0; 4])); 1024];
    let mut polled = 0;
    let mut f = 0u64;
    let mut block = |exec: &mut tutti_graph::Executor| {
        exec.process(
            64,
            &TransportPosition::new(true, Bpm::of(120.0), beat(f as f64), None),
            &[],
            &mut [],
        );
        f += 64;
        out.poll_into(&mut drain)
    };
    // The first block: lazy state, if any.
    polled += block(&mut exec);
    for _ in 0..24 {
        polled += block(&mut exec);
    }
    assert!(polled > 0);
    assert!(
        exec.dropped_event_count() >= 88,
        "{} refused",
        exec.dropped_event_count()
    );
}

/// The ledger on every path, with a full table: 2,100 notes sound at once
/// across two groups against 2,048 keys (the last 52 are skipped), all end
/// on one frame into a port that takes 584 of them (the rest are owed and
/// paid over the next blocks), a seek (every held note ended again) and a
/// stop. None allocates.
#[test]
fn a_clip_with_a_full_table_sends_and_pays_without_allocating() {
    let beat = |frame: u64| Beat::new(frame as f64 / 24_000.0);
    let key = |i: u32| {
        (
            MidiGroup::of((i / 2_048) as u8),
            MidiChannel::of((i % 2_048 / 128) as u8),
            (i % 128) as u8,
        )
    };
    let events = (0..2_100u32).flat_map(|i| {
        let (g, c, n) = key(i);
        [
            TimedMidiEvent::new(
                beat(u64::from(i)),
                Message::midi2(
                    Address::new(g, c),
                    Voice2::note_on(NoteNumber::of(n), NoteVelocity::CENTER),
                ),
            )
            .unwrap(),
            TimedMidiEvent::new(
                beat(4_000),
                Message::midi2(
                    Address::new(g, c),
                    Voice2::note_off(NoteNumber::of(n), NoteVelocity::MIN),
                ),
            )
            .unwrap(),
        ]
    });
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    ed.insert(NodeKey(1), "clip", MidiClipNode::new(events));
    let mut out = common::sink(&mut ed, 1, &[(1, 0)]);
    ed.commit().unwrap();
    let mut drain = [(0, Ump([0; 4])); 1024];
    let mut f = 0u64;
    // Note-ons less note-offs the sink was sent.
    let mut sounding = 0i64;
    let mut block = |exec: &mut tutti_graph::Executor, rolling: bool, frame: u64| {
        let t = TransportPosition::new(rolling, Bpm::of(120.0), beat(frame), None);
        exec.process(512, &t, &[], &mut []);
        let n = out.poll_into(&mut drain);
        for e in &drain[..n] {
            match e.1.note_key_event() {
                Some(tutti_ump::NoteKeyEvent::On(_)) => sounding += 1,
                Some(tutti_ump::NoteKeyEvent::Off(_)) => sounding -= 1,
                _ => {}
            }
        }
        n
    };
    let mut polled = block(&mut exec, true, f);
    // The table fills over blocks 0 to 4; the offs at frame 4,000 are in
    // block 7, which refuses most of them.
    for _ in 0..7 {
        f += 512;
        polled += block(&mut exec, true, f);
    }
    // The refused offs are paid over the next blocks.
    for _ in 0..8 {
        f += 512;
        polled += block(&mut exec, true, f);
    }
    // A seek back: the table is empty, the notes start again.
    for k in 0..8 {
        polled += block(&mut exec, true, 512 * k);
    }
    // A jump while the table is full, then a stop.
    polled += block(&mut exec, true, 0);
    polled += block(&mut exec, false, 0);
    polled += block(&mut exec, false, 0);
    assert!(polled > 0);
    assert_eq!(out.dropped(), 0, "the sink's ring took every event");
    assert_eq!(sounding, 0, "every note the sink heard begin has ended");
    // The table filled, offs were refused, and some were paid late.
    assert!(exec.dropped_event_count() > 52 + 1_400);
    assert!(exec.lost_event_count() < exec.dropped_event_count());
}

/// The event inlet's audio side takes events from its ring, orders them, and
/// sends them out of its port without allocating. The sends stay outside the
/// gate: they are the control thread's.
#[test]
fn the_event_inlet_processes_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    let (queue, mut tx) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    ed.insert(NodeKey(1), "timed", queue);
    let mut out = common::sink(&mut ed, 1, &[(1, 0)]);
    ed.commit().unwrap();
    let t = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
    let mut drain = [(0, Ump([0; 4])); 256];
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let msg = midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100));
    let mut heard = 0;
    for b in 1..200u64 {
        // Control side: one for this block's middle, one
        // already late, one for the next block.
        tx.push(Stamp::Frame(Frame::new(b * 512 + 200)), msg)
            .unwrap();
        tx.push(Stamp::Frame(Frame::new(3)), msg).unwrap();
        tx.push(Stamp::NextBlock, msg).unwrap();
        // The audio thread's part.
        exec.process(512, &t, &[], &mut []);
        heard += out.poll_into(&mut drain);
    }
    assert_eq!(out.dropped(), 0);
    // Every send but the last block's own frame-stamped one was heard.
    assert!(heard >= 199 * 2, "heard only {heard}");
}

/// The full-list path allocates nothing either: with the waiting list full of
/// far-future events, each block takes a due event (it displaces the latest that
/// may be lost), a far note-off (it displaces a note-on) and a later note-on (it
/// is the one dropped), every block's processing inside the gate.
#[test]
fn the_event_inlet_with_a_full_list_processes_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    let (queue, mut tx) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    ed.insert(NodeKey(1), "timed", queue);
    let mut out = common::sink(&mut ed, 1, &[(1, 0)]);
    ed.commit().unwrap();
    let t = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
    let mut drain = [(0, Ump([0; 4])); 256];
    let on = midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100));
    let off = midi::m1_note_off(MidiChannel::FIRST, 60, U7::of(0));
    // Control side: fill the list with events for frames far ahead.
    for k in 0..256u64 {
        tx.push(Stamp::Frame(Frame::new(10_000_000 + k)), on)
            .unwrap();
    }
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    for b in 1..100u64 {
        // Control side.
        tx.push(Stamp::NextBlock, on).unwrap();
        tx.push(Stamp::Frame(Frame::new(20_000_000 + b)), off)
            .unwrap();
        tx.push(Stamp::Frame(Frame::new(30_000_000 + b)), on)
            .unwrap();
        exec.process(512, &t, &[], &mut []);
        let _ = out.poll_into(&mut drain);
    }
    assert!(
        tx.counts().losses().evicted >= 99 * 2,
        "dropped only {}",
        tx.counts().losses().evicted
    );
}

/// The MPE transform node folds a member's gesture, fans a manager's pedal and
/// bend out over its zone and adopts a host-set mode (a publish outside the
/// gate: it is control-side) without allocating, block after block.
#[test]
fn the_mpe_transform_node_processes_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    let cell = Publisher::new(tutti_fixtures::midi::lower_zone(15));
    ed.insert(NodeKey(1), "mpe", MpeTransformNode::new(&cell));
    let mut dev = device(&mut ed, 4);
    let mut out = common::sink(&mut ed, 1, &[(1, 0)]);
    ed.commit().unwrap();
    let t = TransportPosition::default();
    let mut drain = [(0, Ump([0; 4])); 256];
    let manager = |device: &mut EventInletWriter, block: u64| {
        let ch = MidiChannel::FIRST;
        device
            .push(frame_of(block, 0), midi::pitch_bend(ch, 0xC000_0000))
            .unwrap();
        device
            .push(
                frame_of(block, 0),
                midi::cc(ch, tutti_ump::CCNumber::SUSTAIN, u32::MAX),
            )
            .unwrap();
    };
    gesture(&mut dev, 0);
    manager(&mut dev, 0);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let modes = [
        tutti_fixtures::midi::lower_zone(15),
        tutti_fixtures::midi::lower_zone(7),
    ];
    let mut reached = 0;
    for b in 1..=50u64 {
        cell.publish(modes[(b % 2) as usize]);
        gesture(&mut dev, b);
        manager(&mut dev, b);
        exec.process(512, &t, &[], &mut []);
        reached += out.poll_into(&mut drain);
        cell.reclaim();
    }
    assert!(reached > 50 * 16, "the fan-out reached the port: {reached}");
    assert_eq!(out.dropped(), 0);
}

/// **Machine Control out of a sync node processes a block without allocating**, a
/// transport change inside it included: a start on frame 100, a stop on
/// frame 300 and a jump with deferred play on frame 400 each send a message.
#[test]
fn machine_control_out_of_a_sync_node_processes_without_allocating() {
    use tutti_graph::{Offset, TransportChanges};

    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(512)),
    ));
    let controls = ed
        .insert(
            NodeKey(1),
            "mmc",
            SyncOut::new(MidiGroup::FIRST).sending(SyncMessages {
                mmc: Some(Recipient::AllCall),
                rate: MtcRate::Fps30,
                ..SyncMessages::OFF
            }),
        )
        .controls;
    let mut out = common::sink(&mut ed, 1, &[(1, 0)]);
    ed.commit().unwrap();
    let at = |playing: bool, beat: f64| {
        TransportPosition::new(playing, Bpm::of(120.0), Beat::new(beat), None)
    };
    let mut changes = TransportChanges::NONE;
    for (frame, to) in [
        (100, at(true, 0.0)),
        (300, at(false, 0.01)),
        (400, at(true, 9.0)),
    ] {
        changes
            .push(Offset::of(Samples::new(frame), Samples::new(512)), to)
            .unwrap();
    }
    let mut drain = [(0, Ump([0; 4])); 256];
    // Warm up: the first block only looks.
    exec.process(512, &at(false, 0.0), &[], &mut []);
    let mut sent = 0;
    for _ in 0..200 {
        exec.process_with_changes(512, &at(false, 0.0), &changes, &[], &mut []);
        sent += out.poll_into(&mut drain);
        // The next block starts away from where the last left off.
        exec.process_with_changes(512, &at(true, 3.0), &TransportChanges::NONE, &[], &mut []);
        sent += out.poll_into(&mut drain);
        // A stop, and then a transport before zero standing elsewhere: it names
        // no time code, so it is refused and counted.
        for beat in [-4.0, -8.0] {
            exec.process_with_changes(512, &at(false, beat), &TransportChanges::NONE, &[], &mut []);
            sent += out.poll_into(&mut drain);
        }
    }
    assert!(
        sent >= 200 * 4,
        "play, stop, locate and deferred play: {sent}"
    );
    assert!(
        controls.losses().refused >= 200,
        "a refusal each round: {:?}",
        controls.losses()
    );
    assert_eq!(controls.losses().lost, 0);
    assert_eq!(out.dropped(), 0);
}

/// **`reset` allocates nothing**: the executor calls it on the audio thread as
/// a node leaves bypass, on the input node and sync nodes alike, after real
/// traffic (a sync node that has sent clock, timecode, Machine Control and Flex
/// holds state in each) and not only on a fresh node.
#[test]
fn resetting_the_border_nodes_allocates_nothing() {
    use tutti_graph::contract::Direct;

    let rate = SampleRate::SR_48K;
    let mut input = Direct::new(MidiInputNode::new(), rate, 64);
    let sync = SyncOut::new(MidiGroup::FIRST).sending(SyncMessages {
        clock: true,
        mtc: true,
        mmc: Some(Recipient::AllCall),
        flex: true,
        metronome: true,
        rate: MtcRate::Fps30,
    });
    let mut sync = Direct::new(sync, rate, 64);
    let sent = |d: &mut Direct<SyncOut>, playing: bool, beat: f64| {
        let out = tutti_graph::Node::<f32>::shape(&*d.node)
            .event_output(0)
            .expect("an event output");
        d.set_transport(TransportPosition::new(
            playing,
            Bpm::of(120.0),
            Beat::new(beat),
            None,
        ));
        d.block();
        d.event_output(out).len()
    };
    let traffic = |d: &mut Direct<SyncOut>| {
        (0..8)
            .map(|k| sent(d, k != 5, 1.0 + f64::from(k) * 64.0 / 24_000.0))
            .sum::<usize>()
    };
    assert!(traffic(&mut sync) > 0, "the node has state to forget");
    assert_no_alloc::assert_no_alloc(|| {
        tutti_graph::Node::<f32>::reset(&mut *input.node);
        tutti_graph::Node::<f32>::reset(&mut *sync.node);
    });
    assert!(traffic(&mut sync) > 0, "and works after the reset");
}
