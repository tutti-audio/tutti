//! A clip player node whose event output gains a receiver sends it the clip's
//! setup (`MidiClipControls::receiver_changed`): the session raises it when the
//! commit that holds the new edge is sent, and for a sink replaced at its key.

use crate::common::{Seen, blocks, headed_clip, offline, play, program, tap, tick, volume};
use std::sync::Arc;
use tutti_graph::{EventInputPort, Fade, NodeKey};
use tutti_midi_runtime::{MidiClipConfig, MidiClipNode};
use tutti_music::MeterMap;
use tutti_session::{Feeder, Key, LoadedTrack, Replacement, Session};
use tutti_tempo::TempoMap;
use tutti_timeline::ProjectMeter;
use tutti_ump::Ump;
use tutti_units::{Bpm, SampleRate, Samples};

/// Frames in one block of `blocks`.
const BLOCK: u64 = 512;

/// The clip node feeding tap A, and both taps' logs.
fn rig() -> (Session, Key<MidiClipNode>, (NodeKey, Seen), (NodeKey, Seen)) {
    let mut s = offline();
    let a = tap(&mut s);
    let b = tap(&mut s);
    let clip = s.graph().add(MidiClipNode::from(MidiClipConfig::default()));
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
    let clip = s.graph().add(MidiClipNode::from(MidiClipConfig::default()));
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

/// A track of `headed_clip` on a 120 BPM map, as `play_track` takes it.
fn track() -> LoadedTrack {
    LoadedTrack {
        events: Arc::new(headed_clip()),
        tempo: Arc::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo")),
        meter: Arc::new(ProjectMeter::new(MeterMap::default())),
    }
}

/// **The session's own clip path tells the clip too**: `play_track` puts the
/// clip's transform between the clip and its target, so a target swapped at its
/// key (a new unit at the key) is the transform's new receiver, and the clip
/// upstream sends the new unit the setup, header then chase.
#[test]
fn a_played_tracks_target_replaced_at_its_key_gets_the_setup() {
    let mut s = offline();
    let (target, seen) = tap(&mut s);
    s.midi().play_track(target, &track()).expect("plays");
    let _ = s.update();
    blocks(&mut s, 1);
    let _ = s.update();
    play(&s);
    blocks(&mut s, 2);
    let heard_before = heard(&seen).len();
    assert_eq!(heard_before, 3, "header, header, chase at the start");
    s.graph()
        .replace(
            target,
            Replacement::new(crate::common::Tap(seen.clone())).fade(Fade::new(Samples::new(0))),
        )
        .expect("the target is there");
    let _ = s.update();
    blocks(&mut s, 1);
    let _ = s.update();
    blocks(&mut s, 1);
    let at = START + 3 * BLOCK;
    assert_eq!(
        heard(&seen)[heard_before..],
        [(at, program(1)), (at, volume()), (at, program(5))]
    );
}

/// **A playing clip moved to another target sends it the setup**:
/// `retarget` re-feeds the clip's transform into the new target, which has
/// not heard the clip.
#[test]
fn a_played_clip_retargeted_while_playing_sends_the_new_target_the_setup() {
    let mut s = offline();
    let (first, first_seen) = tap(&mut s);
    let (second, second_seen) = tap(&mut s);
    s.midi().play_track(first, &track()).expect("plays");
    let _ = s.update();
    blocks(&mut s, 1);
    let _ = s.update();
    play(&s);
    blocks(&mut s, 2);
    let clip = s.midi().clip(first).expect("the track's clip").erase();
    s.midi().retarget(clip, second).expect("a target");
    let _ = s.update();
    blocks(&mut s, 1);
    assert_eq!(
        heard(&second_seen),
        vec![],
        "applied, the clip not yet told"
    );
    let _ = s.update();
    blocks(&mut s, 1);
    let at = START + 3 * BLOCK;
    assert_eq!(
        heard(&second_seen),
        [(at, program(1)), (at, volume()), (at, program(5))]
    );
    assert_eq!(heard(&first_seen).len(), 3, "the old target hears no more");
}
