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
    FollowTempo, MidiError, MidiReport, Session, SessionEvent, SetRefusal, ValueErrorKind,
};
use tutti_tempo::{TempoMap, TempoPoint, Transition};
use tutti_ump::{Flex, Message, MidiGroup, Ump};
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
    // 10 ns per quarter note; the tempos used divide 6e9 exactly.
    let field = (6.0e9 / bpm) as u32;
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
    events: Vec<MidiReport>,
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
        let before = s.tempo_map();
        let events: Vec<MidiReport> = s
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
                MidiReport::TempoFollowed { .. } | MidiReport::MeterFollowed { .. }
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

fn followed_tempo(events: &[MidiReport]) -> (Bpm, Beat) {
    events
        .iter()
        .find_map(|e| match e {
            MidiReport::TempoFollowed { tempo, beat } => Some((*tempo, *beat)),
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
    let project = s.tempo_map();
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

    let map = s.tempo_map();
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
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Midi(MidiReport::TempoUnfollowed))),
        "the update reports it"
    );
    let restored = s.tempo_map();
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
    let project = s.tempo_map();
    play(&s);
    blocks(&mut s, 4);
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    for _ in 0..40 {
        blocks(&mut s, 1);
        let events = s.update();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::Midi(MidiReport::TempoFollowed { .. }))),
            "{events:?}"
        );
    }
    assert!(Arc::ptr_eq(&s.tempo_map(), &project));
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

    let map = s.tempo_map();
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
    let project_tempo = s.tempo_map();
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
            MidiReport::MeterFollowed { signature, beat } => Some((*signature, *beat)),
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
        s.get(project_meter).unwrap().changes(),
        [
            MeterChange::new(Beat::new(0.0), TimeSignature::of(4, 4)),
            MeterChange::new(Beat::new(4.0), TimeSignature::of(3, 4)),
        ]
    );
    assert!(
        Arc::ptr_eq(&s.tempo_map(), &project_tempo),
        "a meter change is no tempo change"
    );

    s.midi().unfollow_tempo().expect("was following");
    assert_eq!(*s.get(project_meter).unwrap(), MeterMap::default());
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
            .any(|e| matches!(e, MidiReport::MeterFollowed { .. }))
    );
    assert_eq!(*s.get(meter).unwrap(), MeterMap::default());
}

/// **A Group is a cable of its own**: following Group 1 takes its tempo and not
/// Group 0's, and a second follow replaces the first (the project's map stays the
/// one found by the first call).
#[test]
fn another_group_is_ignored_and_a_second_follow_replaces_the_first() {
    let (mut s, fake) = rig_with(Some(MidiGroup::of(1)));
    let project = s.tempo_map();
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
            .filter(|e| matches!(e, MidiReport::TempoFollowed { .. }))
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
        Arc::ptr_eq(&s.tempo_map(), &project),
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
    let map = s.tempo_map();
    send(&fake, flex_tempo(MidiGroup::FIRST, 75.0));
    for _ in 0..40 {
        blocks(&mut s, 1);
        let events = s.update();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::Midi(MidiReport::TempoFollowed { .. }))),
            "{events:?}"
        );
    }
    assert!(Arc::ptr_eq(&s.tempo_map(), &map));
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

/// **While a live tempo is followed, the app's own tempo is refused**, so the
/// map `unfollow_tempo` restores is the one in force when following began:
/// a write to the `tempo` target is refused with `SetRefusal::Following` and
/// publishes nothing, and it works again after unfollowing.
#[test]
fn the_apps_tempo_is_refused_while_following() {
    let (mut s, _fake) = rig();
    let project = s.tempo_map();
    follow(&mut s, MidiGroup::FIRST, false);
    let refused = crate::tempo::set_tempo(&mut s, Bpm::of(90.0)).unwrap_err();
    assert_eq!(refused.kind, ValueErrorKind::Refused(SetRefusal::Following));
    assert_eq!(refused.at, 0);
    assert!(
        Arc::ptr_eq(&s.tempo_map(), &project),
        "a refusal publishes nothing"
    );
    s.midi().unfollow_tempo().expect("was following");
    assert_eq!(crate::tempo::set_tempo(&mut s, Bpm::of(90.0)), Ok(()));
}

/// **A tempo staged before a follow never publishes over what the unfollow
/// restores**: a write staged and not updated, then a follow and an unfollow
/// with no update between, leaves the project's map in force after the
/// update; and a read while following is not the staged map.
#[test]
fn a_tempo_staged_before_following_is_dropped_not_published_over_the_restored_map() {
    let (mut s, _fake) = rig();
    let project = s.tempo_map();
    let tempo = s.transport_handle().tempo();
    let staged = Arc::new(TempoMap::constant(Bpm::of(90.0)).unwrap());
    s.set(tempo.to(Arc::clone(&staged))).unwrap();

    let input = s.input();
    s.midi()
        .follow_tempo(FollowTempo::new(input, MidiGroup::FIRST))
        .expect("follows");
    assert!(
        !Arc::ptr_eq(&s.get(tempo).unwrap(), &staged),
        "a read while following is not the staged map"
    );
    s.midi().unfollow_tempo().expect("was following");
    let _ = s.update();
    assert!(
        Arc::ptr_eq(&s.tempo_map(), &project),
        "the staged map did not publish over the restored one"
    );
}
