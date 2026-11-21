//! The node values written with the session's verbs: a clip's events
//! (`clip.events()`) and a harmony node's changes (`harmony.changes()`) reach
//! their node at the next block, an equal value publishes nothing, and a
//! removed node's target is gone.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::common::{MF, Seen, blocks, note, offline, play, tap};
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::{Cx, EventKind, Fade, Harmony, IntoNode, Io, Node, NodeParts, Prepare, Shape};
use tutti_graph::{ForkByClone, Status};
use tutti_midi_runtime::{
    ClipEvents, HarmonyChanges, HarmonyNode, MidiClipNode, TimedHarmony, TimedMidiEvent,
};
use tutti_session::{Feeder, Replacement, Session, SessionNode, ValueErrorKind};
use tutti_ump::NoteKeyEvent;
use tutti_units::{Beat, BeatDuration, Samples};

/// Frames a beat takes at 120 BPM and 48 kHz.
const BEAT: u64 = 24_000;

fn clip_of(events: impl IntoIterator<Item = TimedMidiEvent>) -> ClipEvents {
    events.into_iter().collect()
}

/// `(frame, note)` of every Note On a tap heard.
fn note_ons(seen: &Seen) -> Vec<(u64, u8)> {
    heard(seen, |e| match e.note_key_event() {
        Some(NoteKeyEvent::On(key)) => Some(key.note().get()),
        _ => None,
    })
}

/// `(frame, note)` of every Note Off a tap heard.
fn note_offs(seen: &Seen) -> Vec<(u64, u8)> {
    heard(seen, |e| match e.note_key_event() {
        Some(NoteKeyEvent::Off(key)) => Some(key.note().get()),
        _ => None,
    })
}

fn heard(seen: &Seen, pick: impl Fn(&tutti_ump::Ump) -> Option<u8>) -> Vec<(u64, u8)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|(f, e)| pick(e).map(|n| (*f, n)))
        .collect()
}

/// A clip on a tap, started and rendered a block, so its note 60 is sounding
/// (its Note Off is far off).
fn sounding() -> (Session, tutti_session::Key<MidiClipNode>, Seen) {
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .play_clip(
            synth,
            Beat::new(0.0),
            note(60, MF, Beat::new(0.0), BeatDuration::new(1_000.0)),
        )
        .expect("the tap takes MIDI");
    let clip = s.midi().clip(synth).expect("the target's clip");
    let _ = s.update();
    play(&s);
    blocks(&s, 1);
    assert_eq!(note_ons(&seen), vec![(0, 60)]);
    (s, clip, seen)
}

/// **A clip's events written to its target play at the next block**: the clip
/// holds note 60 at beat 0; a write of note 64 a third of a beat in replaces
/// it, so that is the note heard, at frame 8 000.
#[test]
fn a_clip_written_through_its_target_plays_from_the_next_block() {
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .play_clip(
            synth,
            Beat::new(0.0),
            note(60, MF, Beat::new(0.0), BeatDuration::new(1.0)),
        )
        .expect("the tap takes MIDI");
    let clip = s.midi().clip(synth).expect("the target's clip");
    s.set(clip.events().to(clip_of(note(
        64,
        MF,
        Beat::new(1.0 / 3.0),
        BeatDuration::new(1.0),
    ))))
    .unwrap();
    let _ = s.update();
    play(&s);
    blocks(&s, 18);
    assert_eq!(note_ons(&seen), vec![(8_000, 64)]);
}

/// **A read answers the clip in force**, the one written, shared.
#[test]
fn a_clips_target_reads_the_clip_written() {
    let (s, clip, _) = sounding();
    let next = Arc::new(clip_of(note(
        62,
        MF,
        Beat::new(0.0),
        BeatDuration::new(1.0),
    )));
    s.set(clip.events().to(Arc::clone(&next))).unwrap();
    assert_eq!(s.get(clip.events()).unwrap(), next);
    assert_eq!(s.get(clip.events()).unwrap().len(), 2);
}

/// **Writing the clip in force cuts no note**: note 60 is sounding; the same
/// events, built again and listed in another order, are written; no Note Off.
/// A different clip does end it, on the next block's first frame (512).
#[test]
fn writing_an_equal_clip_cuts_no_note_and_a_different_one_does() {
    let (mut s, clip, seen) = sounding();
    let mut again = note(60, MF, Beat::new(0.0), BeatDuration::new(1_000.0));
    again.reverse();
    s.set(clip.events().to(clip_of(again))).unwrap();
    let _ = s.update();
    blocks(&s, 1);
    assert_eq!(note_offs(&seen), vec![], "the same clip: nothing cut");

    s.set(clip.events().modify(|_| ClipEvents::default().into()))
        .unwrap();
    let _ = s.update();
    blocks(&s, 1);
    assert_eq!(note_offs(&seen), vec![(1_024, 60)], "a new clip ends it");
}

/// **`modify` builds a clip from the one in force**: an empty edit of a clip
/// with one note pair leaves it as it was, and the value it sees is the clip
/// the node plays.
#[test]
fn a_clip_edit_sees_the_clip_in_force() {
    let (s, clip, seen) = sounding();
    s.set(clip.events().modify(|now| {
        assert_eq!(now.len(), 2, "a note on and its off");
        Arc::clone(now)
    }))
    .unwrap();
    blocks(&s, 1);
    assert_eq!(note_offs(&seen), vec![]);
}

/// **A removed clip's target is gone**: writing it is `NoSuchTarget`, and so
/// is reading it.
#[test]
fn a_removed_clips_target_is_gone() {
    let (mut s, clip, _) = sounding();
    s.remove(clip).unwrap();
    let err = s
        .set(clip.events().to(ClipEvents::default()))
        .expect_err("the clip is gone");
    assert_eq!(err.kind, ValueErrorKind::NoSuchTarget);
    assert_eq!(
        s.get(clip.events()).expect_err("gone").kind,
        ValueErrorKind::NoSuchTarget
    );
}

/// A harmony node fed to a stand-in that logs the chords and scales it takes,
/// with their absolute frames.
struct HarmonyTap(tutti_fixtures::Record<(u64, Harmony)>);

impl Node for HarmonyTap {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            if let EventKind::Harmony(h) = e.kind {
                seen.push((cx.env.frame.get() + u64::from(e.offset.get()), h));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl IntoNode for HarmonyTap {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        IntoNode::into_parts(tutti_graph::Unforkable(self))
    }
}

impl SessionNode for HarmonyTap {
    fn reclaimer(_: &()) -> tutti_session::Reclaims {
        tutti_session::Reclaims::Nothing
    }
}

const C: Harmony = Harmony::chord(60, 60, 0b1001_0001);
const G: Harmony = Harmony::chord(67, 67, 0b1001_0001);
const C_MAJOR: Harmony = Harmony::scale(60, 0b1010_1011_0101);

fn at(beat: f64, harmony: Harmony) -> TimedHarmony {
    TimedHarmony::new(Beat::new(beat), harmony)
}

/// A harmony node with no changes, wired to a tap, and the transport rolling.
fn harmony_session() -> (
    Session,
    tutti_session::Key<HarmonyNode>,
    tutti_fixtures::Record<(u64, Harmony)>,
) {
    let mut s = offline();
    let seen = tutti_fixtures::Record::new();
    let tap = s.graph().add(HarmonyTap(seen.clone()));
    let harmony = s.graph().add(HarmonyNode::new([]));
    s.graph()
        .set_feed(tap, Feeder::Declared, vec![harmony.events_out()]);
    let _ = s.update();
    play(&s);
    (s, harmony, seen)
}

/// **Changes written to a harmony target reach its node at the next block**,
/// which re-states the context in force on its first frame: the chord and the
/// scale, at frame 512 for a write after one block; a change a beat in comes
/// at that beat.
#[test]
fn harmony_changes_written_through_the_target_reach_the_node() {
    let (mut s, harmony, seen) = harmony_session();
    blocks(&s, 1);
    assert_eq!(seen.lock().unwrap().to_vec(), vec![]);
    s.set(
        harmony
            .changes()
            .to([at(0.0, C), at(0.0, C_MAJOR), at(1.0, G)]
                .into_iter()
                .collect::<HarmonyChanges>()),
    )
    .unwrap();
    let _ = s.update();
    blocks(&s, 1);
    assert_eq!(
        seen.lock().unwrap().to_vec(),
        vec![(512, C), (512, C_MAJOR)]
    );
    blocks(&s, (BEAT / 512) as usize);
    let heard = seen.lock().unwrap().to_vec();
    assert_eq!(heard[2], (BEAT, G), "the change on its beat");
    assert_eq!(heard.len(), 3);
}

/// **Writing the changes in force re-states nothing**, however they were
/// listed; different changes do.
#[test]
fn writing_equal_harmony_changes_restates_nothing_and_different_ones_do() {
    let (mut s, harmony, seen) = harmony_session();
    let changes = || -> HarmonyChanges { [at(0.0, C), at(0.0, C_MAJOR)].into_iter().collect() };
    s.set(harmony.changes().to(changes())).unwrap();
    let _ = s.update();
    blocks(&s, 2);
    assert_eq!(seen.lock().unwrap().len(), 2);

    s.set(harmony.changes().to(changes())).unwrap();
    let _ = s.update();
    blocks(&s, 2);
    assert_eq!(seen.lock().unwrap().len(), 2, "the same changes: nothing");

    s.set(
        harmony
            .changes()
            .to([at(0.0, G)].into_iter().collect::<HarmonyChanges>()),
    )
    .unwrap();
    let _ = s.update();
    blocks(&s, 1);
    assert_eq!(seen.lock().unwrap()[2..], [(2_048, G)]);
    assert_eq!(s.get(harmony.changes()).unwrap().as_slice(), [at(0.0, G)]);
}

/// **A removed harmony node's target is gone.**
#[test]
fn a_removed_harmony_nodes_target_is_gone() {
    let (mut s, harmony, _) = harmony_session();
    s.remove(harmony).unwrap();
    let err = s
        .set(harmony.changes().to(HarmonyChanges::default()))
        .expect_err("the node is gone");
    assert_eq!(err.kind, ValueErrorKind::NoSuchTarget);
}

/// Counts the notes and harmony a stand-in takes and keeps none.
#[derive(Clone)]
struct Count(Arc<AtomicUsize>);

impl Node for Count {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        self.0.fetch_add(io.events(0).len(), Ordering::Relaxed);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A block that adopts a written clip or harmony allocates nothing**: both
/// nodes are fed to counters, a clip and changes are written outside the gate,
/// and the blocks that adopt them (the note and the note off of a replaced
/// clip, the re-stated chord) run in it.
#[test]
fn blocks_adopting_written_values_do_not_allocate() {
    let mut s = offline();
    let notes = Arc::new(AtomicUsize::new(0));
    let chords = Arc::new(AtomicUsize::new(0));
    let note_sink = s.graph().add(ForkByClone(Count(Arc::clone(&notes))));
    let chord_sink = s.graph().add(ForkByClone(Count(Arc::clone(&chords))));
    s.midi()
        .play_clip(
            note_sink,
            Beat::new(0.0),
            note(60, MF, Beat::new(0.0), BeatDuration::new(1_000.0)),
        )
        .expect("the sink takes MIDI");
    let clip = s.midi().clip(note_sink).expect("the clip");
    let harmony = s.graph().add(HarmonyNode::new([]));
    s.graph()
        .set_feed(chord_sink, Feeder::Declared, vec![harmony.events_out()]);
    let _ = s.update();
    play(&s);
    blocks(&s, 2);
    let before = (
        notes.load(Ordering::Relaxed),
        chords.load(Ordering::Relaxed),
    );

    s.set((
        clip.events().to(clip_of(note(
            64,
            MF,
            Beat::new(0.1),
            BeatDuration::new(1_000.0),
        ))),
        harmony
            .changes()
            .to([at(0.0, C)].into_iter().collect::<HarmonyChanges>()),
    ))
    .unwrap();
    let _ = s.update();
    let mut buf = vec![0.0f32; 512 * 2];
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..4 {
            s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        }
    });
    let after = (
        notes.load(Ordering::Relaxed),
        chords.load(Ordering::Relaxed),
    );
    // The replaced clip's off, and the new note at 2 400 frames; the one chord.
    assert_eq!((after.0 - before.0, after.1 - before.1), (2, 1));
}

/// **A write after the clip's node is replaced reaches the new node, and the
/// old one gets nothing**: the key names the node now at it, so the write is
/// the new node's clip, which plays at the next block; the replaced node's
/// clip never sounds.
#[test]
fn a_write_after_a_clip_is_replaced_reaches_the_new_node() {
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    let clip = s.graph().add(MidiClipNode::new(note(
        60,
        MF,
        Beat::new(0.0),
        BeatDuration::new(1.0),
    )));
    s.graph()
        .set_feed(synth, Feeder::Declared, vec![clip.events_out()]);
    let _ = s.update();
    s.graph()
        .replace(
            clip,
            Replacement::new(MidiClipNode::new([])).fade(Fade::new(Samples::new(0))),
        )
        .expect("the clip is there");
    let _ = s.update();

    let written = clip_of(note(64, MF, Beat::new(1.0 / 3.0), BeatDuration::new(1.0)));
    s.set(clip.events().to(written)).unwrap();
    let _ = s.update();
    play(&s);
    blocks(&s, 18);
    assert_eq!(
        note_ons(&seen),
        vec![(8_000, 64)],
        "the new node plays the written clip, the replaced node's note 60 never"
    );
}

/// **A clip written with the `Arc` it already holds publishes nothing**, by
/// pointer or by value: no note is cut either way.
#[test]
fn writing_the_clip_in_force_by_pointer_cuts_no_note() {
    let (s, clip, seen) = sounding();
    let held = s.get(clip.events()).unwrap();
    s.set(clip.events().to(Arc::clone(&held))).unwrap();
    blocks(&s, 1);
    assert_eq!(note_offs(&seen), vec![]);
}

/// **A clip written and not yet updated is read back, while its node still
/// plays the clip in force**: note 60 sounds; a write of note 64 changes
/// nothing heard until the update, which ends note 60 on the first frame of
/// the next block (frame 1 024: block 0 sounded the note, block 1 ran pending).
#[test]
fn a_clip_written_is_pending_until_the_update_publishes_it() {
    let (mut s, clip, seen) = sounding();
    let written = Arc::new(clip_of(note(
        64,
        MF,
        Beat::new(0.0),
        BeatDuration::new(1.0),
    )));

    s.set(clip.events().to(Arc::clone(&written))).unwrap();
    assert!(Arc::ptr_eq(&s.get(clip.events()).unwrap(), &written));
    blocks(&s, 1);
    assert_eq!(
        note_offs(&seen),
        vec![],
        "the node still plays the old clip before the update"
    );
    let _ = s.update();
    blocks(&s, 1);
    assert_eq!(
        note_offs(&seen),
        vec![(1_024, 60)],
        "the update published it"
    );
}

/// **A clip write pending when its node is removed leaks nothing**: the
/// written clip is freed by the updates after (no handle on it remains).
#[test]
fn a_pending_clip_write_to_a_removed_node_is_freed() {
    let mut s = offline();
    let clip = s.graph().add(MidiClipNode::new(note(
        60,
        MF,
        Beat::new(0.0),
        BeatDuration::new(1.0),
    )));
    let _ = s.update();
    let written = Arc::new(clip_of(note(
        64,
        MF,
        Beat::new(0.0),
        BeatDuration::new(1.0),
    )));
    let gone = Arc::downgrade(&written);

    s.set(clip.events().to(written)).unwrap();
    s.remove(clip).unwrap();
    let _ = s.update();
    let _ = s.update();
    assert!(gone.upgrade().is_none(), "the pending clip is freed");
}
