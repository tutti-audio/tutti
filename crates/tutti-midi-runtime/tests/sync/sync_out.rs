//! [`SyncOut`] on the graph's clock: each message is the one the transport's
//! change calls for, on the frame the change is on.
//!
//! The oracle is the specs: MIDI 1.0 Beat Clock (24 ticks a quarter note, so a
//! tick every 1 000 frames at 120 BPM and 48 kHz) and MTC (a Quarter Frame
//! every `rate / (4 fps)` frames, the Full Message `7F 7F 01 01 hr mn sc fr`
//! with the rate in the hours byte, M1 p.52), and RP-013 for Machine Control
//! (PLAY `02`, STOP `01`, LOCATE target `44 06 01 hr mn sc fr sf`).
//! At 120 BPM and 48 kHz a beat is 24 000 frames and a second is two beats.

use tutti_graph::contract::Direct;
use tutti_graph::{
    Editor, EventKind, Executor, NodeKey, Offset, TransportChanges, TransportPosition,
};
use tutti_midi_protocol::{MtcRate, Recipient};
use tutti_midi_runtime::{SyncMessages, SyncOut, SyncOutControls};
use tutti_ump::{Message, MidiGroup, SysexStatus, System, U14, Ump};
use tutti_units::{Beat, Bpm, SampleRate, Samples};

use tutti_fixtures::{EventRig, Seen, heard};

const G: MidiGroup = MidiGroup::FIRST;
const RATE: f64 = 48_000.0;
const BLOCK: usize = 4096;
const BEAT: f64 = 24_000.0;

/// A beat `frames` frames after beat `from`, at 120 BPM and 48 kHz.
fn beat(from: f64, frames: usize) -> Beat {
    Beat::new(from + frames as f64 / BEAT)
}

fn stopped(at: Beat) -> TransportPosition {
    TransportPosition::new(false, Bpm::of(120.0), at, None)
}

fn rolling(at: Beat) -> TransportPosition {
    TransportPosition::new(true, Bpm::of(120.0), at, None)
}

struct Rig {
    _editor: Editor,
    exec: Executor,
    seen: Seen,
    controls: std::sync::Arc<SyncOutControls>,
    /// The first frame of the next block.
    frame: u64,
}

impl Rig {
    fn new(group: MidiGroup, messages: SyncMessages) -> Self {
        Self::over(SyncOut::new(group).sending(messages))
    }

    fn over(node: SyncOut) -> Self {
        Self::at(RATE, node)
    }

    /// A node in a graph at `rate`.
    fn at(rate: f64, node: SyncOut) -> Self {
        let mut rig = EventRig::events_only(rate, BLOCK, 1);
        let controls = rig.editor.insert(NodeKey(1), "sync", node).controls;
        rig.feed(NodeKey(1), 0, 0);
        rig.commit();
        Self {
            _editor: rig.editor,
            exec: rig.exec,
            seen: rig.seen,
            controls,
            frame: 0,
        }
    }

    /// One block of `len` frames from `start`, the transport becoming each
    /// `(frame, position)` of `changes` on its frame. What the node sent, on
    /// the graph's frames.
    fn block(
        &mut self,
        len: usize,
        start: TransportPosition,
        changes: &[(usize, TransportPosition)],
    ) -> Vec<(u64, Ump)> {
        let mut cuts = TransportChanges::NONE;
        for &(frame, to) in changes {
            cuts.push(Offset::of(Samples::new(frame), Samples::new(len)), to)
                .unwrap();
        }
        self.seen.lock().unwrap().clear();
        self.exec
            .process_with_changes(len, &start, &cuts, &[], &mut []);
        self.frame += len as u64;
        heard(&self.seen)
    }
}

fn clock() -> SyncMessages {
    SyncMessages {
        clock: true,
        ..SyncMessages::OFF
    }
}

fn system(group: MidiGroup, message: System) -> Ump {
    Ump::from(Message::System(group, message))
}

/// The SysEx7 messages among `events`: the frame of each one's first packet
/// and its payload bytes.
fn sysex(events: &[(u64, Ump)]) -> Vec<(u64, Vec<u8>)> {
    let mut found = Vec::new();
    let mut open: Option<(u64, Vec<u8>)> = None;
    for &(frame, ump) in events {
        let Ok(Message::Sysex7(_, packet)) = ump.decode() else {
            continue;
        };
        let (at, mut bytes) = open.take().unwrap_or((frame, Vec::new()));
        bytes.extend_from_slice(packet.data());
        match packet.status() {
            SysexStatus::Start | SysexStatus::Continue => open = Some((at, bytes)),
            _ => found.push((at, bytes)),
        }
    }
    found
}

/// What `events` carry of System message `pick`, each on its frame.
fn systems<T>(events: &[(u64, Ump)], pick: impl Fn(System) -> Option<T>) -> Vec<(u64, T)> {
    events
        .iter()
        .filter_map(|&(frame, ump)| match ump.decode() {
            Ok(Message::System(_, message)) => pick(message).map(|t| (frame, t)),
            _ => None,
        })
        .collect()
}

/// **Start, Continue, Song Position and Stop are each on the frame the
/// transport changes**, with the 24-ppqn ticks between them on a grid from
/// the start beat: a tick every 1 000 frames.
///
/// Stopped at beat 1 (a quarter note: 4 MIDI beats), rolling from frame 100 of
/// the second block: Song Position and Continue on 2 148 (beat 1 is no
/// Start), a tick there and 1 000 on. In the third block, rolling on, it
/// stops on frame 300.
#[test]
fn a_start_a_stop_and_a_continue_are_on_their_frames() {
    let mut rig = Rig::new(G, clock());
    assert!(rig.block(2048, stopped(Beat::new(1.0)), &[]).is_empty());
    let continued = rig.block(
        2048,
        stopped(Beat::new(1.0)),
        &[(100, rolling(Beat::new(1.0)))],
    );
    let tick = system(G, System::TimingClock);
    assert_eq!(
        continued,
        [
            (2148, system(G, System::SongPosition(U14::of(4)))),
            (2148, system(G, System::Continue)),
            (2148, tick),
            (3148, tick),
        ]
    );
    // The tick after 3 148 is on 4 148: frame 52 of the third block, before
    // the stop on frame 300 of it.
    let stopping = rig.block(
        2048,
        rolling(beat(1.0, 1948)),
        &[(300, stopped(beat(1.0, 1948 + 300)))],
    );
    assert_eq!(
        stopping,
        [(4148, tick), (4096 + 300, system(G, System::Stop))]
    );
    // Stopped, it sends nothing more.
    assert!(rig.block(2048, stopped(beat(1.0, 2248)), &[]).is_empty());
}

/// **A start from beat 0 is Start** (not Continue with a Song Position), on its
/// frame, with the tick of beat 0 on it.
#[test]
fn a_start_from_the_top_is_start() {
    let mut rig = Rig::new(G, clock());
    rig.block(1024, stopped(Beat::new(0.0)), &[]);
    let got = rig.block(
        1024,
        stopped(Beat::new(0.0)),
        &[(200, rolling(Beat::new(0.0)))],
    );
    let tick = system(G, System::TimingClock);
    assert_eq!(got, [(1224, system(G, System::Start)), (1224, tick)]);
}

/// **The Quarter Frames are one every `rate / (4 fps)` frames carrying the
/// eight nibbles of the position (M1 p.52).** At 30 fps a frame is 1 600 frames
/// at 48 kHz, a quarter-frame 400: from beat 0 piece `p` is on `400 p`, the
/// first cycle `00:00:00:00` (piece 7 carries the rate code `11` in its hours
/// nibble: `0x76`), the next cycle, two frames on, `00:00:00:02`.
#[test]
fn the_quarter_frames_are_the_eight_nibbles_four_to_a_frame() {
    let mut rig = Rig::new(
        G,
        SyncMessages {
            clock: true,
            mtc: true,
            rate: MtcRate::Fps30,
            ..SyncMessages::OFF
        },
    );
    let got = rig.block(3700, rolling(Beat::new(0.0)), &[]);
    let pieces = systems(&got, |m| match m {
        System::TimeCode(piece) => Some(piece.get()),
        _ => None,
    });
    let frames: Vec<u64> = pieces.iter().map(|p| p.0).collect();
    assert_eq!(frames, (0..10).map(|k| 400 * k).collect::<Vec<_>>());
    let values: Vec<u8> = pieces.iter().map(|p| p.1).collect();
    assert_eq!(
        values,
        [0x00, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x76, 0x02, 0x10]
    );
}

fn full_message(rate_bits: u8, seconds: u8, frame: u8) -> Vec<u8> {
    vec![0x7F, 0x7F, 0x01, 0x01, rate_bits, 0, seconds, frame]
}

/// **A locate is an MTC Full Message on its frame (M1 p.52)**: a cue while
/// stopped, a jump while playing and a loop wrap each state their position at
/// the node's rate (25 fps here: hours byte `0x20`).
#[test]
fn a_cue_a_jump_and_a_wrap_are_full_messages_on_their_frames() {
    use tutti_music::LoopRange;
    let mut rig = Rig::new(
        G,
        SyncMessages {
            clock: true,
            mtc: true,
            rate: MtcRate::Fps25,
            ..SyncMessages::OFF
        },
    );
    // Cue: stopped at beat 2 (1 s: 00:00:01:00); standing still says nothing more.
    let cue = sysex(&rig.block(BLOCK, stopped(Beat::new(2.0)), &[]));
    assert_eq!(cue, [(0, full_message(0x20, 1, 0))]);
    assert!(sysex(&rig.block(BLOCK, stopped(Beat::new(2.0)), &[])).is_empty());
    // Rolling on from the cued beat: no second message. A jump to beat 40
    // (20 s) on frame 300 of the next block is one on that frame.
    let rolled = rig.block(BLOCK, rolling(Beat::new(2.0)), &[]);
    assert!(sysex(&rolled).is_empty());
    let from = 3 * BLOCK as u64;
    let jump = sysex(&rig.block(
        BLOCK,
        rolling(beat(2.0, BLOCK)),
        &[(300, rolling(Beat::new(40.0)))],
    ));
    assert_eq!(jump, [(from + 300, full_message(0x20, 20, 0))]);
    // A loop of beats 2.0 to 2.1 (2 400 frames) wraps on frame 2 400 of a block
    // that starts at its start, a jump back from beat 40: a message of the
    // loop's start, 1 s, at the block's start and on the wrap.
    let looping = Some(LoopRange::new(Beat::new(2.0), Beat::new(2.1)).expect("a loop"));
    let start = TransportPosition::new(true, Bpm::of(120.0), Beat::new(2.0), looping);
    let wrap = sysex(&rig.block(BLOCK, start, &[]));
    let from = 4 * BLOCK as u64;
    assert_eq!(
        wrap,
        [
            (from, full_message(0x20, 1, 0)),
            (from + 2400, full_message(0x20, 1, 0))
        ]
    );
}

const PLAY: [u8; 4] = [0x7F, 0x7F, 0x06, 0x02];
const STOP: [u8; 4] = [0x7F, 0x7F, 0x06, 0x01];

fn mmc(rate: MtcRate) -> SyncMessages {
    SyncMessages {
        mmc: Some(Recipient::AllCall),
        rate,
        ..SyncMessages::OFF
    }
}

/// **A start and a stop in one block are both sent**, each on its frame
/// (RP-013 pp.20 and 28): PLAY on 4 096 + 100, STOP on 4 096 + 300. The first
/// block only looks.
#[test]
fn a_start_and_a_stop_in_one_block_both_go() {
    let mut rig = Rig::new(G, mmc(MtcRate::Fps30));
    assert!(rig.block(BLOCK, stopped(Beat::new(0.0)), &[]).is_empty());
    let got = sysex(&rig.block(
        BLOCK,
        stopped(Beat::new(0.0)),
        &[
            (100, rolling(Beat::new(0.0))),
            (300, stopped(beat(0.0, 200))),
        ],
    ));
    assert_eq!(
        got,
        [
            (BLOCK as u64 + 100, PLAY.to_vec()),
            (BLOCK as u64 + 300, STOP.to_vec())
        ]
    );
}

/// **One rate states every time code**: the MTC Full Message and the MMC
/// LOCATE of the same locate are the same time, in the same rate, and a rate
/// set between blocks moves both. Beat 4 is 2 s: `00:00:02:00` at 25 fps (hours
/// byte `0x20`), and beat 6 is 3 s, `00:00:03:00` at 30 fps (`0x60`).
#[test]
fn mtc_and_an_mmc_locate_state_one_rate() {
    let mut rig = Rig::new(
        G,
        SyncMessages {
            clock: true,
            mtc: true,
            mmc: Some(Recipient::AllCall),
            flex: false,
            metronome: false,
            rate: MtcRate::Fps25,
        },
    );
    rig.block(BLOCK, stopped(Beat::new(0.0)), &[]);
    let at25 = sysex(&rig.block(BLOCK, stopped(Beat::new(4.0)), &[]));
    let locate = |hours: u8| vec![0x7F, 0x7F, 0x06, 0x44, 0x06, 0x01, hours, 0, 2, 0, 0];
    assert_eq!(
        at25,
        [
            (BLOCK as u64, full_message(0x20, 2, 0)),
            (BLOCK as u64, locate(0x20))
        ]
    );
    rig.controls.messages().store(SyncMessages {
        rate: MtcRate::Fps30,
        ..rig.controls.messages().load()
    });
    let at30 = sysex(&rig.block(BLOCK, stopped(Beat::new(6.0)), &[]));
    let locate = |hours: u8| vec![0x7F, 0x7F, 0x06, 0x44, 0x06, 0x01, hours, 0, 3, 0, 0];
    assert_eq!(
        at30,
        [
            (2 * BLOCK as u64, full_message(0x60, 3, 0)),
            (2 * BLOCK as u64, locate(0x60))
        ]
    );
}

/// **Every message is on the node's Group**: the clock's System messages, the
/// Flex Data and the SysEx packets of Machine Control and MTC alike.
#[test]
fn every_message_is_on_the_nodes_group() {
    let group = MidiGroup::of(5);
    let mut rig = Rig::new(
        group,
        SyncMessages {
            clock: true,
            mtc: true,
            mmc: Some(Recipient::AllCall),
            flex: true,
            metronome: false,
            rate: MtcRate::Fps30,
        },
    );
    rig.block(BLOCK, stopped(Beat::new(0.0)), &[]);
    let got = rig.block(
        BLOCK,
        stopped(Beat::new(0.0)),
        &[(100, rolling(Beat::new(0.0)))],
    );
    assert!(got.len() > 10, "{}", got.len());
    for (frame, ump) in got {
        assert_eq!(ump.group(), Some(group), "frame {frame}: {ump:?}");
    }
}

/// **Nothing is sent until a host sets it, and a setting lands whole at the next
/// block.** Off, a rolling transport is silent; set to the clock, the next block
/// resyncs the receiver with a Song Position of where it stands (no Start is
/// made up). Turning MMC on while rolling sends nothing, and the
/// stop after it is STOP.
#[test]
fn a_setting_lands_at_the_next_block() {
    let mut rig = Rig::new(G, SyncMessages::OFF);
    assert!(rig.block(BLOCK, rolling(Beat::new(1.0)), &[]).is_empty());
    rig.controls.messages().store(clock());
    let got = rig.block(BLOCK, rolling(beat(1.0, BLOCK)), &[]);
    // 1 + 4 096 / 24 000 beats is 4.68 sixteenths: the nearest is 5.
    assert_eq!(
        got[0],
        (BLOCK as u64, system(G, System::SongPosition(U14::of(5))))
    );
    assert!(!got.iter().any(|e| e.1 == system(G, System::Start)));

    let mut rig = Rig::new(G, SyncMessages::OFF);
    rig.block(BLOCK, rolling(Beat::new(1.0)), &[]);
    rig.controls.messages().store(mmc(MtcRate::Fps30));
    assert!(rig.block(BLOCK, rolling(beat(1.0, BLOCK)), &[]).is_empty());
    let got = sysex(&rig.block(
        BLOCK,
        rolling(beat(1.0, 2 * BLOCK)),
        &[(10, stopped(beat(1.0, 2 * BLOCK + 10)))],
    ));
    assert_eq!(got, [(2 * BLOCK as u64 + 10, STOP.to_vec())]);
}

/// **A forked sync node runs, over what the live one sent at the fork.**
#[test]
fn a_fork_sends_what_the_live_node_sent_at_the_fork() {
    use tutti_graph::{ForkMode, ForkTarget};
    // A fork keeps what reaches the master's output: the sink has one.
    let mut rig = EventRig::new(RATE, BLOCK, 1);
    let controls = rig
        .editor
        .insert(
            NodeKey(1),
            "sync",
            SyncOut::new(G).sending(SyncMessages::OFF),
        )
        .controls;
    rig.feed(NodeKey(1), 0, 0);
    rig.commit();
    controls.messages().store(clock());
    let prepare = *rig.editor.prepare();
    let (_fork, mut exec) = rig
        .editor
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect("forks");
    let mut out = vec![0.0f32; 512];
    exec.process(512, &rolling(Beat::new(0.0)), &[], &mut [&mut out[..]]);
    let got: Vec<_> = heard(&rig.seen).into_iter().map(|e| e.1).collect();
    assert_eq!(got[0], system(G, System::Start));
    assert_eq!(got[1], system(G, System::TimingClock));
}

/// **Flex Data states the tempo and the node's meter (M2-104 7.5.3 and 7.5.4)**
/// on the first tick and bar line of a start, with no Beat Clock: Set Tempo for
/// 120 BPM (500 000 µs a quarter, in 10 ns units 50 000 000) and Set Time
/// Signature 3/4 (numerator 3, denominator 2^2, 8 thirty-seconds a beat),
/// both on the start's frame, and nothing more while the transport rolls on.
#[test]
fn flex_states_the_tempo_and_the_nodes_meter_once() {
    use tutti_music::{MeterChange, MeterMap, TimeSignature};
    let meter = std::sync::Arc::new(tutti_rt::Publisher::new(MeterMap::new([MeterChange::new(
        Beat::new(0.0),
        TimeSignature::of(3, 4),
    )])));
    let mut rig = Rig::over(
        SyncOut::new(G)
            .meter(move || meter.reader())
            .sending(SyncMessages {
                flex: true,
                ..SyncMessages::OFF
            }),
    );
    let flexes = |events: &[(u64, Ump)]| -> Vec<(u64, tutti_ump::Flex)> {
        events
            .iter()
            .filter_map(|&(frame, ump)| match ump.decode() {
                Ok(Message::Flex(_, flex)) => Some((frame, flex)),
                _ => None,
            })
            .collect()
    };
    let started = rig.block(BLOCK, rolling(Beat::new(0.0)), &[]);
    assert_eq!(
        flexes(&started),
        [
            (0, tutti_ump::Flex::SetTempo(50_000_000)),
            (
                0,
                tutti_ump::Flex::SetTimeSignature(tutti_ump::FlexTimeSignature {
                    numerator: 3,
                    denominator: 2,
                    thirty_seconds: 8,
                })
            )
        ]
    );
    assert!(
        started
            .iter()
            .all(|e| e.1 != system(G, System::TimingClock)),
        "no clock bytes"
    );
    assert!(flexes(&rig.block(BLOCK, rolling(beat(0.0, BLOCK)), &[])).is_empty());
}

/// **Set Metronome is a Complete Flex Data packet to the Group (M2-104
/// 7.5.5)**, off until set: 6/8 from the top is clocks per click 12, bar parts
/// 6 + 0 + 0 and no subdivision clicks, on the start's frame, as the words
/// `0xD010_0002 0x0C06_0000 0 0` (addrs 1, the Group); the same node with the setting off sends none.
/// Spec: m2-104/7.5.5#3, m2-104/7.5.5#4
#[test]
fn set_metronome_is_a_complete_group_packet_and_off_by_default() {
    use tutti_music::{MeterChange, MeterMap, TimeSignature};
    let meter = std::sync::Arc::new(tutti_rt::Publisher::new(MeterMap::new([MeterChange::new(
        Beat::new(0.0),
        TimeSignature::of(6, 8),
    )])));
    let sent = |metronome: bool| {
        let meter = std::sync::Arc::clone(&meter);
        let mut rig = Rig::over(SyncOut::new(G).meter(move || meter.reader()).sending(
            SyncMessages {
                metronome,
                ..SyncMessages::OFF
            },
        ));
        rig.block(BLOCK, rolling(Beat::new(0.0)), &[])
    };
    assert!(sent(false).is_empty());
    let got = sent(true);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, 0);
    assert_eq!(got[0].1.0, [0xD010_0002, 0x0C06_0000, 0, 0]);
}

/// One block of 64 frames under `transport` through `node`; what it sent.
fn drive(d: &mut Direct<SyncOut>, transport: TransportPosition) -> Vec<Ump> {
    d.set_transport(transport);
    d.block();
    d.events_out(0)
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Midi(ump) => Some(ump),
            _ => None,
        })
        .collect()
}

/// **`reset` returns the node to a freshly prepared one's state and keeps
/// what it sends**, allocating nothing. At 120 BPM and 48 kHz a 64-frame block
/// is 64/24 000 of a beat. Clock: a run-on sends no Continue, the same
/// transport after a reset is a play edge mid-song and sends one. Machine
/// Control: a stopped look and then a started one sends PLAY, but the look
/// after a reset is a first and sends nothing. The messages set stay set.
#[test]
fn reset_forgets_what_was_seen_and_keeps_what_is_sent() {
    let both = SyncMessages {
        clock: true,
        mmc: Some(Recipient::AllCall),
        ..SyncMessages::OFF
    };
    let step = 64.0 / BEAT;
    let continues = |sent: &[Ump]| sent.contains(&system(G, System::Continue));
    let plays = |sent: &[Ump]| {
        sysex(&sent.iter().map(|&u| (0, u)).collect::<Vec<_>>()) == [(0, PLAY.to_vec())]
    };

    let mut d = Direct::new(SyncOut::new(G).sending(both), SampleRate::SR_48K, 64);
    assert!(!drive(&mut d, rolling(Beat::new(1.0))).is_empty());
    let run_on = drive(&mut d, rolling(beat(1.0, 64)));
    assert!(!continues(&run_on), "a run-on is no play edge");
    assert_no_alloc::assert_no_alloc(|| tutti_graph::Node::<f32>::reset(&mut *d.node));
    let after = drive(&mut d, rolling(Beat::new(1.0 + 2.0 * step)));
    assert!(continues(&after), "a reset clock sees a play edge mid-song");
    assert!(!after.is_empty(), "the messages set are still set");

    let mut kept = Direct::new(
        SyncOut::new(G).sending(mmc(MtcRate::Fps30)),
        SampleRate::SR_48K,
        64,
    );
    assert!(
        drive(&mut kept, stopped(Beat::new(0.0))).is_empty(),
        "the first look sends nothing"
    );
    assert!(plays(&drive(&mut kept, rolling(Beat::new(0.0)))));
    let mut reset = Direct::new(
        SyncOut::new(G).sending(mmc(MtcRate::Fps30)),
        SampleRate::SR_48K,
        64,
    );
    assert!(drive(&mut reset, stopped(Beat::new(0.0))).is_empty());
    assert_no_alloc::assert_no_alloc(|| tutti_graph::Node::<f32>::reset(&mut *reset.node));
    assert!(
        drive(&mut reset, rolling(Beat::new(0.0))).is_empty(),
        "the look after a reset is a first"
    );
}

/// **An MMC Locate on a loop wrap is on the first frame of the next block**
/// (RP-013 p.28): a block ends inside the loop having wrapped, the next block
/// is the first to see the position is not where rolling would have left it.
/// The loop is beats 2.0 to 2.1 (2 400 frames); the first block of 4 096 wraps
/// on frame 2 400 and ends 1 696 frames into the loop, beat 2 + 1696/24000,
/// 1 s + 848/24000 s = 1.03533 s, which at 30 fps is frame 31.06: `00:00:01:01`
/// and 6 hundredths of a frame. One message, LOCATE then DEFERRED PLAY `03`,
/// on frame 4 096; the wrap's own frame sends nothing.
#[test]
fn an_mmc_locate_on_a_loop_wrap_is_on_the_next_blocks_first_frame() {
    use tutti_music::LoopRange;
    let looping = Some(LoopRange::new(Beat::new(2.0), Beat::new(2.1)).expect("a loop"));
    let mut rig = Rig::new(G, mmc(MtcRate::Fps30));
    let first = TransportPosition::new(true, Bpm::of(120.0), Beat::new(2.0), looping);
    assert!(
        sysex(&rig.block(BLOCK, first, &[])).is_empty(),
        "the first look"
    );
    let next = TransportPosition::new(true, Bpm::of(120.0), beat(2.0, 1696), looping);
    let got = sysex(&rig.block(BLOCK, next, &[]));
    assert_eq!(
        got,
        [(
            BLOCK as u64,
            vec![0x7F, 0x7F, 0x06, 0x44, 0x06, 0x01, 0x60, 0, 1, 1, 6, 0x03]
        )]
    );
}

/// **A graph at 44.1 kHz ticks every 918.75 frames**: `prepare` gives the
/// clock the graph's rate. Tick `k` is the first frame at or after
/// `918.75 k`: 0, 919, 1 838, 2 757 (2 756.25), 3 675 (exactly).
#[test]
fn a_44_1_khz_graph_ticks_every_918_75_frames() {
    let mut rig = Rig::at(44_100.0, SyncOut::new(G).sending(clock()));
    let got = rig.block(BLOCK, rolling(Beat::new(0.0)), &[]);
    let ticks: Vec<u64> = systems(&got, |m| (m == System::TimingClock).then_some(()))
        .iter()
        .map(|t| t.0)
        .collect();
    assert_eq!(ticks, [0, 919, 1838, 2757, 3675]);
}

/// **MTC without the Beat Clock sends Quarter Frames and Full Messages and no
/// clock bytes** (M1 p.52: timecode does not need the clock). At 30 fps from
/// beat 0: a Full Message of `00:00:00:00` and a quarter-frame every 400 frames
/// on frame 0, no Start, Timing Clock, Song Position or Stop.
#[test]
fn mtc_sends_without_the_beat_clock() {
    let mut rig = Rig::new(
        G,
        SyncMessages {
            mtc: true,
            rate: MtcRate::Fps30,
            ..SyncMessages::OFF
        },
    );
    let got = rig.block(
        1000,
        rolling(Beat::new(0.0)),
        &[(900, stopped(beat(0.0, 900)))],
    );
    assert_eq!(sysex(&got), [(0, full_message(0x60, 0, 0))]);
    let pieces: Vec<u64> = systems(&got, |m| match m {
        System::TimeCode(_) => Some(()),
        _ => None,
    })
    .iter()
    .map(|p| p.0)
    .collect();
    assert_eq!(pieces, [0, 400, 800]);
    let clock_bytes = systems(&got, |m| match m {
        System::TimeCode(_) => None,
        other => Some(other),
    });
    assert!(clock_bytes.is_empty(), "{clock_bytes:?}");
}

/// **With MTC on and the Beat Clock off, a locate states the new time as an MTC
/// Full Message and sends no Song Position Pointer and no Stop.** Song Position
/// (`F2`) and Stop (`FC`) belong to the Beat Clock (M1 p.30 and p.31: Song
/// Position counts MIDI beats of the sequence, Stop halts a sequencer's
/// clock-driven play), and a receiver slaved to time code takes its position
/// from the Full Message (M1 p.52) and keeps rolling. At 30 fps (hours byte
/// `0x60`) a jump to beat 40 on frame 300 of a block is `00:00:20:00` on that
/// frame, and a loop wrap on frame 2 400 of a block is the loop's start, beat
/// 2.0, `00:00:01:00`, on its frame; neither is followed by a System message
/// other than the Quarter Frames.
#[test]
fn a_locate_or_a_wrap_with_mtc_and_no_clock_sends_no_song_position_and_no_stop() {
    use tutti_music::LoopRange;
    let mut rig = Rig::new(
        G,
        SyncMessages {
            mtc: true,
            rate: MtcRate::Fps30,
            ..SyncMessages::OFF
        },
    );
    let not_time_code = |events: &[(u64, Ump)]| {
        systems(events, |m| match m {
            System::TimeCode(_) => None,
            other => Some(other),
        })
    };
    let first = rig.block(BLOCK, rolling(Beat::new(0.0)), &[]);
    assert!(not_time_code(&first).is_empty(), "{first:?}");

    let from = BLOCK as u64;
    let jump = rig.block(
        BLOCK,
        rolling(beat(0.0, BLOCK)),
        &[(300, rolling(Beat::new(40.0)))],
    );
    assert_eq!(sysex(&jump), [(from + 300, full_message(0x60, 20, 0))]);
    assert!(not_time_code(&jump).is_empty(), "{jump:?}");

    let looping = Some(LoopRange::new(Beat::new(2.0), Beat::new(2.1)).expect("a loop"));
    let start = TransportPosition::new(true, Bpm::of(120.0), Beat::new(2.0), looping);
    let wrap = rig.block(BLOCK, start, &[]);
    let from = 2 * BLOCK as u64;
    assert_eq!(
        sysex(&wrap),
        [
            (from, full_message(0x60, 1, 0)),
            (from + 2400, full_message(0x60, 1, 0))
        ]
    );
    assert!(not_time_code(&wrap).is_empty(), "{wrap:?}");
}

/// **A forked sync node states the real meter**, not a constant one: the fork
/// registers its own reader on the cell the live node reads. A 3/4 meter is a
/// Set Time Signature of 3, 2^2, 8 thirty-seconds on the start frame.
#[test]
fn a_fork_states_the_live_nodes_meter() {
    use tutti_graph::{ForkMode, ForkTarget};
    use tutti_music::{MeterChange, MeterMap, TimeSignature};
    let meter = std::sync::Arc::new(tutti_rt::Publisher::new(MeterMap::new([MeterChange::new(
        Beat::new(0.0),
        TimeSignature::of(3, 4),
    )])));
    let mut rig = EventRig::new(RATE, BLOCK, 1);
    rig.editor.insert(
        NodeKey(1),
        "sync",
        SyncOut::new(G)
            .meter(move || meter.reader())
            .sending(SyncMessages {
                flex: true,
                ..SyncMessages::OFF
            }),
    );
    rig.feed(NodeKey(1), 0, 0);
    rig.commit();
    let prepare = *rig.editor.prepare();
    let (_fork, mut exec) = rig
        .editor
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect("forks");
    let mut out = vec![0.0f32; 512];
    exec.process(512, &rolling(Beat::new(0.0)), &[], &mut [&mut out[..]]);
    let signatures: Vec<_> = heard(&rig.seen)
        .into_iter()
        .filter_map(|(frame, ump)| match ump.decode() {
            Ok(Message::Flex(_, tutti_ump::Flex::SetTimeSignature(s))) => Some((frame, s)),
            _ => None,
        })
        .collect();
    assert_eq!(
        signatures,
        [(
            0,
            tutti_ump::FlexTimeSignature {
                numerator: 3,
                denominator: 2,
                thirty_seconds: 8
            }
        )]
    );
}
