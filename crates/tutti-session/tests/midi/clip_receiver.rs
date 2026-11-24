//! A clip player node whose event output gains a receiver sends it the clip's
//! setup (`MidiClipControls::receiver_changed`): the session raises it when the
//! commit that holds the new edge is sent, and for a sink replaced at its key.

use crate::common::{MF, Seen, blocks, note, offline, play, tap};
use tutti_buffers::{ChannelLayout, InterleavedMut};
use tutti_fixtures::midi;
use tutti_graph::{EventInputPort, Fade, NodeKey, TransportPosition};
use tutti_midi_runtime::{ClipEvents, MidiClipNode, TimedMidiEvent};
use tutti_session::{Feeder, Key, Replacement, Session};
use tutti_ump::{Address, CCNumber, MidiChannel, MidiGroup, Ump};
use tutti_units::{Beat, BeatDuration, Bpm, SampleRate, Samples};

/// Frames in one block of `blocks`.
const BLOCK: u64 = 512;

fn program(n: u8) -> Ump {
    midi::program_change(Address::new(MidiGroup::FIRST, MidiChannel::FIRST), n, None)
}

fn volume() -> Ump {
    midi::cc(
        Address::new(MidiGroup::FIRST, MidiChannel::FIRST),
        CCNumber::VOLUME,
        1 << 30,
    )
}

/// A clip with the header `[program 1, volume]`, a Program Change 5 at beat 0.001 (24 frames)
/// beat 0 and a note at beat 1 (24 000 frames at 120 BPM and 48 kHz).
fn headed_clip() -> ClipEvents {
    let events =
        std::iter::once(TimedMidiEvent::new(Beat::new(0.001), program(5)).expect("finite"))
            .chain(note(60, MF, Beat::new(1.0), BeatDuration::new(0.5)));
    let events: ClipEvents = events.collect();
    events.header(
        [program(1), volume()].map(|u| TimedMidiEvent::new(Beat::new(0.0), u).expect("finite")),
    )
}

/// The clip node feeding tap A, and both taps' logs.
fn rig() -> (Session, Key<MidiClipNode>, (NodeKey, Seen), (NodeKey, Seen)) {
    let mut s = offline();
    let a = tap(&mut s);
    let b = tap(&mut s);
    let clip = s.graph().add(MidiClipNode::new([]));
    s.set(clip.events().to(headed_clip())).expect("sets");
    s.graph()
        .set_feed(a.0, Feeder::Declared, vec![clip.event_output(0)])
        .expect("tap A is there");
    let _ = s.update();
    // The feed's own telling lands at the next update, while stopped, where
    // it adds nothing: a later telling is then only about what a test wires.
    blocks(&mut s, 1);
    let _ = s.update();
    (s, clip, a, b)
}

/// Frame of the transport's start: the one stopped block `rig` renders.
const START: u64 = BLOCK;

fn wire(s: &mut Session, clip: Key<MidiClipNode>, to: NodeKey) {
    s.graph()
        .connect(clip.event_output(0), EventInputPort::new(to, 0))
        .expect("both ends are there");
    let _ = s.update();
}

fn heard(seen: &Seen) -> Vec<(u64, Ump)> {
    seen.lock().unwrap().iter().copied().collect()
}

/// **A clip playing, wired to a second receiver, sends it the setup**: the
/// header in file order and then the chase to the playhead, before the clip's
/// note. The commit that wires B is applied at the start of the next block,
/// and the clip is told by the update after that, so the burst leaves on the
/// block after it. Tap A, which already had the setup, hears it again: the
/// burst leaves the node's one output, so every receiver of that output gets
/// it.
#[test]
fn a_clip_wired_to_a_second_receiver_while_playing_sends_it_the_setup() {
    let (mut s, clip, a, b) = rig();
    play(&s);
    blocks(&mut s, 2);
    assert_eq!(
        heard(&a.1),
        vec![
            (START, program(1)),
            (START, volume()),
            (START + 24, program(5))
        ]
    );
    assert_eq!(heard(&b.1), vec![]);

    wire(&mut s, clip, b.0);
    blocks(&mut s, 1);
    assert_eq!(heard(&b.1), vec![], "applied, the clip not yet told");
    let _ = s.update();
    blocks(&mut s, 1);
    let at = START + 3 * BLOCK;
    let setup = vec![(at, program(1)), (at, volume()), (at, program(5))];
    assert_eq!(heard(&b.1), setup, "the new receiver gets the setup");
    assert_eq!(
        heard(&a.1)[3..],
        setup[..],
        "the old receiver gets it again"
    );

    blocks(&mut s, 50);
    let later = heard(&b.1);
    assert_eq!(
        later.len(),
        4,
        "the setup is sent once, then the clip's note"
    );
    assert_eq!(later[3].0, START + 24_000);
}

/// **While stopped, wiring a receiver sends nothing; the next start sends the
/// header to both.**
#[test]
fn a_clip_wired_to_a_second_receiver_while_stopped_sends_nothing_until_the_start() {
    let (mut s, clip, a, b) = rig();
    blocks(&mut s, 2);
    wire(&mut s, clip, b.0);
    blocks(&mut s, 2);
    let _ = s.update();
    blocks(&mut s, 1);
    assert_eq!(heard(&a.1), vec![]);
    assert_eq!(heard(&b.1), vec![]);
    play(&s);
    blocks(&mut s, 2);
    let first = [(6 * BLOCK, program(1)), (6 * BLOCK, volume())];
    assert_eq!(heard(&a.1)[..2], first[..]);
    assert_eq!(heard(&b.1)[..2], first[..]);
    assert_eq!(heard(&b.1).iter().filter(|e| e.1 == program(1)).count(), 1);
}

/// **Wiring an edge that exists sends nothing**: the receiver has the setup
/// from the start, and the second `connect` is not a new receiver.
#[test]
fn re_adding_an_existing_edge_sends_no_setup() {
    let (mut s, clip, _a, b) = rig();
    wire(&mut s, clip, b.0);
    blocks(&mut s, 1);
    let _ = s.update();
    play(&s);
    blocks(&mut s, 2);
    let before = heard(&b.1);
    assert_eq!(before.iter().filter(|e| e.1 == program(1)).count(), 1);
    wire(&mut s, clip, b.0);
    blocks(&mut s, 1);
    let _ = s.update();
    blocks(&mut s, 2);
    assert_eq!(heard(&b.1), before);
}

/// **A sink replaced at its key is a new receiver**: the new node never heard
/// the setup, and gets it on the block after the update that collects the swap.
#[test]
fn a_sink_replaced_at_its_key_while_the_clip_plays_gets_the_setup() {
    let (mut s, _clip, a, _b) = rig();
    play(&s);
    blocks(&mut s, 2);
    let fresh = crate::common::Tap(a.1.clone());
    s.graph()
        .replace(
            a.0,
            Replacement::new(fresh).fade(Fade::new(Samples::new(0))),
        )
        .expect("tap A is there");
    let _ = s.update();
    blocks(&mut s, 1);
    let _ = s.update();
    blocks(&mut s, 1);
    let at = START + 3 * BLOCK;
    assert_eq!(
        heard(&a.1)[3..],
        [(at, program(1)), (at, volume()), (at, program(5))]
    );
}

/// One frame of the audio side, the `n`th, on a rolling transport that has
/// run `n` frames (a beat is 24 000 frames at 120 BPM) from beat 0.01, past the
/// clip's Program Change 5 at beat 0.001.
fn tick(side: &mut tutti_session::AudioSide, n: u32) {
    side.set_transport(TransportPosition::new(
        true,
        Bpm::of(120.0),
        Beat::new(0.01 + f64::from(n) / 24_000.0),
        None,
    ));
    let mut out = [0.0f32; 2];
    side.tick(InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
}

/// **The clip is told once the executor has applied the commit, not when it
/// is sent**: with the audio side held by the test, the commit that wires B is
/// sent by `update` and lands at the next tick, and B hears nothing in that
/// block; the update that collects it raises the flag, and B gets the header
/// then the chase on the block after. Raised at the send, B would have heard
/// the setup in the first tick. (A block already running when a commit is
/// sent cannot be held back by this rig: it applies at the top of each tick.)
#[test]
fn the_setup_is_sent_after_the_executor_applied_the_wiring() {
    let mut s = Session::builder()
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let heard_b = Seen::new();
    let a = s.graph().add(crate::common::Tap(Seen::new())).erase();
    let b = s.graph().add(crate::common::Tap(heard_b.clone())).erase();
    let clip = s.graph().add(MidiClipNode::new([]));
    s.set(clip.events().to(headed_clip())).expect("sets");
    s.graph()
        .set_feed(a, Feeder::Declared, vec![clip.event_output(0)])
        .expect("tap A is there");
    let _ = s.update();
    let mut side = s.graph().take_audio_side();
    tick(&mut side, 0);
    tick(&mut side, 1);
    s.graph()
        .connect(clip.event_output(0), EventInputPort::new(b, 0))
        .expect("both ends are there");
    let _ = s.update();
    tick(&mut side, 2);
    assert_eq!(
        heard(&heard_b),
        vec![],
        "applied, and the clip not yet told"
    );
    let _ = s.update();
    tick(&mut side, 3);
    let got: Vec<Ump> = heard(&heard_b).into_iter().map(|e| e.1).collect();
    assert_eq!(got, vec![program(1), volume(), program(5)]);
}
