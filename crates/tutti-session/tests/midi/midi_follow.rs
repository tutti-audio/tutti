//! The session follows a live Flex Set Tempo and Set Time Signature a fake device sends the UMP, the
//! session reads it from the input node's tap in `Phase::Poll`, places it at the
//! next block start and publishes the overlay maps, and the engine renders under
//! them.

use crate::common::OpenPorts as _;
use crate::common::{Keyed, blocks, play, tap};
use std::sync::Arc;
use tutti_platform::Instant;

use tutti_midi_device::EndpointId;
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint};
use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_session::{
    FollowKind, FollowTempo, MidiError, MidiEvent, Session, SessionEvent, UnfollowCause, Unplayed,
};
use tutti_tempo::{TempoMap, TempoPoint, Transition};
use tutti_ump::{Flex, FlexTempo, Message, MidiGroup, Ump};
use tutti_units::{Beat, Bpm, SampleRate};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 512;
const KEYS: u64 = 1;

/// A session over a fake OS with one input device, `Keys`, connected and
/// wired.
fn rig_with(groups: Option<MidiGroup>) -> (Keyed, FakeCounters) {
    let (backend, fake) = FakeBackend::build_with(vec![endpoint(KEYS, "Keys")], vec![], usize::MAX);
    let mut builder = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new());
    if let Some(last) = groups {
        builder = builder.midi_groups(last);
    }
    let mut s = builder
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    let _ = s.update();
    let keys = s.midi().devices().remove(0);
    let keys = s.open_input(keys).expect("opens");
    (Keyed::new(s, keys), fake)
}

fn rig() -> (Keyed, FakeCounters) {
    rig_with(None)
}

fn follow(s: &mut Keyed, group: MidiGroup, meter: bool) {
    let input = s.input();
    s.midi()
        .follow_tempo({
            let f = FollowTempo::new(input, group);
            if meter { f.meter() } else { f }
        })
        .expect("follows");
    // The update that commits the tap.
    let _ = s.update();
}

fn flex_tempo(group: MidiGroup, bpm: f64) -> Ump {
    // The spec field, 10 ns units per quarter note, computed apart from `FlexTempo`.
    let field = FlexTempo::new((6.0e9 / bpm) as u32);
    Ump::from(Message::Flex(group, Flex::SetTempo(field)))
}

fn flex_meter(group: MidiGroup, numerator: u8, denominator: u8) -> Ump {
    Ump::from(Message::Flex(
        group,
        Flex::SetTimeSignature(tutti_ump::FlexTimeSignature {
            numerator,
            denominator,
            thirty_seconds: 8,
        }),
    ))
}

fn send(fake: &FakeCounters, ump: Ump) {
    assert!(fake.send_input(EndpointId::from_raw(KEYS), ump, Instant::now()));
}

/// What the update that found a change saw: the playhead of the last block and
/// the tempo map then in force.
struct Found {
    events: Vec<MidiEvent>,
    playhead: tutti_engine::Playhead,
    before: Arc<TempoMap>,
}

/// Renders a block and updates until an update reports a follow event
/// (`TempoFollowed` or `MeterFollowed`), at most `LIMIT` times.
fn until_followed(s: &mut Session) -> Found {
    const LIMIT: usize = 200;
    for _ in 0..LIMIT {
        blocks(s, 1);
        let playhead = s.playhead();
        let before = crate::tempo::effective(s);
        let events: Vec<MidiEvent> = s
            .update()
            .into_iter()
            .filter_map(|e| match e {
                SessionEvent::Midi(e) => Some(e),
                _ => None,
            })
            .collect();
        if events.iter().any(|e| {
            matches!(
                e,
                MidiEvent::TempoFollowed { .. } | MidiEvent::MeterFollowed { .. }
            )
        }) {
            return Found {
                events,
                playhead,
                before,
            };
        }
    }
    panic!("no follow event in {LIMIT} updates");
}

fn followed_tempo(events: &[MidiEvent]) -> (Bpm, Beat) {
    events
        .iter()
        .find_map(|e| match e {
            MidiEvent::TempoFollowed { tempo, beat } => Some((*tempo, *beat)),
            _ => None,
        })
        .expect("a TempoFollowed")
}

/// The beat of the first frame of the block after the one the playhead was read
/// in, from the map's own seconds: the playhead's beat plus one block of time.
fn next_block_beat(found: &Found) -> Beat {
    let p = &found.playhead;
    assert!(p.playing());
    let seconds = found.before.seconds_at(p.beat()).get() + BLOCK as f64 / RATE;
    found
        .before
        .beat_at_seconds(tutti_units::Seconds::new(seconds))
}

fn approx(a: Beat, b: Beat) -> bool {
    (a.get() - b.get()).abs() < 1e-6
}

/// **A Set Tempo from the followed input is a step in the project's map at the
/// next block start**: the published map is the old one up to the beat, then the
/// sent tempo; `TempoFollowed` reports that beat; the engine renders at the new
/// tempo from there; and unfollowing publishes the project's map again.
#[test]
fn a_set_tempo_is_a_step_at_the_next_block_start_and_unfollowing_restores() {
    let (mut s, fake) = rig();
    let project = crate::tempo::effective(&s);
    assert_eq!(project.constant_tempo(), Some(Bpm::of(120.0)));
    follow(&mut s, MidiGroup::FIRST, false);
    play(&s);
    blocks(&mut s, 4);

    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    let found = until_followed(&mut s);

    let (tempo, beat) = followed_tempo(&found.events);
    assert_eq!(tempo, Bpm::of(75.0));
    let expected = next_block_beat(&found);
    assert!(
        approx(beat, expected),
        "placed at {beat:?}, not {expected:?}"
    );
    assert!(
        beat > found.playhead.beat(),
        "ahead of the block the playhead was read in"
    );

    let map = crate::tempo::effective(&s);
    assert_eq!(
        map.points(),
        [
            TempoPoint::step(Beat::new(0.0), Bpm::of(120.0)),
            TempoPoint::step(beat, Bpm::of(75.0)),
        ],
        "the map before the beat is the project's; a step to the sent tempo after"
    );
    assert!(
        Arc::ptr_eq(&project, &found.before),
        "nothing was published before the first change"
    );

    // The engine takes it: after the step's beat the playhead is at 75 bpm.
    blocks(&mut s, 64);
    assert!(s.playhead().beat() > beat);
    assert_eq!(s.playhead().tempo(), Bpm::of(75.0));

    s.midi().unfollow_tempo().expect("was following");
    assert!(
        matches!(s.midi().unfollow_tempo(), Err(MidiError::NotFollowing)),
        "a second unfollow names what was not there"
    );
    let events = s.update();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Midi(MidiEvent::Unfollowed {
                follower: FollowKind::Tempo,
                cause: UnfollowCause::Requested
            })
        )),
        "the update reports it"
    );
    let restored = crate::tempo::effective(&s);
    assert!(Arc::ptr_eq(&restored, &project), "the project's own map");
    let before = s.playhead().beat();
    blocks(&mut s, 4);
    assert_eq!(s.playhead().tempo(), Bpm::of(120.0));
    assert!(s.playhead().beat() > before, "the playhead kept going");
}

/// **Off by default**: with no follow, a Flex tempo changes nothing, and reports
/// nothing.
#[test]
fn without_a_follow_a_flex_tempo_changes_nothing() {
    let (mut s, fake) = rig();
    let project = crate::tempo::effective(&s);
    play(&s);
    blocks(&mut s, 4);
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    for _ in 0..40 {
        blocks(&mut s, 1);
        let events = s.update();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::Midi(MidiEvent::TempoFollowed { .. }))),
            "{events:?}"
        );
    }
    assert!(Arc::ptr_eq(&crate::tempo::effective(&s), &project));
    assert_eq!(s.playhead().tempo(), Bpm::of(120.0));

    // The silence was the follow's: the same message, sent once the session
    // follows, is heard through the same device and input node.
    follow(&mut s, MidiGroup::FIRST, false);
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    let found = until_followed(&mut s);
    assert_eq!(followed_tempo(&found.events).0, Bpm::of(75.0));
}

/// **A step that lands in a project ramp cuts it**: the project's ramp (60 to 180
/// over 8 beats, then a step at 16) is kept up to the beat, reaching the tempo it
/// had there, the sent tempo follows, and the ramp's end and the later point are
/// not in the overlay; every beat before the cut keeps its time.
#[test]
fn a_step_in_a_project_ramp_cuts_the_ramp() {
    let (mut s, fake) = rig();
    let ramp = TempoMap::builder(Bpm::of(60.0))
        .ramp_time_to(Beat::new(8.0), Bpm::of(180.0))
        .step_to(Beat::new(16.0), Bpm::of(90.0))
        .build()
        .unwrap();
    s.set(s.transport_handle().tempo().to(ramp.clone()))
        .expect("not following yet");
    let _ = s.update();
    follow(&mut s, MidiGroup::FIRST, false);
    play(&s);
    blocks(&mut s, 8);

    send(&fake, flex_tempo(MidiGroup::FIRST, 150.0));
    let found = until_followed(&mut s);
    let (_, beat) = followed_tempo(&found.events);
    assert!(
        beat.get() > 0.0 && beat.get() < 8.0,
        "inside the ramp: {beat:?}"
    );
    let expected = next_block_beat(&found);
    assert!(approx(beat, expected), "{beat:?} vs {expected:?}");

    let map = crate::tempo::effective(&s);
    let points = map.points();
    assert_eq!(points.len(), 2, "{points:?}");
    assert_eq!(
        (points[0].beat, points[0].tempo),
        (Beat::new(0.0), Bpm::of(60.0))
    );
    assert_eq!(
        points[1].beat, beat,
        "the ramp ends and the step is at one beat"
    );
    assert_eq!(
        points[0].transition,
        Transition::RampTime {
            end: Some(ramp.tempo_at(beat))
        },
        "the ramp as it stood"
    );
    assert_eq!(points[1].tempo, Bpm::of(150.0));
    assert_eq!(points[1].transition, Transition::Step, "ends on a step");
    for b in [0.0, beat.get() / 2.0, beat.get()] {
        let (a, n) = (
            ramp.seconds_at(Beat::new(b)).get(),
            map.seconds_at(Beat::new(b)).get(),
        );
        assert!((a - n).abs() < 1e-9, "beat {b}: {a} vs {n}");
    }
    blocks(&mut s, 32);
    assert_eq!(
        s.playhead().tempo(),
        Bpm::of(150.0),
        "the engine took the step"
    );
}

/// **A Set Time Signature lands on the next bar line** at or after the beat the
/// transport is at, in the project meter, and unfollowing restores the project's;
/// the tempo map is untouched.
#[test]
fn a_set_time_signature_lands_on_the_next_bar_line() {
    let (mut s, fake) = rig();
    let project_tempo = crate::tempo::effective(&s);
    let project_meter = s.transport_handle().meter();
    assert_eq!(s.get(project_meter).unwrap().changes().len(), 1);
    follow(&mut s, MidiGroup::FIRST, true);
    play(&s);
    blocks(&mut s, 4);

    send(&fake, flex_meter(MidiGroup::FIRST, 3, 2));
    let found = until_followed(&mut s);
    let (signature, beat) = found
        .events
        .iter()
        .find_map(|e| match e {
            MidiEvent::MeterFollowed { signature, beat } => Some((*signature, *beat)),
            _ => None,
        })
        .expect("a MeterFollowed");
    assert_eq!(signature, TimeSignature::of(3, 4));
    assert!(
        found.playhead.beat().get() < 4.0,
        "still in the first bar: {:?}",
        found.playhead.beat()
    );
    assert_eq!(
        beat,
        Beat::new(4.0),
        "the next bar line of 4/4 after beat ~0.1"
    );
    assert_eq!(
        s.get(project_meter.effective()).unwrap().changes(),
        [
            MeterChange::new(Beat::new(0.0), TimeSignature::of(4, 4)),
            MeterChange::new(Beat::new(4.0), TimeSignature::of(3, 4)),
        ]
    );
    assert!(
        Arc::ptr_eq(&crate::tempo::effective(&s), &project_tempo),
        "a meter change is no tempo change"
    );

    s.midi().unfollow_tempo().expect("was following");
    assert_eq!(
        *s.get(project_meter.effective()).unwrap(),
        MeterMap::default()
    );
}

/// **The meter is followed only when asked.**
#[test]
fn the_meter_is_followed_only_when_asked() {
    let (mut s, fake) = rig();
    let meter = s.transport_handle().meter();
    follow(&mut s, MidiGroup::FIRST, false);
    play(&s);
    blocks(&mut s, 4);
    send(&fake, flex_meter(MidiGroup::FIRST, 3, 2));
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    let found = until_followed(&mut s);
    assert!(
        !found
            .events
            .iter()
            .any(|e| matches!(e, MidiEvent::MeterFollowed { .. }))
    );
    assert_eq!(*s.get(meter.effective()).unwrap(), MeterMap::default());
}

/// **A Group is a cable of its own**: following Group 1 takes its tempo and not
/// Group 0's, and a second follow replaces the first (the project's map stays the
/// one found by the first call).
#[test]
fn another_group_is_ignored_and_a_second_follow_replaces_the_first() {
    let (mut s, fake) = rig_with(Some(MidiGroup::of(1)));
    let project = crate::tempo::effective(&s);
    follow(&mut s, MidiGroup::of(1), false);
    play(&s);
    blocks(&mut s, 4);

    send(&fake, flex_tempo(MidiGroup::FIRST, 60.0));
    send(&fake, flex_tempo(MidiGroup::of(1), 75.0));
    let found = until_followed(&mut s);
    assert_eq!(followed_tempo(&found.events).0, Bpm::of(75.0));
    assert_eq!(
        found
            .events
            .iter()
            .filter(|e| matches!(e, MidiEvent::TempoFollowed { .. }))
            .count(),
        1,
        "Group 0's tempo is not followed"
    );

    // A second follow, of Group 0, replaces the first.
    follow(&mut s, MidiGroup::FIRST, false);
    send(&fake, flex_tempo(MidiGroup::of(1), 100.0));
    send(&fake, flex_tempo(MidiGroup::FIRST, 150.0));
    let found = until_followed(&mut s);
    assert_eq!(followed_tempo(&found.events).0, Bpm::of(150.0));

    s.midi().unfollow_tempo().expect("was following");
    assert!(
        matches!(s.midi().unfollow_tempo(), Err(MidiError::NotFollowing)),
        "one follower: one unfollow ends it"
    );
    assert!(
        Arc::ptr_eq(&crate::tempo::effective(&s), &project),
        "the project's map from before the first follow"
    );
}

/// **A repeated tempo grows no map**: the source sends the tempo in force again
/// and the map keeps its two points.
#[test]
fn a_repeated_tempo_adds_no_point() {
    let (mut s, fake) = rig();
    follow(&mut s, MidiGroup::FIRST, false);
    play(&s);
    blocks(&mut s, 4);
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    let _ = until_followed(&mut s);
    let map = crate::tempo::effective(&s);
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    for _ in 0..40 {
        blocks(&mut s, 1);
        let events = s.update();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::Midi(MidiEvent::TempoFollowed { .. }))),
            "{events:?}"
        );
    }
    assert!(Arc::ptr_eq(&crate::tempo::effective(&s), &map));
}

/// **The refusals name what was wrong and change nothing**: a Group the input
/// does not hear, and an unfollow with nothing followed.
#[test]
fn follow_refusals() {
    let (mut s, _fake) = rig();
    let node = s.input();
    assert!(matches!(
        s.midi().follow_tempo(FollowTempo::new(node, MidiGroup::of(3))),
        Err(MidiError::GroupNotHeard(g)) if g == MidiGroup::of(3)
    ));
    let (not_a_port, _) = tap(&mut s);
    assert!(matches!(
        s.midi().follow_tempo(FollowTempo::new(not_a_port, MidiGroup::FIRST)),
        Err(MidiError::NotAnInput(n)) if n == not_a_port
    ));
    assert!(
        matches!(s.midi().unfollow_tempo(), Err(MidiError::NotFollowing)),
        "nothing was followed"
    );
}

/// **A write to the tempo while following is accepted and lands in the base**:
/// `get` reads the map written, and the map the engine uses (`effective`) is
/// the live-step map until the update publishes the rebuilt one.
#[test]
fn a_tempo_set_while_following_lands_in_the_base_and_not_yet_in_force() {
    let (mut s, fake) = rig();
    follow(&mut s, MidiGroup::FIRST, false);
    play(&s);
    blocks(&mut s, 4);
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    let _ = until_followed(&mut s);
    let live = crate::tempo::effective(&s);

    let tempo = s.transport_handle().tempo();
    let written = Arc::new(TempoMap::constant(Bpm::of(90.0)).unwrap());
    assert_eq!(s.set(tempo.to(Arc::clone(&written))), Ok(()));
    assert!(
        Arc::ptr_eq(&s.get(tempo).unwrap(), &written),
        "get reads the base written"
    );
    assert!(
        Arc::ptr_eq(&crate::tempo::effective(&s), &live),
        "the map in force is the live-step map until an update publishes"
    );
}

/// **A live step after a base change keeps the new base's points before the
/// step and drops its points after it.** The base is written while following
/// as three points, 100 bpm from 0, 150 bpm at half the step's beat `b` and 200
/// bpm at `b + 1`; the published map is then 100 bpm, 150 bpm from `b / 2`, and
/// the followed 75 bpm from `b`, with nothing of the 200 bpm point.
#[test]
fn a_live_step_keeps_the_new_bases_points_before_it_and_drops_those_after() {
    let (mut s, fake) = rig();
    follow(&mut s, MidiGroup::FIRST, false);
    play(&s);
    blocks(&mut s, 4);
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    let (_, step) = followed_tempo(&until_followed(&mut s).events);

    let half = Beat::new(step.get() / 2.0);
    let base = TempoMap::builder(Bpm::of(100.0))
        .step_to(half, Bpm::of(150.0))
        .step_to(Beat::new(step.get() + 1.0), Bpm::of(200.0))
        .build()
        .expect("a map");
    s.set(s.transport_handle().tempo().to(base)).unwrap();
    let _ = s.update();

    assert_eq!(
        crate::tempo::effective(&s).points(),
        [
            TempoPoint::step(Beat::new(0.0), Bpm::of(100.0)),
            TempoPoint::step(half, Bpm::of(150.0)),
            TempoPoint::step(step, Bpm::of(75.0)),
        ]
    );
}

/// **After an unfollow the map in force is the base written while following**,
/// not the map the base was when following began.
#[test]
fn unfollowing_publishes_the_base_written_while_following() {
    let (mut s, fake) = rig();
    let first = crate::tempo::effective(&s);
    follow(&mut s, MidiGroup::FIRST, false);
    play(&s);
    blocks(&mut s, 4);
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    let _ = until_followed(&mut s);
    assert_eq!(crate::tempo::set_tempo(&mut s, Bpm::of(90.0)), Ok(()));

    s.midi().unfollow_tempo().expect("was following");
    let _ = s.update();
    let in_force = crate::tempo::effective(&s);
    assert!(
        !Arc::ptr_eq(&in_force, &first),
        "not the map found at follow"
    );
    assert!(Arc::ptr_eq(
        &in_force,
        &s.get(s.transport_handle().tempo()).unwrap()
    ));
    assert_eq!(in_force.constant_tempo(), Some(Bpm::of(90.0)));
}

/// **A Set Time Signature lands on the next bar line of the base in force
/// when the base changes.** Asked for at a beat inside the first bar, over a 4/4
/// base it lands at beat 4; the base then written is 5/4, and the published
/// meter is 5/4 from 0 and 3/4 from beat 5, the next bar line of 5/4.
#[test]
fn a_meter_step_lands_on_the_next_bar_line_over_a_changed_base() {
    let (mut s, fake) = rig();
    follow(&mut s, MidiGroup::FIRST, true);
    play(&s);
    blocks(&mut s, 4);
    send(&fake, flex_meter(MidiGroup::FIRST, 3, 2));
    let found = until_followed(&mut s);
    assert!(found.playhead.beat().get() < 4.0, "inside the first bar");

    let meter = s.transport_handle().meter();
    s.set(meter.to(MeterMap::new([MeterChange::new(
        Beat::new(0.0),
        TimeSignature::of(5, 4),
    )])))
    .unwrap();
    let _ = s.update();
    assert_eq!(
        s.get(meter.effective()).unwrap().changes(),
        [
            MeterChange::new(Beat::new(0.0), TimeSignature::of(5, 4)),
            MeterChange::new(Beat::new(5.0), TimeSignature::of(3, 4)),
        ]
    );
}

/// **A bounce made while following renders the base tempo.** The base is 120
/// bpm; the followed 75 bpm is in force from the next block's beat. Four beats
/// at the base are two seconds, 96 000 frames at 48 kHz; at the followed map
/// they would be longer.
#[cfg(feature = "export")]
#[test]
fn a_bounce_while_following_renders_the_base_tempo() {
    use tutti_session::{Bounce, Range};
    let (mut s, fake) = rig();
    follow(&mut s, MidiGroup::FIRST, false);
    play(&s);
    blocks(&mut s, 4);
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    let _ = until_followed(&mut s);
    assert!(
        crate::tempo::effective(&s).constant_tempo().is_none(),
        "the followed map is in force"
    );

    let bounce = Bounce::new()
        .main_output_buffers()
        .range(Range::Beats(Beat::new(0.0), Beat::new(4.0)));
    let planes = crate::bounced::bounced(&mut s, bounce)
        .expect("renders")
        .into_main_output()
        .into_planes();
    assert_eq!(planes[0].len(), 2 * RATE as usize);
}

/// **An SMPTE file loaded while following is placed by the base tempo.** Tick
/// 1000 at `-25` frames and 40 ticks a frame is 1.0 s: beat 2.0 on the base's
/// 120 bpm, not the beat the followed 75 bpm map (a step from beat ~0.1)
/// gives.
#[test]
fn an_smpte_load_while_following_places_notes_by_the_base() {
    let (mut s, fake) = rig();
    follow(&mut s, MidiGroup::FIRST, false);
    play(&s);
    blocks(&mut s, 4);
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    let _ = until_followed(&mut s);

    // Header: format 0, one track, division -25 fps and 40 ticks a frame; the
    // track is delta 1000, the F7 escape of one byte, end of track.
    let mut bytes = b"MThd\0\0\0\x06\0\0\0\x01\xE7\x28MTrk\0\0\0\x09".to_vec();
    bytes.extend([0x87, 0x68, 0xF7, 1, 0x55, 0x00, 0xFF, 0x2F, 0x00]);
    let loaded = tutti_fixtures::block_on(s.midi().load_midi(bytes)).expect("loads");
    let Unplayed::Smf { beat, .. } = &loaded.report().unplayed[0] else {
        panic!("a Standard MIDI File's event");
    };
    assert_eq!(*beat, Beat::new(2.0));
}
