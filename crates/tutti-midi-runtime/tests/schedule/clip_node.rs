//! [`MidiClipNode`]: a clip plays its notes out of an event port, on the
//! frames playback reaches their beats in the block's `Env` — through a seek,
//! a loop wrap, a stop and a replaced clip — and ends the notes it leaves
//! sounding when playback jumps.
//!
//! Every test renders a clip node into a sink that logs each event it
//! receives with its absolute frame. 120 BPM at 48 kHz: a beat is 24 000
//! frames, so a frame is exact in beats.

use crate::common::{at, beat_of, off, on, rolling};
use tutti_fixtures::midi;
use tutti_fixtures::midi::key_of;
use tutti_fixtures::{EventRig, Seen, heard};
use tutti_graph::{
    Editor, Executor, IntoNode, Offset, TransportChanges, TransportPosition, Unforkable,
};
use tutti_graph::{HostValues, NodeKey};
use tutti_midi_runtime::{
    ClipEvents, MidiClipConfig, MidiClipControls, MidiClipNode, TimedMidiEvent,
};
use tutti_music::LoopRange;
use tutti_ump::Ump;
use tutti_ump::{Address, ControlValue, NoteNumber, RegisteredPerNoteController, parse_exact};
use tutti_ump::{CCNumber, Message, MidiChannel, MidiGroup, Rpn, U7, Voice2};
use tutti_units::{Beat, BeatDuration, Bpm, SampleRate, Samples};

const RATE: f64 = 48_000.0;

/// Replaces the clip `controls` publishes with `events`.
fn publish(controls: &MidiClipControls, events: impl IntoIterator<Item = TimedMidiEvent>) {
    controls.set(events.into_iter().collect::<ClipEvents>());
}

/// A clip at key 1 feeding the sink at key 2, blocks of at most `max`.
fn rig<N: IntoNode>(clip: N, max: usize) -> (Editor, Executor, N::Controls, Seen) {
    let mut rig = EventRig::events_only(RATE, max, 1);
    let controls = rig.editor.insert(NodeKey(1), "clip", clip).controls;
    rig.feed(NodeKey(1), 0, 0);
    rig.commit();
    (rig.editor, rig.exec, controls, rig.seen)
}

/// Render `frames` in blocks of `block`, rolling from frame 0.
fn roll(exec: &mut Executor, frames: u64, block: usize) {
    let mut f = 0;
    while f < frames {
        exec.process(block, &rolling(f), &[], &mut []);
        f += block as u64;
    }
}

/// **Notes land on their frames, whatever the blocks.** Onsets inside a
/// block, on a block's first and last frames, and a note-off, each at its
/// absolute frame, rendered in 256- and 100-frame blocks.
#[test]
fn notes_land_on_their_frames_whatever_the_blocks() {
    let frames = [100u64, 255, 256, 999, 1_000, 5_000];
    for block in [256usize, 100] {
        let clip = MidiClipNode::from(
            MidiClipConfig::default().events(ClipEvents::from_iter(
                frames
                    .iter()
                    .enumerate()
                    .map(|(i, &f)| at(f, if i % 2 == 0 { on(60) } else { off(60) })),
            )),
        );
        let (_ed, mut exec, _c, seen) = rig(clip, 256);
        roll(&mut exec, 6_000, block);
        let got: Vec<u64> = heard(&seen).iter().map(|&(f, _)| f).collect();
        assert_eq!(got, frames, "{block}-frame blocks");
    }
}

/// **A seek inside a block plays from the seek, on the seek's frame, and ends
/// what was sounding there.** Block 0 rolls from beat 0 and seeks at frame
/// 200 to frame 48 000. The note-on at frame 24 plays; the one at 48 100
/// plays at block frame 300; the held note 60 ends at 200, before it.
#[test]
fn a_seek_inside_a_block_plays_from_the_seek() {
    let clip = MidiClipNode::from(
        MidiClipConfig::default()
            .events(ClipEvents::from_iter([at(24, on(60)), at(48_100, on(62))])),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let mut changes = TransportChanges::NONE;
    changes
        .push(
            Offset::of(Samples::new(200), Samples::new(512)),
            rolling(48_000),
        )
        .unwrap();
    exec.process_with_changes(512, &rolling(0), &changes, &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![(24, on(60)), (200, off(60)), (300, on(62))]
    );
}

/// **A loop that wraps inside a block plays the loop's start after the wrap,
/// and ends the notes held across it.** The loop is beat [0, 1) (24 000
/// timeline frames). A 100-frame block from timeline frame 23 800 plays note
/// 60 (at 23 850); the next block, from 23 900, plays note 64 at its frame 50,
/// wraps at its frame 100, ends both there, and plays the note at loop frame
/// 48 at its frame 148. (The sink logs the executor's frames, which start at
/// 0: timeline frame 23 800 is executor frame 0.)
#[test]
fn a_loop_that_wraps_inside_a_block_plays_its_start() {
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(48, on(62)),
        at(23_850, on(60)),
        at(23_950, on(64)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let lp = Some(LoopRange::new(Beat::new(0.0), Beat::new(1.0)).expect("a loop"));
    let looping = |f: u64| TransportPosition::new(true, Bpm::of(120.0), beat_of(f), lp);
    exec.process(100, &looping(23_800), &[], &mut []);
    exec.process(512, &looping(23_900), &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![
            (50, on(60)),
            (150, on(64)),
            (200, off(60)),
            (200, off(64)),
            (248, on(62)),
        ]
    );
}

/// **Stopping ends every held note, once.** A note-on at frame 10, then a
/// stopped block: its note-off is on the stopped block's first frame, and a
/// second stopped block sends nothing.
#[test]
fn stopping_ends_the_held_notes_once() {
    let clip = MidiClipNode::from(
        MidiClipConfig::default().events(ClipEvents::from_iter([at(10, on(60))])),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    let stopped = TransportPosition::new(false, Bpm::of(120.0), beat_of(64), None);
    exec.process(64, &stopped, &[], &mut []);
    exec.process(64, &stopped, &[], &mut []);
    assert_eq!(heard(&seen), vec![(10, on(60)), (64, off(60))]);
}

/// **A new clip ends the old clip's notes, and plays from where playback
/// is.** Note 60 sounds from frame 10; the clip is replaced before the
/// second block, whose first frame gets the note-off, and the new clip's note
/// at frame 100 plays.
#[test]
fn a_new_clip_ends_the_old_clips_notes() {
    let clip = MidiClipNode::from(
        MidiClipConfig::default()
            .events(ClipEvents::from_iter([at(10, on(60)), at(1_000, off(60))])),
    );
    let (_ed, mut exec, controls, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    publish(&controls, [at(100, on(72))]);
    assert_eq!(controls.len(), 1);
    exec.process(64, &rolling(64), &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![(10, on(60)), (64, off(60)), (100, on(72))]
    );
}

/// **Without a publish the node keeps its notes across blocks; each publish
/// ends them once.** Note 60 sounds from frame 10 through three blocks with
/// no `set` (no note-off), one `set` between the third and fourth ends it on
/// the fourth block's first frame, and two more blocks send nothing more.
#[test]
fn a_block_ends_the_held_notes_once_per_publish_and_never_without_one() {
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(10, on(60)),
        at(100_000, off(60)),
    ])));
    let (_ed, mut exec, controls, seen) = rig(clip, 64);
    for f in [0, 64, 128] {
        exec.process(64, &rolling(f), &[], &mut []);
    }
    assert_eq!(heard(&seen), vec![(10, on(60))], "no publish, no release");
    publish(&controls, [at(5_000, on(72))]);
    for f in [192, 256, 320] {
        exec.process(64, &rolling(f), &[], &mut []);
    }
    assert_eq!(heard(&seen), vec![(10, on(60)), (192, off(60))]);
}

/// **Two publishes between blocks end each held note once**, on the next
/// block: notes 60 and 62 sounding, two `set`s, one block, exactly one
/// note-off each.
#[test]
fn two_publishes_between_blocks_release_each_held_note_once() {
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(10, on(60)),
        at(20, on(62)),
        at(100_000, off(60)),
    ])));
    let (_ed, mut exec, controls, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    publish(&controls, [at(5_000, on(72))]);
    publish(&controls, [at(5_000, on(73))]);
    exec.process(64, &rolling(64), &[], &mut []);
    exec.process(64, &rolling(128), &[], &mut []);
    let offs: Vec<_> = heard(&seen)
        .into_iter()
        .filter(|&(_, e)| e == off(60) || e == off(62))
        .collect();
    assert_eq!(offs, vec![(64, off(60)), (64, off(62))]);
}

/// **A fork plays the clip as it was when forked.** The live clip is
/// replaced after the fork; the forked node still plays the original's note,
/// at its frame, on the `Env` it is rendered with.
#[test]
fn a_fork_plays_the_clip_as_it_was_at_the_fork() {
    use tutti_graph::{NodeParts, RenderMode};
    let NodeParts { controls, fork, .. } = MidiClipNode::from(
        MidiClipConfig::default().events(ClipEvents::from_iter([at(300, on(60))])),
    )
    .into_parts();
    let forked = fork
        .expect("a clip is forkable")
        .fork(RenderMode::Realtime)
        .expect("a clip forks");
    publish(&controls, [at(100, on(72))]);
    let (_ed, mut exec, (), seen) = rig(Unforkable(forked.node), 256);
    roll(&mut exec, 512, 256);
    assert_eq!(heard(&seen), vec![(300, on(60))]);
}

/// **A loop that wraps exactly at a block's end ends the notes held across
/// it**, at the next block's first frame. Loop [0, 1); 100-frame blocks from
/// timeline frames 23 800 and 23 900 (which ends on the loop's end), then
/// from 0. Note 60, on at 23 850, is ended at executor frame 200.
#[test]
fn a_loop_that_wraps_at_a_block_edge_ends_its_held_notes() {
    let clip = MidiClipNode::from(
        MidiClipConfig::default().events(ClipEvents::from_iter([at(23_850, on(60))])),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let lp = Some(LoopRange::new(Beat::new(0.0), Beat::new(1.0)).expect("a loop"));
    let looping = |f: u64| TransportPosition::new(true, Bpm::of(120.0), beat_of(f), lp);
    exec.process(100, &looping(23_800), &[], &mut []);
    exec.process(100, &looping(23_900), &[], &mut []);
    exec.process(100, &looping(0), &[], &mut []);
    assert_eq!(heard(&seen), vec![(50, on(60)), (200, off(60))]);
}

/// **A beat that is not finite is refused at construction**, so a clip never
/// holds one: NaN (negative on x86, made at run time) and both infinities
/// build no event, and a finite beat does.
#[test]
fn a_beat_that_is_not_finite_builds_no_event() {
    let nan = Beat::new(0.0 / std::hint::black_box(0.0));
    for bad in [nan, Beat::new(f64::INFINITY), Beat::new(f64::NEG_INFINITY)] {
        assert_eq!(TimedMidiEvent::new(bad, on(61)), None, "{bad:?}");
    }
    let good = TimedMidiEvent::new(Beat::new(1.5), on(61)).expect("a finite beat");
    assert_eq!((good.beat(), good.event()), (Beat::new(1.5), on(61)));
}

/// **A shifted event is the same packet at the moved beat**, and a shift that
/// leaves the finite beats (an overflow) builds no event.
#[test]
fn a_shifted_event_moves_its_beat_and_refuses_a_beat_that_is_not_finite() {
    let e = TimedMidiEvent::new(Beat::new(1.5), on(61)).unwrap();
    let moved = e.shifted(BeatDuration::new(2.0)).expect("finite");
    assert_eq!((moved.beat(), moved.event()), (Beat::new(3.5), on(61)));
    assert_eq!(
        e.shifted(BeatDuration::new(-1.5)).unwrap().beat(),
        Beat::new(0.0)
    );
    let huge = TimedMidiEvent::new(Beat::new(f64::MAX), on(61)).unwrap();
    assert_eq!(huge.shifted(BeatDuration::new(f64::MAX)), None);
}

/// **Note-offs the port refuses go out in the blocks after, until none is
/// held.** 400 notes are started over two blocks (200 each, under the
/// port's 256); a seek then owes 400 note-offs: 296 on the seek block's
/// first frame (the port's 256 and its note-off room, `note_off_room`), the
/// other 104 on the next block's, which rolls on from the seek (continuous:
/// nothing but the debt would release them).
#[test]
fn refused_note_offs_are_retried_until_none_is_held() {
    let note = |i: u16| midi::note_on(MidiChannel::of((i / 128) as u8), (i % 128) as u8, 0xFFFF);
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter(
        (0..400u16).map(|i| at(if i < 200 { 10 } else { 300 }, note(i))),
    )));
    let (_ed, mut exec, _c, seen) = rig(clip, 256);
    roll(&mut exec, 512, 256);
    // The seek, far from any event, then rolling on from it.
    for f in [48_000u64, 48_256, 48_512] {
        exec.process(256, &rolling(f), &[], &mut []);
    }
    let seen = heard(&seen);
    let offs: Vec<u64> = seen
        .iter()
        .filter(|(_, ump)| key_of(ump).is_some_and(|(_, _, on)| !on))
        .map(|&(f, _)| f)
        .collect();
    assert_eq!(offs.len(), 400, "every started note is ended");
    let room = tutti_graph::note_off_room(256);
    assert_eq!(room, 296);
    assert_eq!(offs.iter().filter(|&&f| f == 512).count(), room);
    assert_eq!(offs.iter().filter(|&&f| f == 768).count(), 400 - room);
}

/// **A note lands on its frame where the beat is not a whole number of
/// frames' worth of float** (the frame, not the beat, is the source of
/// truth). At 90 BPM and 48 kHz a beat is 32 000 frames: notes at beats 1
/// and 3 land on frames 32 000 and 96 000 exactly, in 64-frame blocks whose
/// transport beat is `frame / 32 000` (not exact in binary).
#[test]
fn a_note_lands_on_its_frame_at_ninety_bpm() {
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        TimedMidiEvent::new(Beat::new(1.0), on(60)).unwrap(),
        TimedMidiEvent::new(Beat::new(3.0), on(62)).unwrap(),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 64);
    let mut f = 0u64;
    while f < 100_000 {
        let t = TransportPosition::new(true, Bpm::of(90.0), Beat::new(f as f64 / 32_000.0), None);
        exec.process(64, &t, &[], &mut []);
        f += 64;
    }
    assert_eq!(heard(&seen), vec![(32_000, on(60)), (96_000, on(62))]);
}

/// **A loop shorter than a block plays every pass the block holds.** The
/// loop is 100 frames (beats [0, 100 / 24 000)); note 60 is on at loop frame
/// 10 and off at 60. Two 512-frame blocks, the second from where the first
/// left the playhead (loop frame 12): the note plays on every pass, eleven
/// times, and is ended within each.
#[test]
fn a_loop_shorter_than_a_block_plays_every_pass() {
    let clip = MidiClipNode::from(
        MidiClipConfig::default().events(ClipEvents::from_iter([at(10, on(60)), at(60, off(60))])),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let lp = Some(LoopRange::new(Beat::new(0.0), beat_of(100)).expect("a loop"));
    let looping = |f: u64| TransportPosition::new(true, Bpm::of(120.0), beat_of(f), lp);
    exec.process(512, &looping(0), &[], &mut []);
    exec.process(512, &looping(12), &[], &mut []);
    let mut want: Vec<(u64, Ump)> = (0..11)
        .map(|k| (10 + 100 * k, on(60)))
        .chain((0..10).map(|k| (60 + 100 * k, off(60))))
        .collect();
    want.sort_unstable_by_key(|&(f, u)| (f, u.0));
    assert_eq!(heard(&seen), want);
}

/// **A note started twice and ended once leaves nothing sounding.** A key is
/// held or not: the second note-on of note 60 (frame 20, while it holds) is
/// preceded by a note-off on its frame, so a receiver that stacks a voice per
/// note-on still has one. The one note-off (frame 30) is the first note's,
/// already ended, and is swallowed: the second note sounds on until the seek
/// at frame 64 ends it.
#[test]
fn a_note_started_twice_and_ended_once_leaves_nothing_sounding() {
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(10, on(60)),
        at(20, on(60)),
        at(30, off(60)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    exec.process(64, &rolling(48_000), &[], &mut []);
    let seen = heard(&seen);
    assert_eq!(
        *seen,
        vec![(10, on(60)), (20, off(60)), (20, on(60)), (64, off(60)),]
    );
    // A receiver that starts a voice per note-on and ends one per note-off.
    let voices = seen.iter().fold(0i32, |v, (_, ump)| match key_of(ump) {
        Some((_, _, true)) => v + 1,
        Some((_, _, false)) => (v - 1).max(0),
        None => v,
    });
    assert_eq!(voices, 0, "nothing left sounding");
}

/// **Overlapping notes of one key: the newest sounds to its end, and no stray
/// note-off follows.** Each note-on ends the one sounding on its frame; the
/// note-offs of the ended notes are swallowed, so only the newest note's
/// note-off ends the key.
#[test]
fn overlapping_notes_of_one_key_leave_the_newest_whole() {
    // (case, clip events, heard events)
    let cases: [(&str, Vec<(u64, Ump)>, Vec<(u64, Ump)>); 3] = [
        (
            // A on 0..100, B on 50..150: A's note-off at 100 is swallowed.
            "overlap",
            vec![(0, on(60)), (50, on(60)), (100, off(60)), (150, off(60))],
            vec![(0, on(60)), (50, off(60)), (50, on(60)), (150, off(60))],
        ),
        (
            // A on 0..100, B on 20..50: B's note-off ends the key, A's is swallowed.
            "nested",
            vec![(0, on(60)), (100, off(60)), (20, on(60)), (50, off(60))],
            vec![(0, on(60)), (20, off(60)), (20, on(60)), (50, off(60))],
        ),
        (
            // A on 0..30, B on 10..40, C on 20..50.
            "three",
            vec![
                (0, on(60)),
                (30, off(60)),
                (10, on(60)),
                (40, off(60)),
                (20, on(60)),
                (50, off(60)),
            ],
            vec![
                (0, on(60)),
                (10, off(60)),
                (10, on(60)),
                (20, off(60)),
                (20, on(60)),
                (50, off(60)),
            ],
        ),
    ];
    for (name, events, want) in cases {
        let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter(
            events.into_iter().map(|(f, u)| at(f, u)),
        )));
        let (_ed, mut exec, _c, seen) = rig(clip, 256);
        exec.process(256, &rolling(0), &[], &mut []);
        assert_eq!(heard(&seen), want, "{name}");
    }
}

/// **A key's owner starts over at a loop wrap.** A 100-frame loop, six
/// passes: A on
/// 10..30, B on 50 and off past the loop's end, so B holds the key at every
/// wrap and the wrap ends it. Each pass then plays A whole: A's note-on owns
/// the key again, and its note-off at 30 ends it.
#[test]
fn a_keys_owner_starts_over_at_a_loop_wrap() {
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(10, on(60)),
        at(30, off(60)),
        at(50, on(60)),
        at(150, off(60)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    // Three passes, then a block from the loop's start (the third wrap falls
    // on the first block's edge, and is ended on the next block's first frame).
    for _ in 0..2 {
        exec.process(
            300,
            &looping_at(120.0, Beat::new(0.0), Beat::new(0.0), beat_of(100)),
            &[],
            &mut [],
        );
    }
    let want: Vec<(u64, Ump)> = (0..6)
        .flat_map(|k| {
            let p = 100 * k;
            [
                (p + 10, on(60)),
                (p + 30, off(60)),
                (p + 50, on(60)),
                (p + 100, off(60)),
            ]
        })
        .filter(|&(f, _)| f < 600)
        .collect();
    assert_eq!(heard(&seen), want);
}

/// A loop of `[start, end)` beats under `tempo`, rolling from `beat`.
fn looping_at(tempo: f64, beat: Beat, start: Beat, end: Beat) -> TransportPosition {
    TransportPosition::new(
        true,
        Bpm::of(tempo),
        beat,
        Some(LoopRange::new(start, end).expect("a loop")),
    )
}

/// **A pass that ends exactly on a block's edge ends what it held at the
/// next block's first frame.** A 100-frame loop, 500-frame blocks: note 60 on
/// at loop frame 95 and never off, so every wrap ends it. The fifth wrap of
/// the first block falls on its end (frame 500); the next block starts on
/// the loop's start, which reads as continuous, and still ends the note on
/// its first frame.
#[test]
fn a_pass_ending_on_a_block_edge_ends_what_it_held() {
    let clip = MidiClipNode::from(
        MidiClipConfig::default().events(ClipEvents::from_iter([at(95, on(60))])),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    exec.process(
        500,
        &looping_at(120.0, Beat::new(0.0), Beat::new(0.0), beat_of(100)),
        &[],
        &mut [],
    );
    exec.process(
        500,
        &looping_at(120.0, Beat::new(0.0), Beat::new(0.0), beat_of(100)),
        &[],
        &mut [],
    );
    let offs: Vec<u64> = heard(&seen)
        .iter()
        .filter(|&&(_, w)| w == off(60))
        .map(|&(f, _)| f)
        .collect();
    assert_eq!(offs, (1..10).map(|k| 100 * k).collect::<Vec<u64>>());
}

/// **A tempo change inside a block re-times the passes after it.** A
/// 100-frame loop at 120 BPM (beats [0, 100 / 24 000)); note 60 on at loop
/// frame 10, off at 60. At frame 250 (loop frame 50) the tempo doubles, so a
/// pass is 50 frames: the note-off still owed in that pass lands 5 frames
/// on (255), the loop wraps at 275, and each 50-frame pass after plays the
/// note 5 frames in and ends it 30 frames in.
#[test]
fn a_tempo_change_inside_a_block_re_times_the_passes() {
    let clip = MidiClipNode::from(
        MidiClipConfig::default().events(ClipEvents::from_iter([at(10, on(60)), at(60, off(60))])),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let mut changes = TransportChanges::NONE;
    changes
        .push(
            Offset::of(Samples::new(250), Samples::new(512)),
            looping_at(240.0, beat_of(50), Beat::new(0.0), beat_of(100)),
        )
        .unwrap();
    exec.process_with_changes(
        512,
        &looping_at(120.0, Beat::new(0.0), Beat::new(0.0), beat_of(100)),
        &changes,
        &[],
        &mut [],
    );
    let seen = heard(&seen);
    let frames = |w: Ump| -> Vec<u64> {
        seen.iter()
            .filter(|&&(_, x)| x == w)
            .map(|&(f, _)| f)
            .collect()
    };
    assert_eq!(frames(on(60)), vec![10, 110, 210, 280, 330, 380, 430, 480]);
    assert_eq!(frames(off(60)), vec![60, 160, 255, 305, 355, 405, 455, 505]);
}

/// **Every pass lands where the host's clock reaches its beats**, at a tempo
/// and a loop that are no whole number of frames (97 BPM at 48 kHz, a loop
/// of 0.00126 beat ≈ 37.4 frames), over four 512-frame blocks, each started
/// where the clock stands. The reference is `FrameClock` walked frame by
/// frame (an analytic walk, not a render): a note fires on the first frame
/// whose beat reaches it, and on a wrap's frame for a beat the wrap
/// crossed.
#[test]
fn every_pass_lands_where_the_hosts_clock_reaches_it() {
    use tutti_music::{FRAME_TOLERANCE, FrameClock, LoopRange as Region};
    const TEMPO: f64 = 97.0;
    let (start, end) = (Beat::new(3.0), Beat::new(3.00126));
    let fpb = RATE * 60.0 / TEMPO;
    let notes = [3.000_1, 3.000_5, 3.001_1];
    let clip = MidiClipNode::from(
        MidiClipConfig::default().events(ClipEvents::from_iter(
            notes
                .iter()
                .enumerate()
                .map(|(i, &b)| TimedMidiEvent::new(Beat::new(b), on(60 + i as u8)).unwrap()),
        )),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let region = Region::new(start, end).unwrap();
    let mut clock = FrameClock::new(
        Beat::new(2.999),
        tutti_units::Bpm::of(TEMPO),
        SampleRate::of(RATE),
    );
    for _ in 0..4 {
        exec.process(
            512,
            &looping_at(TEMPO, clock.beat(), start, end),
            &[],
            &mut [],
        );
        clock.advance(Samples::new(512), Some(region));
    }

    // The reference walk.
    let tol = FRAME_TOLERANCE / fpb;
    let mut want = Vec::new();
    let mut walk = FrameClock::new(
        Beat::new(2.999),
        tutti_units::Bpm::of(TEMPO),
        SampleRate::of(RATE),
    );
    let mut prev: Option<f64> = None;
    for f in 0..4 * 512u64 {
        let b = walk.beat().get();
        let wrapped = prev.is_some_and(|p| b < p);
        for (i, &n) in notes.iter().enumerate() {
            let reached = b + tol >= n;
            let fresh = match prev {
                None => true,
                Some(_) if wrapped => n >= start.get(),
                Some(p) => p + tol < n,
            };
            if reached && fresh {
                want.push((f, on(60 + i as u8)));
            }
        }
        prev = Some(b);
        walk.advance(Samples::new(1), Some(region));
    }
    assert!(
        want.len() > 100,
        "the loop passes many times: {}",
        want.len()
    );
    // The clip also ends what it held at each wrap; the reference counts
    // only where each pass's note-ons land.
    let ons: Vec<Ump> = (0..notes.len()).map(|i| on(60 + i as u8)).collect();
    let mut got: Vec<_> = heard(&seen)
        .iter()
        .copied()
        .filter(|(_, w)| ons.contains(w))
        .collect();
    got.sort_unstable_by_key(|&(f, u)| (f, u.0));
    want.sort_unstable_by_key(|&(f, u)| (f, u.0));
    assert_eq!(got, want);
}

/// Renders four 64-frame blocks of a clip with one note at the loop's start,
/// rolling from it, on a thread, and returns what the sink saw, or `None`
/// when that takes longer than a generous deadline.
fn sub_frame_loop(start: Beat, end: Beat) -> Option<Vec<(u64, Ump)>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
            TimedMidiEvent::new(start, on(60)).unwrap(),
        ])));
        let (_ed, mut exec, _c, seen) = rig(clip, 64);
        let mut clock =
            tutti_music::FrameClock::new(start, tutti_units::Bpm::of(120.0), SampleRate::of(RATE));
        let region = tutti_music::LoopRange::new(start, end).unwrap();
        for _ in 0..4 {
            exec.process(
                64,
                &looping_at(120.0, clock.beat(), start, end),
                &[],
                &mut [],
            );
            clock.advance(Samples::new(64), Some(region));
        }
        let _ = tx.send(heard(&seen));
    });
    rx.recv_timeout(std::time::Duration::from_secs(10)).ok()
}

/// **A loop shorter than a frame costs one pass a frame.** A one-ulp loop
/// at beat 4 (`4.0.next_up()`, a realistic host input) and a loop of 1e-300
/// beat at 0: each frame wraps once, so the note at the loop's start plays
/// on every frame (ended on the next), and four 64-frame blocks finish at
/// once.
#[cfg_attr(miri, ignore = "a wall-clock deadline")]
#[test]
fn a_loop_shorter_than_a_frame_costs_one_pass_a_frame() {
    for (start, end) in [
        (Beat::new(4.0), Beat::new(4.0f64.next_up())),
        (Beat::new(0.0), Beat::new(1e-300)),
    ] {
        let seen = sub_frame_loop(start, end)
            .unwrap_or_else(|| panic!("[{start:?}, {end:?}) did not finish"));
        let ons = seen.iter().filter(|&&(_, w)| w == on(60)).count();
        let offs = seen.iter().filter(|&&(_, w)| w == off(60)).count();
        assert_eq!((ons, offs), (256, 255), "[{start:?}, {end:?})");
    }
}

/// **A clip that sets an RPN with a CC 6 alone keeps it, at the CC 6's own
/// frame** (M2-104 D.3.3: a CC 6 waits for a CC 38, a later CC 6 or a new
/// select; the clip's end is the one trigger a stream never sends, so the
/// loader flushes). `B0 65 00, B0 64 00, B0 06 0C` with notes after it on
/// other beats: one Registered Controller, bank 0 index 0, 12 semitones
/// zero-extended (M2-115 section 4.1, `12 << 25`), on the CC 6's frame and
/// before the later note.
#[test]
fn a_clip_ending_on_a_lone_cc_6_flushes_its_rpn_at_the_cc_6() {
    use tutti_ump::{CCNumber, Message, U7, Voice2};
    let ch = MidiChannel::FIRST;
    let cc = |n: CCNumber, v: u8| midi::m1_cc(ch, n, U7::of(v));
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(100, cc(CCNumber::RPN_MSB, 0)),
        at(200, cc(CCNumber::RPN_LSB, 0)),
        at(300, cc(CCNumber::DATA_ENTRY, 12)),
        at(5_000, on(60)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 256);
    roll(&mut exec, 6_000, 256);
    let seen = heard(&seen);
    let frames: Vec<u64> = seen.iter().map(|&(f, _)| f).collect();
    assert_eq!(frames, [300, 5_000]);
    let Ok(Message::Midi2(_, Voice2::Registered { bank, index, value })) = seen[0].1.decode()
    else {
        panic!("a Registered Controller, got {:?}", seen[0].1.decode());
    };
    assert_eq!((bank.get(), index.get(), value.get()), (0, 0, 12 << 25));
}

// ---------------------------------------------------------------------------
// Controllers back to their normal positions when playback stops or jumps.
// ---------------------------------------------------------------------------

const DOWN: u32 = 0xFFFF_FFFF;
const CENTRE: u32 = 0x8000_0000;

fn pedal(channel: u8, controller: CCNumber, value: u32) -> Ump {
    midi::cc(MidiChannel::of(channel), controller, value)
}

fn bend(channel: u8, value: u32) -> Ump {
    midi::pitch_bend(MidiChannel::of(channel), value)
}

/// **A stop returns every pedal and the bend to their normal positions,
/// after the note-offs, once.** MIDI 1.0 Detailed Spec p. 31 (Stop): controllers
/// off their initialized position (pitch wheels, sustain pedal, etc.) return
/// to normal; A-5: a pedal down outranks Note Off and All Notes Off until it
/// is released, so the offs come first and the pedals are lifted after them.
/// Sustain, sostenuto, soft and hold 2 are down and the bend is off centre;
/// a second stopped block sends nothing.
#[test]
fn stopping_lifts_the_pedals_and_centres_the_bend_after_the_note_offs() {
    let sustain = CCNumber::SUSTAIN;
    let sostenuto = CCNumber::SOSTENUTO;
    let soft = CCNumber::SOFT_PEDAL;
    let hold2 = CCNumber::new(69).unwrap();
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(10, on(60)),
        at(20, pedal(0, sustain, DOWN)),
        at(21, pedal(0, sostenuto, DOWN)),
        at(22, pedal(0, soft, DOWN)),
        at(23, pedal(0, hold2, DOWN)),
        at(24, bend(0, 0x9000_0000)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    let stopped = TransportPosition::new(false, Bpm::of(120.0), beat_of(64), None);
    exec.process(64, &stopped, &[], &mut []);
    exec.process(64, &stopped, &[], &mut []);
    let seen = heard(&seen);
    let after: Vec<(u64, Ump)> = seen.iter().copied().filter(|&(f, _)| f == 64).collect();
    assert_eq!(
        after,
        vec![
            (64, off(60)),
            (64, pedal(0, sustain, 0)),
            (64, pedal(0, sostenuto, 0)),
            (64, pedal(0, soft, 0)),
            (64, pedal(0, hold2, 0)),
            (64, bend(0, CENTRE)),
        ]
    );
    assert_eq!(seen.len(), 6 + 6, "nothing after the first stopped block");
}

/// **A controller already at its normal position is not sent again.**
/// Channel 0's sustain goes down and up and its bend goes off centre and
/// back; channel 1's sustain stays down. A stop sends one pedal-up, for
/// channel 1, and nothing else (M1 p. 31 asks only for controllers off
/// their initialized position).
#[test]
fn a_controller_at_rest_is_not_resent_at_a_stop() {
    let s = CCNumber::SUSTAIN;
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(10, pedal(0, s, DOWN)),
        at(11, pedal(0, s, 0)),
        at(12, bend(0, 0xA000_0000)),
        at(13, bend(0, CENTRE)),
        at(14, pedal(1, s, DOWN)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    exec.process(
        64,
        &TransportPosition::new(false, Bpm::of(120.0), beat_of(64), None),
        &[],
        &mut [],
    );
    let late: Vec<(u64, Ump)> = heard(&seen)
        .iter()
        .copied()
        .filter(|&(f, _)| f >= 64)
        .collect();
    assert_eq!(late, vec![(64, pedal(1, s, 0))]);
}

/// **A loop wrap releases the pedal after ending the notes it held.** The loop
/// is beat [0, 1): note 60 and the sustain pedal go down at timeline frame
/// 23 850; the block that wraps at its frame 100 (executor frame 200) ends the
/// note, then releases the pedal, on that frame (M1 p. 31; A-5 for the order).
#[test]
fn a_loop_wrap_lifts_the_pedal_after_the_note_off() {
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(23_850, on(60)),
        at(23_851, pedal(0, CCNumber::SUSTAIN, DOWN)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let lp = Some(LoopRange::new(Beat::new(0.0), Beat::new(1.0)).expect("a loop"));
    let looping = |f: u64| TransportPosition::new(true, Bpm::of(120.0), beat_of(f), lp);
    exec.process(100, &looping(23_800), &[], &mut []);
    exec.process(512, &looping(23_900), &[], &mut []);
    let tail: Vec<(u64, Ump)> = heard(&seen).iter().copied().skip(2).collect();
    assert_eq!(
        tail,
        vec![(200, off(60)), (200, pedal(0, CCNumber::SUSTAIN, 0))]
    );
}

/// **A seek releases the pedal and centres the bend where the jump happens,
/// then chases them to where the timeline holds them.** Playback seeks to
/// beat 4 at frame 200 of a 512-frame block, with the pedal down and the
/// bend off centre: both are returned at frame 200, after the note-off; the
/// clip set both before beat 4, so the chase sends them again after the
/// return (the note is not restarted), and the next block, continuing from
/// where playback is, sends nothing.
#[test]
fn a_seek_returns_the_controllers_where_the_jump_happens() {
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(24, on(60)),
        at(25, pedal(0, CCNumber::SUSTAIN, DOWN)),
        at(26, bend(0, 0x7000_0000)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    let mut changes = TransportChanges::NONE;
    changes
        .push(
            Offset::of(Samples::new(200), Samples::new(512)),
            rolling(96_000),
        )
        .unwrap();
    exec.process_with_changes(512, &rolling(0), &changes, &[], &mut []);
    exec.process(512, &rolling(96_312), &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![
            (24, on(60)),
            (25, pedal(0, CCNumber::SUSTAIN, DOWN)),
            (26, bend(0, 0x7000_0000)),
            (200, off(60)),
            (200, pedal(0, CCNumber::SUSTAIN, 0)),
            (200, bend(0, CENTRE)),
            (200, pedal(0, CCNumber::SUSTAIN, DOWN)),
            (200, bend(0, 0x7000_0000)),
        ]
    );
}

/// **A pedal-up the port refuses is owed and goes out after the note-offs.**
/// 400 notes and a sustain pedal are down when a seek ends them into a port
/// with room for 296 offs: the offs the port refuses are paid in the blocks
/// after, and the pedal-up, refused behind them, goes out only after the last
/// of them (A-5: a pedal still down holds the note through its note-off), exactly
/// once.
#[test]
fn a_refused_pedal_up_is_owed_and_follows_the_last_note_off() {
    let note = |i: u16| midi::note_on(MidiChannel::of((i / 128) as u8), (i % 128) as u8, 0xFFFF);
    let s = CCNumber::SUSTAIN;
    let clip = MidiClipNode::from(
        MidiClipConfig::default().events(ClipEvents::from_iter(
            (0..400u16)
                .map(|i| at(if i < 200 { 10 } else { 300 }, note(i)))
                .chain([at(5, pedal(15, s, DOWN))]),
        )),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 256);
    roll(&mut exec, 512, 256);
    for f in [48_000u64, 48_256, 48_512] {
        exec.process(256, &rolling(f), &[], &mut []);
    }
    let seen = heard(&seen);
    let last_off = seen
        .iter()
        .rposition(|(_, u)| key_of(u).is_some_and(|(_, _, on)| !on))
        .unwrap();
    let ups: Vec<usize> = seen
        .iter()
        .enumerate()
        .filter(|&(_, &(_, u))| u == pedal(15, s, 0))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(ups.len(), 1, "the pedal is lifted exactly once");
    assert!(ups[0] > last_off, "after every note-off");
}

// ---------------------------------------------------------------------------
// The chase: playback that starts away from the clip's start hears the
// channel state the clip set before that beat.
// ---------------------------------------------------------------------------

/// **A render that starts mid-clip (a bounce's first block, a start from a
/// locate) first sends, for each state the clip set before its beat, the
/// last value that set it**, by group and channel, each channel's program
/// first, then its registered controllers, its controllers, its pressure
/// and its bend. Notes, per-note controllers and relative controllers are
/// not chased; a state set after the beat is not chased; an event on the
/// start beat itself is played once, by the walk, after the chase.
#[test]
fn a_render_that_starts_mid_clip_starts_from_the_timelines_state() {
    let (g, c0, c1) = (MidiGroup::FIRST, MidiChannel::FIRST, MidiChannel::of(1));
    let volume = |ch, v| midi::cc(Address::new(g, ch), CCNumber::VOLUME, v);
    let range = midi::registered(
        Address::new(g, c0),
        Rpn::PITCH_BEND_SENSITIVITY,
        0x1800_0000,
    );
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(10, midi::program_change(Address::new(g, c0), 5, None)),
        at(20, volume(c0, 0x1000_0000)),
        at(30, on(60)),
        at(
            35,
            Ump::from(Message::midi2(
                Address::new(g, c0),
                Voice2::RegisteredPerNote {
                    note: NoteNumber::of(60),
                    controller: RegisteredPerNoteController::Pitch,
                    value: ControlValue::new(0x4000_0000),
                },
            )),
        ),
        at(40, volume(c0, 0x2000_0000)),
        at(
            45,
            Ump::from(Message::midi2(
                Address::new(g, c0),
                Voice2::relative_registered(Rpn::new(U7::of(0), U7::of(0)), 0x100),
            )),
        ),
        at(50, range),
        at(60, bend(0, 0x7000_0000)),
        at(70, midi::channel_pressure(Address::new(g, c0), 0x3000_0000)),
        at(80, volume(c1, 0x5000_0000)),
        at(96_000, volume(c0, 0x6000_0000)),
        at(100_000, volume(c0, 0x7000_0000)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    exec.process(512, &rolling(96_000), &[], &mut []);
    let got: Vec<Ump> = heard(&seen).iter().map(|&(_, u)| u).collect();
    assert!(
        heard(&seen).iter().all(|&(f, _)| f == 0),
        "all on the first frame"
    );
    assert_eq!(
        got,
        vec![
            midi::program_change(Address::new(g, c0), 5, None),
            range,
            volume(c0, 0x2000_0000),
            midi::channel_pressure(Address::new(g, c0), 0x3000_0000),
            bend(0, 0x7000_0000),
            volume(c1, 0x5000_0000),
            volume(c0, 0x6000_0000),
        ]
    );
}

/// **A render that starts at the clip's start chases nothing**: no state was
/// set before beat 0, so the first block holds only what the walk plays.
#[test]
fn a_render_from_the_start_chases_nothing() {
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(0, bend(0, 0x7000_0000)),
        at(10, on(60)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    exec.process(512, &rolling(0), &[], &mut []);
    assert_eq!(heard(&seen), vec![(0, bend(0, 0x7000_0000)), (10, on(60))]);
}

/// **A chased Program Change selects the bank the timeline holds there**,
/// even when the last one carried none. MIDI 1.0 Bank Select (MSB 1), then
/// Program Change 5, then Program Change 7 with no Bank Select before it:
/// the channel keeps bank 1 (M1 p.13), and a render that starts past both
/// chases program 7 on bank 1, so a receiver last on another bank is moved.
#[test]
fn a_chased_program_carries_the_bank_in_force() {
    let c = MidiChannel::FIRST;
    let clip = MidiClipNode::from(
        MidiClipConfig::default().events(ClipEvents::from_iter([
            at(10, midi::m1_cc(c, CCNumber::BANK_SELECT, U7::of(1))),
            at(
                11,
                parse_exact(MidiGroup::FIRST, &[0xC0, 5])
                    .map(Ump::from)
                    .unwrap(),
            ),
            at(
                20,
                parse_exact(MidiGroup::FIRST, &[0xC0, 7])
                    .map(Ump::from)
                    .unwrap(),
            ),
        ])),
    );
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    exec.process(512, &rolling(96_000), &[], &mut []);
    let programs: Vec<(u8, Option<u8>)> = heard(&seen)
        .iter()
        .filter_map(|&(_, u)| match u.decode() {
            Ok(Message::Midi2(_, Voice2::ProgramChange { program, bank })) => {
                Some((program.get(), bank.map(|b| b.msb.get())))
            }
            _ => None,
        })
        .collect();
    assert_eq!(programs, vec![(7, Some(1))]);
}

/// **The Channel Mode messages are not chased** (CC 120 to 127, M1 p.20):
/// they are commands, and Reset All Controllers re-sent after the chased
/// controllers would undo them. Volume, Reset All Controllers, then Volume
/// again: a render past all three chases the last Volume and nothing else.
#[test]
fn channel_mode_messages_are_not_chased() {
    let (g, c) = (MidiGroup::FIRST, MidiChannel::FIRST);
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter([
        at(
            10,
            midi::cc(Address::new(g, c), CCNumber::VOLUME, 0x1000_0000),
        ),
        at(20, midi::cc(Address::new(g, c), CCNumber::of(121), 0)),
        at(
            30,
            midi::cc(Address::new(g, c), CCNumber::VOLUME, 0x2000_0000),
        ),
        at(40, midi::cc(Address::new(g, c), CCNumber::ALL_SOUND_OFF, 0)),
    ])));
    let (_ed, mut exec, _c, seen) = rig(clip, 512);
    exec.process(512, &rolling(96_000), &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![(
            0,
            midi::cc(Address::new(g, c), CCNumber::VOLUME, 0x2000_0000)
        )]
    );
}

/// **A chase larger than the port is cut short, and what is cut is counted.**
/// Thirty controllers on each of sixteen channels are 480 states; a
/// 256-frame block's port holds 256 events, so a render that starts past
/// them sends the first 256 in chase order (channel 0's first) and the graph
/// is told of the 224 it refused.
#[test]
fn a_chase_larger_than_the_port_is_cut_short_and_counted() {
    let g = MidiGroup::FIRST;
    let clip = MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter(
        (0..16u8).flat_map(|ch| {
            (40..70u8).map(move |n| {
                at(
                    u64::from(ch) * 30 + u64::from(n),
                    midi::cc(
                        Address::new(g, MidiChannel::of(ch)),
                        CCNumber::of(n),
                        0x1000_0000,
                    ),
                )
            })
        }),
    )));
    let (mut ed, mut exec, _c, seen) = rig(clip, 256);
    exec.process(256, &rolling(96_000), &[], &mut []);
    let seen = heard(&seen);
    assert_eq!(seen.len(), 256);
    assert_eq!(
        seen[0].1,
        midi::cc(
            Address::new(g, MidiChannel::FIRST),
            CCNumber::of(40),
            0x1000_0000
        ),
        "channel 0's first"
    );
    let refused: u32 = ed.take_event_losses().map(|l| l.count).sum();
    assert_eq!(refused, 480 - 256);
}

/// **Setting the events the clip already holds cuts nothing.** Note 60 is
/// sounding; the same events (in another order) are set again; no note-off.
#[test]
fn setting_the_same_events_cuts_nothing() {
    let events = [at(10, on(60)), at(1_000, off(60))];
    let (_ed, mut exec, controls, seen) = rig(
        MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter(events))),
        64,
    );
    exec.process(64, &rolling(0), &[], &mut []);
    publish(&controls, [events[1], events[0]]);
    exec.process(64, &rolling(64), &[], &mut []);
    assert_eq!(heard(&seen), vec![(10, on(60))]);
    // A clip that differs does cut it.
    publish(&controls, [at(5_000, on(72))]);
    exec.process(64, &rolling(128), &[], &mut []);
    assert_eq!(heard(&seen), vec![(10, on(60)), (128, off(60))]);
}

/// A program change on the first group and channel.
fn program(n: u8) -> Ump {
    midi::program_change(Address::new(MidiGroup::FIRST, MidiChannel::FIRST), n, None)
}

/// Controller `n` set to `value` on the first group and channel.
fn volume(value: u32) -> Ump {
    midi::cc(
        Address::new(MidiGroup::FIRST, MidiChannel::FIRST),
        CCNumber::VOLUME,
        value,
    )
}

/// `events` as a clip carrying `header`, each message at its own time on the
/// header's clock, a quarter note apart from 0 (the node ignores the times).
fn headed(events: impl IntoIterator<Item = TimedMidiEvent>, header: &[Ump]) -> ClipEvents {
    let events: ClipEvents = events.into_iter().collect();
    events.header(
        header
            .iter()
            .enumerate()
            .map(|(i, &u)| TimedMidiEvent::new(Beat::new(i as f64 * 0.25), u).unwrap()),
    )
}

/// A clip node over an empty clip, with `clip` set before the first block.
fn rig_headed(clip: ClipEvents, max: usize) -> (Editor, Executor, MidiClipControls, Seen) {
    let (ed, exec, controls, seen) = rig(MidiClipNode::from(MidiClipConfig::default()), max);
    controls.set(clip);
    (ed, exec, controls, seen)
}

fn stopped(frame: u64) -> TransportPosition {
    TransportPosition::new(false, Bpm::of(120.0), beat_of(frame), None)
}

fn times(seen: &Seen, ump: Ump) -> usize {
    heard(seen).iter().filter(|&&(_, u)| u == ump).count()
}

/// **A clip's header goes out once on the block the clip starts, in file
/// order, before beat 0's events.** Three header messages and a note on beat
/// 0: the first block has the three on its first frame in the order given,
/// then the note; the following blocks repeat none.
/// Spec: m2-116/6.3#1
#[test]
fn a_header_is_sent_once_at_the_start_in_file_order_before_beat_0() {
    let header = [program(3), volume(0x4000_0000), program(9)];
    let clip = headed([at(0, on(60)), at(20, off(60))], &header);
    let (_ed, mut exec, _c, seen) = rig_headed(clip, 64);
    roll(&mut exec, 192, 64);
    assert_eq!(
        heard(&seen),
        vec![
            (0, header[0]),
            (0, header[1]),
            (0, header[2]),
            (0, on(60)),
            (20, off(60)),
        ]
    );
}

/// **A loop wrap does not send the header again**, inside a block or on a
/// block's edge: the receiver was configured when the run started.
#[test]
fn a_loop_wrap_does_not_send_the_header_again() {
    let lp = Some(LoopRange::new(Beat::new(0.0), Beat::new(1.0)).expect("a loop"));
    let looping = |f: u64| TransportPosition::new(true, Bpm::of(120.0), beat_of(f), lp);
    for (len, wraps) in [(128usize, "inside a block"), (64, "on a block's edge")] {
        let clip = headed([at(10, program(5))], &[program(1)]);
        let (_ed, mut exec, _c, seen) = rig_headed(clip, 128);
        exec.process(len, &looping(0), &[], &mut []);
        exec.process(len, &looping(24_000 - 64), &[], &mut []);
        exec.process(len, &looping(0), &[], &mut []);
        assert_eq!(times(&seen, program(1)), 1, "{wraps}");
        assert!(times(&seen, program(5)) > 1, "the loop did wrap {wraps}");
    }
}

/// **A seek while playing does not send the header again**, only the chase
/// to the new beat.
#[test]
fn a_seek_while_playing_does_not_send_the_header_again() {
    let clip = headed([at(10, program(5)), at(48_010, on(60))], &[program(1)]);
    let (_ed, mut exec, _c, seen) = rig_headed(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    exec.process(64, &rolling(48_000), &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![
            (0, program(1)),
            (10, program(5)),
            (64, program(5)),
            (74, on(60))
        ]
    );
}

/// **A start mid-clip sends the header, then the state the clip held there,
/// then the events**: the header's Program Change 1, the clip's Program Change
/// 5 before the start beat, the note after it. The receiver's last program is
/// the clip's.
#[test]
fn a_start_mid_clip_sends_the_header_then_the_chase_then_the_events() {
    let clip = headed([at(10, program(5)), at(48_010, on(60))], &[program(1)]);
    let (_ed, mut exec, _c, seen) = rig_headed(clip, 64);
    exec.process(64, &rolling(48_000), &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![(0, program(1)), (0, program(5)), (10, on(60))]
    );
}

/// **A start after a stop sends the header again**: once per start, not per
/// block of the run.
#[test]
fn each_start_from_stopped_sends_the_header_once() {
    let clip = headed([at(1_000, on(60))], &[program(1)]);
    let (_ed, mut exec, _c, seen) = rig_headed(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    exec.process(64, &rolling(64), &[], &mut []);
    assert_eq!(times(&seen, program(1)), 1);
    exec.process(64, &stopped(128), &[], &mut []);
    exec.process(64, &stopped(128), &[], &mut []);
    exec.process(64, &rolling(128), &[], &mut []);
    exec.process(64, &rolling(192), &[], &mut []);
    assert_eq!(times(&seen, program(1)), 2);
}

/// **Replacing the clip while it plays sends the new clip's header once**
/// (then the state the new clip holds at the playhead), and not the old one's
/// again.
#[test]
fn replacing_the_clip_while_playing_sends_the_new_header_once() {
    let first = headed([at(10_000, on(60))], &[program(1)]);
    let (_ed, mut exec, controls, seen) = rig_headed(first, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    // The new clip sets program 5 before the playhead, and has another header.
    controls.set(headed(
        [at(5, program(5)), at(10_000, on(62))],
        &[program(2)],
    ));
    exec.process(64, &rolling(64), &[], &mut []);
    exec.process(64, &rolling(128), &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![(0, program(1)), (64, program(2)), (64, program(5))]
    );
}

/// **A clip replaced while stopped sends its header at the next start, once**:
/// the replacement does not send it into a stopped receiver.
#[test]
fn replacing_the_clip_while_stopped_sends_the_header_at_the_start_once() {
    let (_ed, mut exec, controls, seen) = rig_headed(headed([], &[program(1)]), 64);
    exec.process(64, &stopped(0), &[], &mut []);
    controls.set(headed([], &[program(2)]));
    exec.process(64, &stopped(0), &[], &mut []);
    assert_eq!(heard(&seen), vec![]);
    exec.process(64, &rolling(0), &[], &mut []);
    exec.process(64, &rolling(64), &[], &mut []);
    assert_eq!(heard(&seen), vec![(128, program(2))]);
}

/// **A changed receiver gets the header and then the chase, once, while the
/// clip plays**; two calls before a block are one send, and the events go on.
#[test]
fn a_changed_receiver_while_playing_gets_the_header_then_the_chase_once() {
    let clip = headed([at(10, program(5)), at(10_000, on(60))], &[program(1)]);
    let (_ed, mut exec, controls, seen) = rig_headed(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    controls.receiver_changed();
    controls.receiver_changed();
    exec.process(64, &rolling(64), &[], &mut []);
    exec.process(64, &rolling(128), &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![
            (0, program(1)),
            (10, program(5)),
            (64, program(1)),
            (64, program(5)),
        ]
    );
}

/// **A changed receiver while stopped gets its header at the next start, once**
/// (not also when it changed).
#[test]
fn a_changed_receiver_while_stopped_gets_the_header_at_the_next_start_once() {
    let clip = headed([at(10_000, on(60))], &[program(1)]);
    let (_ed, mut exec, controls, seen) = rig_headed(clip, 64);
    exec.process(64, &stopped(0), &[], &mut []);
    controls.receiver_changed();
    exec.process(64, &stopped(0), &[], &mut []);
    assert_eq!(heard(&seen), vec![]);
    exec.process(64, &rolling(0), &[], &mut []);
    exec.process(64, &rolling(64), &[], &mut []);
    assert_eq!(heard(&seen), vec![(128, program(1))]);
}

/// **A header larger than the port's room lands in part, counted**: 300
/// header messages in a 64-frame block, whose port holds 256, put the first
/// 256 on the first frame in file order; the 44 past them are refused and
/// counted with the graph's other event losses, and none is sent later.
#[test]
fn a_header_larger_than_the_port_is_cut_short_and_counted() {
    let header: Vec<Ump> = (0..300u32).map(|n| volume(n << 20)).collect();
    let (mut ed, mut exec, _c, seen) = rig_headed(headed([], &header), 64);
    exec.process(64, &rolling(0), &[], &mut []);
    exec.process(64, &rolling(64), &[], &mut []);
    let got = heard(&seen);
    assert_eq!(got.len(), 256);
    assert!(got.iter().all(|&(f, _)| f == 0));
    assert_eq!(
        got.iter().map(|&(_, u)| u).collect::<Vec<_>>(),
        header[..256]
    );
    let refused: u32 = ed.take_event_losses().map(|l| l.count).sum();
    assert_eq!(refused, 300 - 256);
}

/// **A header is not part of the clip**: its messages are no events, notes or
/// chased states, and a clip differing only in its header is a different clip.
#[test]
fn a_header_is_not_an_event_of_the_clip_and_makes_clips_differ() {
    let plain: ClipEvents = [at(0, on(60)), at(5, off(60))].into_iter().collect();
    let headed = headed([at(0, on(60)), at(5, off(60))], &[program(1)]);
    assert_eq!(headed.len(), plain.len());
    assert_eq!(
        headed.notes().collect::<Vec<_>>(),
        plain.notes().collect::<Vec<_>>()
    );
    assert_ne!(headed, plain);
}

/// A System Exclusive packet of status `status` on the first group.
fn sysex7(status: tutti_ump::SysexStatus) -> Ump {
    Ump::from(Message::Sysex7(
        MidiGroup::FIRST,
        tutti_ump::Sysex7Packet::new(status, &[1, 2]).unwrap(),
    ))
}

/// **A header's System Exclusive goes out whole or not at all, and the
/// header stops at the first message that does not fit.** 254 controllers
/// leave room for 2 of the 256 events of a 64-frame block; a Start, Continue,
/// End run of 3 packets does not fit: none of its packets reaches the port, so
/// no Start is left without its End, and the controller after it (which would
/// fit) is not sent either, so the receiver has a prefix of the header. The 3
/// packets and the 1 controller are counted lost.
#[test]
fn a_header_sysex_that_does_not_fit_is_dropped_whole_and_ends_the_header() {
    use tutti_ump::SysexStatus::{Continue, End, Start};
    let mut header: Vec<Ump> = (0..254u32).map(|n| volume(n << 20)).collect();
    header.extend([
        sysex7(Start),
        sysex7(Continue),
        sysex7(End),
        volume(0x7000_0000),
    ]);
    let (mut ed, mut exec, _c, seen) = rig_headed(headed([], &header), 64);
    exec.process(64, &rolling(0), &[], &mut []);
    let got: Vec<Ump> = heard(&seen).iter().map(|&(_, u)| u).collect();
    assert_eq!(got, header[..254]);
    let refused: u32 = ed.take_event_losses().map(|l| l.count).sum();
    assert_eq!(refused, 4);
}

/// **A header's System Exclusive that fits is sent whole**, and a run with no
/// End is one message to its last packet: a Start and Continue with no End,
/// at a room of 1, are dropped together.
#[test]
fn a_header_sysex_run_without_an_end_is_one_message() {
    use tutti_ump::SysexStatus::{Continue, End, Start};
    let whole = [sysex7(Start), sysex7(End)];
    let (_ed, mut exec, _c, seen) = rig_headed(headed([], &whole), 64);
    exec.process(64, &rolling(0), &[], &mut []);
    assert_eq!(heard(&seen).len(), 2);

    let mut header: Vec<Ump> = (0..255u32).map(|n| volume(n << 20)).collect();
    header.extend([sysex7(Start), sysex7(Continue)]);
    let (mut ed, mut exec, _c, seen) = rig_headed(headed([], &header), 64);
    exec.process(64, &rolling(0), &[], &mut []);
    assert_eq!(heard(&seen).len(), 255);
    let refused: u32 = ed.take_event_losses().map(|l| l.count).sum();
    assert_eq!(refused, 2);
}

/// **A clip's System Exclusive goes out whole or not at all**: 254
/// controllers on frame 0 leave room for 2 of the 256 events of a 64-frame
/// block, so a Start, Continue, End run on frame 0 does not fit. None of its
/// packets reaches the port, so no Start is left without its End, and the 3
/// are counted lost; the controller on frame 1, which fits, is sent: a clip's
/// event that does not fit costs only itself, unlike a header's.
#[test]
fn a_clip_sysex_that_does_not_fit_is_dropped_whole() {
    use tutti_ump::SysexStatus::{Continue, End, Start};
    let mut events: Vec<TimedMidiEvent> = (0..254u32).map(|n| at(0, volume(n << 20))).collect();
    events.extend([sysex7(Start), sysex7(Continue), sysex7(End)].map(|u| at(0, u)));
    events.push(at(1, volume(0x7000_0000)));
    let (mut ed, mut exec, _c, seen) = rig(
        MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter(events.clone()))),
        64,
    );
    exec.process(64, &rolling(0), &[], &mut []);
    let got: Vec<Ump> = heard(&seen).iter().map(|&(_, u)| u).collect();
    let want: Vec<Ump> = events[..254]
        .iter()
        .chain(&events[257..])
        .map(|e| e.event())
        .collect();
    assert_eq!(got, want);
    let refused: u32 = ed.take_event_losses().map(|l| l.count).sum();
    assert_eq!(refused, 3);
}

/// **A clip's System Exclusive run is one event, sent whole at its Start**:
/// a Start on frame 10, a Continue on frame 20 and an End on frame 30 reach
/// the port in order, all on frame 10, and the clip counts them as one event.
#[test]
fn a_clip_sysex_run_is_one_event_sent_at_its_start() {
    use tutti_ump::SysexStatus::{Continue, End, Start};
    let run = [sysex7(Start), sysex7(Continue), sysex7(End)];
    let events = [at(10, run[0]), at(20, run[1]), at(30, run[2])];
    assert_eq!(events.into_iter().collect::<ClipEvents>().len(), 1);
    let (_ed, mut exec, _c, seen) = rig(
        MidiClipNode::from(MidiClipConfig::default().events(ClipEvents::from_iter(events))),
        64,
    );
    exec.process(64, &rolling(0), &[], &mut []);
    assert_eq!(heard(&seen), run.map(|u| (10, u)));
}

/// **A changed receiver does not end the notes the clip holds**: the resend is
/// the header and the chase, with no note-off at the port.
#[test]
fn a_changed_receiver_leaves_the_held_notes_sounding() {
    let clip = headed([at(10, on(60)), at(10_000, off(60))], &[program(1)]);
    let (_ed, mut exec, controls, seen) = rig_headed(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    controls.receiver_changed();
    exec.process(64, &rolling(64), &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![(0, program(1)), (10, on(60)), (64, program(1))]
    );
}

/// **A seek on the block a receiver changed sends the header once**: the jump
/// is the start of the resend, not a second send after it.
#[test]
fn a_seek_in_the_block_a_receiver_changed_sends_the_header_once() {
    let clip = headed([at(10, program(5)), at(48_010, on(60))], &[program(1)]);
    let (_ed, mut exec, controls, seen) = rig_headed(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    controls.receiver_changed();
    exec.process(64, &rolling(48_000), &[], &mut []);
    assert_eq!(
        heard(&seen),
        vec![
            (0, program(1)),
            (10, program(5)),
            (64, program(1)),
            (64, program(5)),
            (74, on(60)),
        ]
    );
}

/// **A start in the middle of a block sends the header on the start's frame**:
/// stopped for 20 frames, rolling from there.
#[test]
fn a_start_inside_a_block_sends_the_header_on_its_frame() {
    let clip = headed([at(30, on(60))], &[program(1)]);
    let (_ed, mut exec, _c, seen) = rig_headed(clip, 64);
    let mut changes = TransportChanges::NONE;
    changes
        .push(Offset::of(Samples::new(20), Samples::new(64)), rolling(0))
        .unwrap();
    exec.process_with_changes(64, &stopped(0), &changes, &[], &mut []);
    assert_eq!(heard(&seen), vec![(20, program(1)), (50, on(60))]);
}

/// **A start that finds notes held ends them before it sends the header and
/// the chase** (a seek on the block a receiver changed is a start with notes
/// sounding): the order at the port is note-off, header, chase.
#[test]
fn a_start_releases_before_the_header_and_the_chase() {
    let clip = headed(
        [at(10, on(60)), at(20, program(5)), at(10_000, off(60))],
        &[program(1)],
    );
    let (_ed, mut exec, controls, seen) = rig_headed(clip, 64);
    exec.process(64, &rolling(0), &[], &mut []);
    controls.receiver_changed();
    exec.process(64, &rolling(48_000), &[], &mut []);
    assert_eq!(
        heard(&seen)[3..],
        [(64, off(60)), (64, program(1)), (64, program(5))]
    );
}

/// **A clip config has no structure: the clip is a published value.** Any two
/// configs make the same node, so a config with other events keeps the running
/// node (the events are a write to it). The config lists no initial value (the
/// clip is no param or setting), and builds a node that plays its events.
#[test]
fn a_clip_config_has_no_structure_and_builds_a_node_playing_its_events() {
    use tutti_graph::{Initials, IntoNode, NodeConfig};
    let one = MidiClipConfig::default().events(ClipEvents::from_iter([at(10, on(60))]));
    let other =
        MidiClipConfig::default().events(ClipEvents::from_iter([at(20, on(62)), at(30, off(62))]));
    assert!(NodeConfig::<f32>::same_structure(&one, &other));
    assert!(NodeConfig::<f32>::same_structure(
        &one,
        &MidiClipConfig::default()
    ));
    assert_eq!(NodeConfig::<f32>::initials(&one), Initials::new());
    let node = NodeConfig::<f32>::build(&other, &HostValues::new());
    let controls = IntoNode::<f32>::into_parts(node).controls;
    assert_eq!(controls.len(), 2);
    assert_eq!(
        *controls.current(),
        ClipEvents::from_iter([at(20, on(62)), at(30, off(62))])
    );
}
