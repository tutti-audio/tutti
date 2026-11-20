//! MIDI in the session: clips, live inputs, the hardware
//! input's routes and the outputs, on an offline session the test
//! renders. A [`Tap`] stands in for a synth: it logs every MIDI event its
//! event input receives with its absolute frame.

#[cfg(feature = "midi-hardware")]
use crate::common::with_input;
use crate::common::{MF, Seen, blocks, note, offline, play, tap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_session::Stamp;

#[cfg(feature = "midi-hardware")]
use std::time::Duration;
use tutti_graph::Losses;
#[cfg(feature = "midi-hardware")]
use tutti_graph::NodeKey;
#[cfg(feature = "midi-hardware")]
use tutti_midi_device::{EndpointId, PortDirection};
#[cfg(feature = "midi-hardware")]
use tutti_midi_runtime::{CHANNELLESS_PORT, MIDI_INPUT_PORTS};
use tutti_midi_runtime::{LIVE_INLET_CAPACITY, TimedMidiEvent};
#[cfg(feature = "midi-hardware")]
use tutti_platform::Instant;
use tutti_rt::PushError;
#[cfg(feature = "midi-hardware")]
use tutti_ump::MidiGroup;
#[cfg(feature = "midi-hardware")]
use tutti_ump::Ump;
use tutti_ump::{MidiChannel, NoteKeyEvent, NoteNumber, NoteVelocity};
#[cfg(feature = "midi-hardware")]
use tutti_ump::{MpeChannels, MpeZoneSpan};
use tutti_units::{Beat, BeatDuration, Frame, SampleRate};

use tutti_fixtures::midi;
use tutti_session::{
    EventInPort, EventOutPort, Feeder, MidiError, Session, SessionEvent, midi_sink,
};
#[cfg(feature = "midi-hardware")]
use tutti_session::{MidiReport, PHASES, PhaseCx};
#[cfg(feature = "midi-hardware")]
use tutti_session::{MidiRouteRule, RouteDeclarer};
#[cfg(feature = "midi-hardware")]
use tutti_session::{Peer as _, Resource as _};
use tutti_ump::{Message, Voice2};

/// Every phase at `now`: one update, at a time the test picks.
#[cfg(feature = "midi-hardware")]
pub(crate) fn update_at(s: &mut Session, now: Instant) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, PhaseCx::at(now), &mut out);
    }
    out
}

#[cfg(feature = "midi-hardware")]
const RATE: f64 = 48_000.0;

/// Plays the transport from beat 0 and renders `frames` frames in 512-frame
/// blocks.
fn roll(s: &Session, frames: usize) {
    play(s);
    blocks(s, frames.div_ceil(512));
}

fn note_ons(seen: &Seen) -> Vec<(u64, u8)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|&(f, e)| match e.note_key_event() {
            Some(NoteKeyEvent::On(key)) => Some((f, key.note().get())),
            _ => None,
        })
        .collect()
}

/// The nodes the last plan sent holds.
fn plan_nodes(s: &Session) -> usize {
    s.graph_ref()
        .testing()
        .sent_plan()
        .expect("a plan")
        .order()
        .len()
}

/// **A clip lands its note on its frame**: a third of a beat in at 120 BPM
/// / 48 kHz is frame 8 000, inside a block.
#[test]
fn a_clip_plays_its_note_on_its_frame() {
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .play_clip(
            synth,
            Beat::new(0.0),
            note(60, MF, Beat::new(1.0 / 3.0), BeatDuration::new(1.0)),
        )
        .expect("the tap takes MIDI");
    let _ = s.update();
    roll(&s, 9_000);
    assert_eq!(note_ons(&seen), vec![(8_000, 60)]);
}

/// **A clip edit updates in place**: a second `play_clip` on one target
/// replaces its clip's events, and the graph's node count stays the same.
/// `stop_clip` empties it and keeps it.
#[test]
fn a_clip_edit_updates_in_place() {
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .play_clip(
            synth,
            Beat::new(0.0),
            note(60, MF, Beat::new(0.0), BeatDuration::new(1.0)),
        )
        .unwrap();
    let clip = s.midi().clip(synth).expect("the target's clip");
    let _ = s.update();
    let nodes = plan_nodes(&s);

    s.midi()
        .play_clip(
            synth,
            Beat::new(0.0),
            note(64, MF, Beat::new(1.0 / 3.0), BeatDuration::new(1.0)),
        )
        .unwrap();
    let _ = s.update();
    assert_eq!(s.midi().clip(synth), Some(clip), "the same clip");
    assert_eq!(plan_nodes(&s), nodes, "and no node more");
    roll(&s, 9_000);
    assert_eq!(note_ons(&seen), vec![(8_000, 64)], "playing the new events");

    s.midi().stop_clip(synth).expect("the target has a clip");
    let _ = s.update();
    assert_eq!(s.midi().clip(synth), Some(clip), "the emptied clip stays");
    assert!(s.graph_ref().contains(clip));
}

/// **A clip's events land `at` beats later**: a third of a beat into the
/// clip, started at beat 1, is a third of a beat past beat 1, frame 32 000 at
/// 120 BPM / 48 kHz.
#[test]
fn a_clip_starts_at_the_beat_it_is_given() {
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    s.midi()
        .play_clip(
            synth,
            Beat::new(1.0),
            note(60, MF, Beat::new(1.0 / 3.0), BeatDuration::new(1.0)),
        )
        .expect("the tap takes MIDI");
    let _ = s.update();
    roll(&s, 33_000);
    assert_eq!(note_ons(&seen), vec![(32_000, 60)]);
}

/// **A start that is not a finite beat is refused, and nothing is made.**
#[test]
fn a_clip_start_that_is_not_finite_is_refused() {
    let mut s = offline();
    let (synth, _) = tap(&mut s);
    let nan = Beat::new(0.0 / std::hint::black_box(0.0));
    for bad in [nan, Beat::new(f64::INFINITY)] {
        assert!(matches!(
            s.midi().play_clip(
                synth,
                bad,
                note(60, MF, Beat::new(0.0), BeatDuration::new(1.0))
            ),
            Err(MidiError::BeatNotFinite(_))
        ));
    }
    assert_eq!(s.midi().clip(synth), None, "no clip was made");
}

/// **Stopping a clip a target does not have names the target.**
#[test]
fn stopping_a_clip_that_is_not_there_names_the_target() {
    let mut s = offline();
    let (synth, _) = tap(&mut s);
    assert!(matches!(
        s.midi().stop_clip(synth),
        Err(MidiError::NoClip(n)) if n == synth
    ));
}

/// **A loaded track plays as a clip on the transport's timeline**: a note on
/// the file's beat 2 sounds at 48 000 frames at 120 beats a minute.
#[test]
fn a_loaded_track_plays_its_events_at_their_beats() {
    use tutti_midi_file::SmfDivision;
    use tutti_midi_file::{Layout, LeadTrack, MidiFile, Tick, Track};
    use tutti_ump::U7;
    let track = Track::new().note(
        Tick(960),
        Tick(1200),
        MidiChannel::FIRST,
        NoteNumber::of(62),
        U7::of(100),
    );
    let file = MidiFile::smf(
        SmfDivision::per_quarter(480),
        Layout::Format0(LeadTrack::new(track)),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("one.mid");
    file.write(&path).unwrap();
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    let job = s.midi().load_midi(path);
    let loaded = job.wait(&mut s).unwrap();
    s.midi()
        .play_track(synth, loaded.track(0).unwrap())
        .unwrap();
    let _ = s.update();
    roll(&s, 49_000);
    assert_eq!(note_ons(&seen), vec![(48_000, 62)]);
}

/// **A target that takes no MIDI is refused**: a node with no event input,
/// and one not in the graph.
#[test]
fn a_target_without_an_event_input_takes_no_clip() {
    let mut s = offline();
    let (dc, _) = s
        .graph()
        .add(tutti_graph::ForkByClone(tutti_nodes::testing::Const::mono(
            0.5,
        )));
    assert!(matches!(
        s.midi().play_clip(dc, Beat::new(0.0), std::iter::empty::<TimedMidiEvent>()),
        Err(MidiError::NoEventInput(n)) if n == dc.erase()
    ));
    s.remove(dc).unwrap();
    assert!(matches!(
        s.midi().input(dc),
        Err(MidiError::NoEventInput(_))
    ));
}

/// **A clip and a live input survive their target's re-bind** (a past
/// regression, at the session): the target is removed, a new node takes
/// its place, and the host re-aims both at it. The same clip and queue
/// nodes play the new node, and the removed one hears nothing.
#[test]
fn a_clip_and_a_live_input_survive_a_re_bind() {
    let mut s = offline();
    let (old, old_seen) = tap(&mut s);
    s.midi()
        .play_clip(
            old,
            Beat::new(0.0),
            note(60, MF, Beat::new(1.0 / 3.0), BeatDuration::new(1.0)),
        )
        .unwrap();
    let clip = s.midi().clip(old).expect("the target's clip");
    let mut live = s.midi().input(old).unwrap();
    let inlet = s.graph_ref().feed(old, Feeder::LiveInput)[0].node();
    let _ = s.update();

    s.remove(old).unwrap();
    let (new, new_seen) = tap(&mut s);
    s.midi().retarget(clip, new).expect("the clip re-aims");
    s.midi()
        .retarget_input(old, new)
        .expect("the queue re-aims");
    let _ = s.update();
    assert_eq!(s.midi().clip(new), Some(clip), "the same clip");
    assert_eq!(
        s.graph_ref().feed(new, Feeder::LiveInput)[0].node(),
        inlet,
        "the same inlet"
    );
    assert!(
        matches!(s.midi().input(new), Err(MidiError::InputTaken(n)) if n == new),
        "its keys were handed out once"
    );
    let mut fed = s.graph_ref().sources(EventInPort::new(new, 0)).unwrap();
    fed.sort_by_key(|e| e.node());
    // The clip feeds the target through its MPE transform.
    let transform = s.graph_ref().feed(new, Feeder::Sequencer)[0];
    let mut want = vec![transform, EventOutPort::new(inlet, 0)];
    want.sort_by_key(|e| e.node());
    assert_eq!(fed, want);

    live.push(Stamp::NextBlock, midi::note_on(MidiChannel::FIRST, 72, MF))
        .unwrap();
    roll(&s, 9_000);
    let heard = note_ons(&new_seen);
    assert!(heard.contains(&(8_000, 60)), "the clip plays it: {heard:?}");
    assert!(
        heard.iter().any(|&(_, n)| n == 72),
        "the keyboard plays it: {heard:?}"
    );
    assert!(
        note_ons(&old_seen).is_empty(),
        "the removed node hears nothing"
    );
}

/// **A live input's keys are the inlet's one producer and send in order**:
/// the key helpers build group-1 MIDI 2.0 notes, a frame is held until its
/// block, a full ring says so, and a second `input` for the target is refused
/// rather than a second writer on the ring.
#[test]
fn a_live_inputs_keys_send_in_order_and_are_handed_out_once() {
    let mut s = offline();
    let (target, seen) = tap(&mut s);
    let mut keys = s.midi().input(target).unwrap();
    assert!(
        matches!(s.midi().input(target), Err(MidiError::InputTaken(n)) if n == target),
        "one producer"
    );
    let _ = s.update();

    let on = |keys: &mut tutti_midi_runtime::MidiKeys, at: Stamp, n: u8| {
        keys.note_on(
            at,
            MidiChannel::FIRST,
            NoteNumber::of(n),
            NoteVelocity::new(MF),
        )
    };
    on(&mut keys, Stamp::NextBlock, 60).unwrap();
    on(&mut keys, Stamp::NextBlock, 62).unwrap();
    on(&mut keys, Frame::new(3_000).into(), 64).unwrap();
    roll(&s, 4_000);
    let heard = note_ons(&seen);
    assert_eq!(
        heard.iter().map(|&(_, n)| n).collect::<Vec<_>>(),
        [60, 62, 64]
    );
    assert!(heard[0].0 < 512 && heard[1].0 < 512, "{heard:?}");
    assert_eq!(heard[2].0, 3_000);

    for _ in 0..LIVE_INLET_CAPACITY.slots() {
        keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
            .unwrap();
    }
    assert_eq!(
        keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60)),
        Err(PushError::Full)
    );
}

/// **A route rule feeds its targets the hardware input's ports**: an
/// any-channel rule all of them, a channel rule its channel's and the
/// channelless one; the fallback the ports no rule covers; unrouting
/// unwires.
#[cfg(feature = "midi-hardware")]
#[test]
fn route_rules_wire_the_hardware_input() {
    let (mut s, input) = with_input(None);
    let (a, _) = tap(&mut s);
    let (b, _) = tap(&mut s);
    let ports = |s: &Session, n: NodeKey| -> Vec<u16> {
        let mut p: Vec<u16> = s
            .graph_ref()
            .feed(n, Feeder::Routes)
            .iter()
            .map(|e| {
                assert_eq!(e.node(), input);
                u16::try_from(e.index()).unwrap()
            })
            .collect();
        p.sort();
        p
    };

    let all = s.midi().route(MidiRouteRule::any_channel().to(a)).unwrap();
    assert_eq!(
        ports(&s, a),
        (0..MIDI_INPUT_PORTS as u16).collect::<Vec<_>>()
    );

    let ch2 = MidiChannel::of(2);
    s.midi()
        .route(MidiRouteRule::for_channel(ch2).to(b))
        .unwrap();
    assert_eq!(ports(&s, b), vec![2, CHANNELLESS_PORT as u16]);

    s.midi().unroute(all).unwrap();
    assert!(
        matches!(
            s.midi().unroute(all),
            Err(MidiError::NoSuchRoute(id)) if id == all
        ),
        "a removed rule is not there"
    );
    assert_eq!(ports(&s, a), Vec::<u16>::new(), "unrouted");

    s.midi().set_route_fallback(Some(a)).unwrap();
    let rest: Vec<u16> = (0..MIDI_INPUT_PORTS as u16)
        .filter(|&p| p != 2 && p != CHANNELLESS_PORT as u16)
        .collect();
    assert_eq!(ports(&s, a), rest, "the fallback takes what no rule covers");
}

/// **A route rule names a Group, and the engine's input hears the Groups the
/// builder asks for** (M2-104 Appendix B.1): channel 2 of Group 2 reaches its
/// own listener through Group 2's ports, not Group 1's channel 2, and a rule
/// for a Group the input does not hear wires nothing.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_route_rule_names_a_group() {
    let (mut s, input) = with_input(Some(MidiGroup::of(1)));
    let (a, _) = tap(&mut s);
    let (b, _) = tap(&mut s);
    let (c, _) = tap(&mut s);
    let ports = |s: &Session, n: NodeKey| -> Vec<u16> {
        let mut p: Vec<u16> = s
            .graph_ref()
            .feed(n, Feeder::Routes)
            .iter()
            .map(|e| {
                assert_eq!(e.node(), input);
                u16::try_from(e.index()).unwrap()
            })
            .collect();
        p.sort();
        p
    };
    let ch2 = MidiChannel::of(2);
    let group = MIDI_INPUT_PORTS as u16;
    s.midi()
        .route(
            MidiRouteRule::for_channel(ch2)
                .in_group(MidiGroup::of(1))
                .to(a),
        )
        .unwrap();
    assert_eq!(
        ports(&s, a),
        vec![group + 2, group + CHANNELLESS_PORT as u16]
    );
    s.midi()
        .route(MidiRouteRule::for_channel(ch2).to(b))
        .unwrap();
    assert_eq!(ports(&s, b), vec![2, CHANNELLESS_PORT as u16]);
    s.midi()
        .route(
            MidiRouteRule::for_channel(ch2)
                .in_group(MidiGroup::of(2))
                .to(c),
        )
        .unwrap();
    assert_eq!(ports(&s, c), Vec::<u16>::new(), "a Group not heard");
}

/// **The routes have one declarer.** While the session's own rules are
/// declared, a host's `set_routes` is refused and changes nothing; once
/// they are unrouted it applies, and then the session's own `route` and
/// `set_route_fallback` are refused, until a `set_routes` with no rule and
/// no fallback hands the routes back.
#[cfg(feature = "midi-hardware")]
#[test]
fn the_routes_have_one_declarer() {
    let (mut s, input) = with_input(None);
    let (a, _) = tap(&mut s);
    let (b, _) = tap(&mut s);
    let fed = |s: &Session, n: NodeKey| !s.graph_ref().feed(n, Feeder::Routes).is_empty();

    let own = s.midi().route(MidiRouteRule::any_channel().to(a)).unwrap();
    let host = [MidiRouteRule::<u8>::any_channel().to(1)];
    let resolve = |k: u8| (k == 1).then_some(b);
    assert!(matches!(
        s.midi().set_routes(input, &host, None, resolve),
        Err(MidiError::TwoRouteDeclarers(RouteDeclarer::Session))
    ));
    assert!(fed(&s, a) && !fed(&s, b), "the refusal changed nothing");

    s.midi().unroute(own).unwrap();
    s.midi().set_routes(input, &host, None, resolve).unwrap();
    assert!(fed(&s, b), "the host's routes apply");
    assert!(matches!(
        s.midi().route(MidiRouteRule::any_channel().to(a)),
        Err(MidiError::TwoRouteDeclarers(RouteDeclarer::Host))
    ));
    assert!(matches!(
        s.midi().set_route_fallback(Some(a)),
        Err(MidiError::TwoRouteDeclarers(RouteDeclarer::Host))
    ));
    assert!(!fed(&s, a) && fed(&s, b), "the refusals changed nothing");

    s.midi()
        .set_routes(input, &[] as &[MidiRouteRule<u8>], None, resolve)
        .unwrap();
    assert!(!fed(&s, b), "handed back: nothing routed");
    s.midi().route(MidiRouteRule::any_channel().to(a)).unwrap();
    assert!(fed(&s, a), "the session declares again");
}

/// Counts what reached the wire.
struct CountingSink(Arc<AtomicUsize>);
impl tutti_midi_runtime::MidiOut for CountingSink {
    fn queue_timed(&self, batch: &[tutti_midi_runtime::Outgoing]) -> tutti_midi_runtime::Taken {
        let events: Vec<tutti_graph::UmpAt> = batch.iter().map(|e| (e.at, e.ump)).collect();
        self.0.fetch_add(events.len(), Ordering::SeqCst);
        tutti_midi_runtime::Taken::within(batch, batch.len())
    }
}

/// A sink that takes nothing: every event it is handed is refused.
struct Refusing;
impl tutti_midi_runtime::MidiOut for Refusing {
    fn queue_timed(&self, batch: &[tutti_midi_runtime::Outgoing]) -> tutti_midi_runtime::Taken {
        tutti_midi_runtime::Taken::within(batch, 0)
    }
}

/// The builder's clock, enabled, rolling for half a second into the
/// `midi_sink` entry declared on `sink`: its Beat Clock reaches the sink's node.
fn clocked(sink: Arc<dyn tutti_midi_runtime::MidiOut>) -> Session {
    let mut s = offline();
    s.pool(midi_sink("out", sink)).expect("declares");
    let _ = s.update(); // the sink's node, loaded and committed
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    roll(&s, 24_000);
    s
}

/// **A sink is drained to with stamping off**: the `MidiOut` phase hands the
/// sink the clock's ticks whether or not it JR-stamps (stamping gates
/// timestamps, not delivery), and none is counted lost.
#[test]
fn a_sink_is_drained_to_stamping_off() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut s = clocked(Arc::new(CountingSink(Arc::clone(&count))));
    let _ = s.update();
    assert!(
        count.load(Ordering::SeqCst) > 0,
        "the clock reached the sink, unstamped"
    );
    assert_eq!(s.midi_sink(&"out").unwrap().losses(), Losses::NONE);
}

/// **A sink that refuses events has them counted in its own losses.**
#[test]
fn a_refusing_sink_counts_what_it_refused() {
    let mut s = clocked(Arc::new(Refusing));
    let _ = s.update();
    let losses = s.midi_sink(&"out").unwrap().losses();
    assert!(
        losses.refused > 0,
        "the ticks are refused, counted: {losses:?}"
    );
    assert_eq!(losses.lost, 0, "the tap had room for them");
    let before = losses.refused;
    roll(&s, 24_000);
    let _ = s.update();
    assert!(
        s.midi_sink(&"out").unwrap().losses().refused > before,
        "and the count only rises"
    );
}

/// **The session's own MIDI nodes go in through `add`**, so the reclaim
/// phase sweeps their published cells: a clip `play_clip` made (its events)
/// and an input port's input node (its MPE mode). What each reclaimer frees is
/// pinned in `tutti-midi-runtime` with a held loan, and the sweep of the
/// graph's reclaimers in `tests/wiring/graph.rs`.
#[cfg(feature = "midi-hardware")]
#[test]
fn the_sessions_midi_nodes_keep_their_reclaimers() {
    let (mut s, input) = with_input(None);
    let (synth, _) = tap(&mut s);
    s.midi()
        .play_clip(
            synth,
            Beat::new(0.0),
            note(60, MF, Beat::new(0.0), BeatDuration::new(1.0)),
        )
        .expect("the tap takes MIDI");
    let clip = s.midi().clip(synth).expect("the target's clip");
    assert!(s.graph_ref().testing().keeps_reclaimer(clip), "the clip's");
    assert!(
        s.graph_ref().testing().keeps_reclaimer(input),
        "the input node's"
    );
    assert!(
        !s.graph_ref().testing().keeps_reclaimer(synth),
        "a tap publishes nothing"
    );
}

/// A session over a fake OS with no devices, and the handle that plugs some.
#[cfg(feature = "midi-hardware")]
fn hot_plug_session() -> (Session, tutti_midi_hardware::test_support::FakeCounters) {
    let (backend, fake) =
        tutti_midi_hardware::test_support::FakeBackend::build_with(vec![], vec![], usize::MAX);
    let s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    (s, fake)
}

/// As [`hot_plug_session`], with MIDI-CI under `identity` from the start.
#[cfg(feature = "midi-hardware")]
pub(crate) fn hot_plug_session_ci(
    identity: tutti_ump::DeviceIdentity,
) -> (Session, tutti_midi_hardware::test_support::FakeCounters) {
    let (mut s, fake) = hot_plug_session();
    present(&mut s, identity);
    (s, fake)
}

/// The policy that presents `identity` on every MIDI device, named "Session".
#[cfg(feature = "midi-hardware")]
fn present(s: &mut Session, identity: tutti_ump::DeviceIdentity) {
    s.set(
        s.policy::<tutti_session::MidiPolicy>()
            .to(tutti_session::MidiPolicy {
                identity: Some(tutti_midi_device::MidiIdentity {
                    device: identity,
                    name: tutti_ump::EndpointName::new("Session").unwrap(),
                    instance: None,
                }),
            }),
    )
    .expect("the session has an OS MIDI layer");
}

/// The device events of an update, as `(kind, device name)`.
#[cfg(feature = "midi-hardware")]
fn device_events(events: &[SessionEvent]) -> Vec<(&'static str, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Midi(MidiReport::Added { info }) => Some(("added", info.name.clone())),
            SessionEvent::Midi(MidiReport::Removed { info }) => {
                Some(("removed", info.name.clone()))
            }
            SessionEvent::Midi(MidiReport::Changed { info }) => {
                Some(("changed", info.name.clone()))
            }
            _ => None,
        })
        .collect()
}

#[cfg(feature = "midi-hardware")]
pub(crate) fn plug(
    fake: &tutti_midi_hardware::test_support::FakeCounters,
    raw: u64,
    name: &str,
    d: PortDirection,
) {
    fake.plug(tutti_midi_hardware::test_support::endpoint(raw, name), d);
}

/// **A device that comes and goes reaches `Session::update` as an event**,
/// in the order the OS reported, on a session with no hardware and a fake OS.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_device_that_comes_and_goes_is_a_session_event() {
    let (mut s, fake) = hot_plug_session();
    assert!(device_events(&s.update()).is_empty(), "nothing yet");

    plug(&fake, 1, "Keys", PortDirection::Input);
    assert_eq!(device_events(&s.update()), [("added", "Keys".to_string())]);
    assert!(device_events(&s.update()).is_empty(), "reported once");

    fake.unplug(EndpointId::from_raw(1));
    assert_eq!(
        device_events(&s.update()),
        [("removed", "Keys".to_string())],
        "unheld, so nothing is retired"
    );
}

/// A session over a fake OS with the device `Keys` (id 1) and `Pads` (id 2)
/// present, and a stand-in synth that every channel of every input port
/// routes to. The devices are told for certain across a replug.
#[cfg(feature = "midi-hardware")]
fn keyboards() -> (
    Session,
    tutti_midi_hardware::test_support::FakeCounters,
    Seen,
) {
    let (backend, fake) = tutti_midi_hardware::test_support::FakeBackend::build_with(
        vec![
            tutti_midi_hardware::test_support::persistent_endpoint(1, "Keys"),
            tutti_midi_hardware::test_support::persistent_endpoint(2, "Pads"),
        ],
        vec![],
        usize::MAX,
    );
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    let _ = s.update();
    (s, fake, seen)
}

/// Declares an input port for each of `names` (a device's name, which is also
/// its key), whole, and updates: the entries load and their input nodes and
/// inlets are in the plan.
#[cfg(feature = "midi-hardware")]
pub(crate) fn connect(s: &mut Session, names: &[&'static str]) {
    let list = s.midi().devices();
    let ports: Vec<_> = names
        .iter()
        .map(|&name| {
            let device = list
                .iter()
                .find(|d| d.name.eq_ignore_ascii_case(name))
                .unwrap_or_else(|| panic!("{name} is listed"));
            tutti_session::midi_input(name, device.clone())
        })
        .collect();
    s.pool(ports).expect("declares");
    let _ = s.update();
}

/// A note-on on channel `channel`.
#[cfg(feature = "midi-hardware")]
fn key(channel: u8, n: u8) -> Ump {
    midi::note_on(MidiChannel::of(channel), n, MF)
}

/// **A connected device's note reaches the synth its channel routes to**
/// through the device's own event inlet, once: the update after the connection
/// wires the inlet to the input node, the device's thread
/// pushes, and the synth hears it.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_connected_devices_note_reaches_the_synth_through_its_inlet() {
    let (mut s, fake, seen) = keyboards();
    connect(&mut s, &["keys"]);
    roll(&s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(2, 60), Instant::now()));
    roll(&s, 8_192);
    let heard = note_ons(&seen);
    assert_eq!(heard.len(), 1, "once: {heard:?}");
    assert_eq!(heard[0].1, 60);
}

/// **Two devices are two ports, merged at the synth**: each device's note is
/// heard, and one port going takes only its own input node and inlet.
#[cfg(feature = "midi-hardware")]
#[test]
fn two_devices_are_two_ports_and_one_leaving_takes_only_its_own() {
    let (mut s, fake, seen) = keyboards();
    let before = plan_nodes(&s);
    connect(&mut s, &["keys", "pads"]);
    assert_eq!(
        plan_nodes(&s),
        before + 4,
        "an input node and an inlet each"
    );
    roll(&s, 1_024);
    // Both arrive at one instant, the second device's first: the merged order
    // is the graph's `(offset, source)` and not the order of arrival.
    let at = Instant::now();
    assert!(fake.send_input(EndpointId::from_raw(2), key(0, 64), at));
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 60), at));
    roll(&s, 8_192);
    let heard = note_ons(&seen);
    assert_eq!(
        heard.iter().map(|n| n.1).collect::<Vec<_>>(),
        vec![60, 64],
        "both devices are heard, on one frame, in the order their ports were declared"
    );
    assert_eq!(heard[0].0, heard[1].0, "one frame: {heard:?}");

    connect(&mut s, &["pads"]);
    blocks(&s, 2);
    let _ = s.update();
    assert_eq!(
        plan_nodes(&s),
        before + 2,
        "only the removed port's input node and inlet left"
    );
    assert!(
        fake.send_input(EndpointId::from_raw(2), key(0, 65), Instant::now()),
        "the other device is still connected"
    );
}

/// **A port that is removed with a key held ends the key downstream, once**
/// (MIDI 1.0 Detailed Specification p. 24: individual Note Offs): the input node
/// node releases what the device held at its next block, and the executor's
/// close of the inlet's edge, which would send the same Note Off at the next
/// update, finds a device that is gone and is not heard.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_removed_port_ends_its_held_key_once() {
    let (mut s, fake, seen) = keyboards();
    connect(&mut s, &["keys"]);
    roll(&s, 1_024);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 60), Instant::now()));
    roll(&s, 8_192);
    assert_eq!(note_ons(&seen).len(), 1);
    s.pool(Vec::<
        tutti_session::Declared<&str, tutti_session::MidiInputEntry>,
    >::new())
        .expect("declares nothing");
    let _ = s.update();
    roll(&s, 8_192);
    let offs: Vec<u8> = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|&(_, e)| match e.note_key_event() {
            Some(NoteKeyEvent::Off(key)) => Some(key.note().get()),
            _ => None,
        })
        .collect();
    assert_eq!(offs, vec![60]);
}

/// **An arrival is placed on the engine's clock**: a note the OS stamps one
/// second ahead of now sounds a second of frames on, not at the next block.
/// An event pushed for the next block (the path of a session with no clock)
/// sounds within one.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_devices_arrival_is_placed_on_the_engines_clock() {
    let (mut s, fake, seen) = keyboards();
    connect(&mut s, &["keys"]);
    roll(&s, 1_024);
    let ahead = Instant::now() + Duration::from_secs(1);
    assert!(fake.send_input(EndpointId::from_raw(1), key(0, 60), ahead));
    roll(&s, 4_096);
    assert!(note_ons(&seen).is_empty(), "not yet: it is a second away");
    roll(&s, 60_000);
    let heard = note_ons(&seen);
    assert_eq!(heard.len(), 1);
    assert!(
        heard[0].0 >= 48_000,
        "a second of frames on, not at the next block: frame {}",
        heard[0].0
    );
}

/// **A watch that dies is reported once**, with its reason: the list is frozen
/// and the host is told so instead of seeing a quiet table.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_dead_device_watch_is_reported_once() {
    let (mut s, fake) = hot_plug_session();
    let failed = |events: &[SessionEvent]| {
        events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Midi(MidiReport::WatchFailed(why)) => Some(why.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(failed(&s.update()).is_empty());
    fake.fail_watch();
    let died = failed(&s.update());
    assert!(
        matches!(
            died.as_slice(),
            [tutti_midi_hardware::WatchFailure::Failed(e)]
                if matches!(**e, tutti_midi_hardware::MidiHardwareError::DescriptorBroken)
        ),
        "{died:?}"
    );
    assert!(failed(&s.update()).is_empty(), "once");
}

/// **An app holding a session restores a remembered device through it.** The
/// fake OS names devices as ALSA does, so a `midi_input` of the saved device
/// binds the present device as a same-name match and reads `Live`, and reads
/// `Lost` once it is gone.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_remembered_device_resolves_through_the_session() {
    use tutti_session::{PortCause, Presence, midi_input};
    let (mut s, fake) = hot_plug_session();
    plug(&fake, 1, "Keys", PortDirection::Input);
    let _ = s.update();
    let saved = s.midi().devices().remove(0);
    s.pool(midi_input("keys", saved)).expect("declares");
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
    assert_eq!(
        port.presence(),
        Presence::Live,
        "bound to the one of its name"
    );
    fake.unplug(EndpointId::from_raw(1));
    let _ = s.update();
    assert_eq!(port.presence(), Presence::Lost(PortCause::Unplugged));
}

/// **The feeds are partial, as audio's declarations are: an edge a host
/// wired on a fed event input stays.** A host wires its own clip into the
/// tap's event input 0 by hand; the session's sequencer then feeds the tap
/// too, and both notes play. A host's later `set_sources` on that
/// input sets its own edges beside the sequencer's; the sequencer's clip
/// re-aimed elsewhere leaves the host's edges where they are.
#[test]
fn a_hand_wired_event_edge_survives_the_sequencers_feed() {
    use tutti_midi_runtime::MidiClipNode;

    let sorted = |mut v: Vec<EventOutPort>| {
        v.sort();
        v
    };
    let input = |synth| EventInPort::new(synth, 0);
    let mut s = offline();
    let (synth, seen) = tap(&mut s);
    let (other, _) = tap(&mut s);
    let (hand, _) = s.graph().add(MidiClipNode::new(note(
        62,
        MF,
        Beat::new(1.0 / 3.0),
        BeatDuration::new(1.0),
    )));
    s.graph()
        .set_sources(input(synth), [hand.events_out()])
        .unwrap();
    s.midi()
        .play_clip(
            synth,
            Beat::new(0.0),
            note(60, MF, Beat::new(1.0 / 3.0), BeatDuration::new(1.0)),
        )
        .unwrap();
    let clip = s.midi().clip(synth).expect("the target's clip");
    let _ = s.update();
    // The clip feeds the target through its MPE transform.
    let transform = s.graph_ref().feed(synth, Feeder::Sequencer)[0];
    assert_eq!(
        sorted(s.graph_ref().sources(input(synth)).unwrap()),
        sorted(vec![hand.events_out(), transform]),
        "the sequencer's clip joins the hand-wired one"
    );
    roll(&s, 9_000);
    let mut ons = note_ons(&seen);
    ons.sort_unstable();
    assert_eq!(ons, vec![(8_000, 60), (8_000, 62)], "both play");

    let (hand2, _) = s.graph().add(MidiClipNode::new([]));
    s.graph()
        .set_sources(input(synth), [hand2.events_out()])
        .unwrap();
    assert_eq!(
        sorted(s.graph_ref().sources(input(synth)).unwrap()),
        sorted(vec![hand2.events_out(), transform]),
        "the host's call sets its own edges, beside the feed's"
    );

    s.midi().retarget(clip, other).unwrap();
    assert_eq!(
        s.graph_ref().sources(input(synth)).unwrap(),
        vec![hand2.events_out()],
        "the feed's edge goes, the host's stays"
    );
}

/// **A port that loses events says so, as a session event**: a chord of 1,100
/// notes on beat 0 against a port of 1,024 (the session's block) has its last
/// 76 note-ons refused. The next update reports `EventsDropped` once, for the
/// clip's port 0, with count 76; a second update with no further loss reports
/// nothing.
#[test]
fn a_port_that_loses_events_is_reported() {
    let mut s = offline();
    let (synth, _seen) = tap(&mut s);
    let chord = (0..1_100u32).map(|i| {
        TimedMidiEvent::new(
            Beat::new(0.0),
            Message::midi2(
                MidiChannel::of(((i / 128) % 16) as u8),
                Voice2::note_on(NoteNumber::of((i % 128) as u8), NoteVelocity::new(MF)),
            ),
        )
        .unwrap()
    });
    s.midi()
        .play_clip(synth, Beat::new(0.0), chord)
        .expect("takes MIDI");
    let _ = s.update();
    roll(&s, 512);
    let lost: Vec<_> = s
        .update()
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::EventsDropped { port, count, .. } => Some((port, count)),
            _ => None,
        })
        .collect();
    assert_eq!(lost, vec![(Some(0), 76)]);
    assert!(
        s.update()
            .iter()
            .all(|e| !matches!(e, SessionEvent::EventsDropped { .. }))
    );
}

/// **A MIDI-CI peer of an input port is in the port's info** (M2-101 §5.5 and
/// §5.6): once the UMP Endpoint settles, a Function Block that declares
/// MIDI-CI is sent a Discovery by the session's tick; the device's Reply to
/// Discovery is noticed as the port's info changing, and the peer is then in
/// [`MidiInputInfo::peers`](tutti_session::MidiInputInfo::peers).
#[cfg(feature = "midi-hardware")]
#[test]
fn a_midi_ci_peer_that_answers_discovery_is_in_the_ports_info() {
    use tutti_midi_ci::Muid;
    use tutti_midi_ci::{CiCategories, CiHeader, CiMessage, Decoded, DiscoveryData, MaxSysexSize};
    use tutti_midi_device::{Capabilities, Endpoint, FunctionBlock, Supports};
    use tutti_ump::BlockName;
    use tutti_ump::{
        BlockDirection, FirstGroup, FunctionBlockId, GroupSpan, SysexAssembler, SysexStep,
    };

    // Off unless the app says who it is, with an id it registered.
    let (mut s, fake) = hot_plug_session_ci(tutti_ump::DeviceIdentity {
        manufacturer: tutti_ump::ManufacturerId::NON_COMMERCIAL,
        family: [tutti_ump::U7::of(1); 2],
        model: [tutti_ump::U7::of(2); 2],
        revision: tutti_ump::SoftwareRevision::from([tutti_ump::U7::of(3); 4]),
    });
    plug(&fake, 7, "Wire", PortDirection::Duplex);
    let _ = s.update();
    connect(&mut s, &["wire"]);
    let port = s.midi_input(&"wire").expect("loaded");

    let block = FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        BlockDirection::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .midi_ci(2, 0)
    .name(BlockName::new("Main").unwrap())
    .build()
    .unwrap();
    let mut device = Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    }))
    .block(block);
    let raw = EndpointId::from_raw(7);
    let peer = Muid::of(0x0012_3456);
    let data = DiscoveryData::new(
        tutti_ump::DeviceIdentity {
            manufacturer: tutti_ump::ManufacturerId::of(0x41),
            family: [tutti_ump::U7::of(1); 2],
            model: [tutti_ump::U7::of(2); 2],
            revision: tutti_ump::SoftwareRevision::from([tutti_ump::U7::of(3); 4]),
        },
        CiCategories::empty(),
        MaxSysexSize::of(256),
    );
    let room = SysexAssembler::room_for(256, 1, 16);
    let mut assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), 256, 16);
    let start = Instant::now();
    let mut noticed = false;
    let mut replied = false;
    for ms in 0..40 {
        let now = start + Duration::from_millis(ms);
        for event in update_at(&mut s, now) {
            if let SessionEvent::ResourceChanged { id, fact } = event
                && id == port.id()
                && fact == tutti_session::ResourceFact::Info
                && !port.info().peers().is_empty()
            {
                noticed = true;
            }
        }
        let mut answers: Vec<Ump> = Vec::new();
        for ump in fake.take_sent(raw) {
            if let Ok(message @ tutti_ump::Message::Stream(_)) = ump.decode() {
                let _ = device.reply(message, &mut answers);
                continue;
            }
            assembler.begin_block();
            let SysexStep::Complete(done) = assembler.push(ump) else {
                continue;
            };
            if let Decoded::Message(CiMessage::Discovery { header, .. }) =
                CiMessage::decode(assembler.message(done).payload())
                && !replied
            {
                replied = true;
                let reply = CiMessage::DiscoveryReply {
                    header: CiHeader::new(peer),
                    to: header.source,
                    data,
                };
                answers.extend(reply.packets(MidiGroup::FIRST).unwrap());
            }
        }
        for ump in answers {
            assert!(fake.send_input(raw, ump, now));
        }
    }
    assert!(replied, "the session sent a Discovery");
    assert!(noticed, "the port's info changed when the peer was found");
    let info = port.info();
    assert_eq!(info.device().name, "Wire");
    let [listed] = info.peers() else {
        panic!("one peer listed, got {:?}", info.peers());
    };
    assert_eq!(listed.id().muid, peer);
    assert_eq!(listed.id().group, MidiGroup::FIRST);
    assert_eq!(listed.peer().discovery(), &data);
}

/// **A MIDI-CI peer's Profiles are the session's, and it switches them as the
/// Initiator** (M2-101 section 7): a peer whose Discovery Reply declares Profile
/// Configuration is sent a Profile Inquiry by the session's tick; its Replies
/// make the Profiles the port's info lists (`MidiReport::CiProfiles`, `Known`
/// at the final Reply on the Function Block); `enable` of a listed handle is
/// sent as a Set Profile On (Sub-ID#2 `0x22`) with the Channel count asked, and
/// the peer's Profile Enabled Report lands the job with `Enabled` and updates
/// the list. A request of a Profile the peer took off, of a lost port or of a
/// released one fails at the call.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_midi_ci_peers_profiles_are_listed_and_switched_through_the_session() {
    use tutti_midi_ci::Muid;
    use tutti_midi_ci::{
        CI_VERSION, CiCategories, CiDeviceId, CiHeader, CiMessage, Decoded, DiscoveryData,
        MaxSysexSize, ProfileChannels, ProfileId, ProfileList, ProfileLists, ProfileState,
        StandardProfile,
    };
    use tutti_midi_device::{Capabilities, Endpoint, FunctionBlock, Supports};
    use tutti_midi_hardware::{ChannelCount, ProfilesChange};
    use tutti_session::ListedProfile;
    use tutti_ump::BlockName;
    use tutti_ump::{
        BlockDirection, DeviceIdentity, FirstGroup, FunctionBlockId, GroupSpan, ManufacturerId,
        SysexAssembler, SysexStep, U7,
    };

    let identity = |manufacturer| DeviceIdentity {
        manufacturer,
        family: [U7::of(1); 2],
        model: [U7::of(2); 2],
        revision: tutti_ump::SoftwareRevision::from([U7::of(3); 4]),
    };
    let (mut s, fake) = hot_plug_session_ci(identity(ManufacturerId::NON_COMMERCIAL));
    plug(&fake, 7, "Wire", PortDirection::Duplex);
    let _ = s.update();
    connect(&mut s, &["wire"]);
    let port = s.midi_input(&"wire").expect("loaded");
    let block = FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        BlockDirection::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .midi_ci(2, 0)
    .name(BlockName::new("Main").unwrap())
    .build()
    .unwrap();
    let mut device = Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    }))
    .block(block);
    let raw = EndpointId::from_raw(7);
    let peer = Muid::of(0x0012_3456);
    let profile = ProfileId::Standard(StandardProfile::new(
        U7::of(0x21),
        U7::of(2),
        U7::of(1),
        U7::of(1),
    ));
    let at = |device_id| CiHeader {
        device_id,
        ci_version: CI_VERSION,
        source: peer,
    };
    let room = SysexAssembler::room_for(256, 1, 16);
    let mut assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), 256, 16);
    let start = Instant::now();
    let mut seen = Vec::new();
    let mut set_on = Vec::new();
    let mut enable = None;
    let mut enabled = None;
    for ms in 0..80 {
        let now = start + Duration::from_millis(ms);
        for event in update_at(&mut s, now) {
            match event {
                SessionEvent::Midi(MidiReport::CiProfiles { change, .. }) => seen.push(change),
                SessionEvent::JobDone(mut output) => {
                    enabled = enable.as_ref().and_then(|job| output.take(job)).or(enabled);
                }
                _ => {}
            }
        }
        if enable.is_none() && seen.iter().any(|c| matches!(c, ProfilesChange::Known)) {
            let info = port.info();
            let listed: Vec<_> = info.profiles()[0].listed().collect();
            let [ListedProfile::Channel(c)] = listed.as_slice() else {
                panic!("one Profile on a Channel: {listed:?}");
            };
            assert_eq!(listed[0].state(), ProfileState::Disabled);
            enable = Some(c.enable(ChannelCount::new(2).unwrap()));
        }
        let mut answers: Vec<Ump> = Vec::new();
        for ump in fake.take_sent(raw) {
            if let Ok(message @ tutti_ump::Message::Stream(_)) = ump.decode() {
                let _ = device.reply(message, &mut answers);
                continue;
            }
            assembler.begin_block();
            let SysexStep::Complete(done) = assembler.push(ump) else {
                continue;
            };
            match CiMessage::decode(assembler.message(done).payload()) {
                Decoded::Message(CiMessage::Discovery { header, .. }) => {
                    let data = DiscoveryData::new(
                        identity(ManufacturerId::of(0x41)),
                        CiCategories::PROFILE_CONFIGURATION,
                        MaxSysexSize::of(256),
                    );
                    answers.extend(
                        CiMessage::DiscoveryReply {
                            header: CiHeader::new(peer),
                            to: header.source,
                            data,
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    );
                }
                Decoded::Message(CiMessage::ProfileInquiry { header, .. }) => {
                    for (address, lists) in [
                        (
                            CiDeviceId::Channel(MidiChannel::of(0)),
                            ProfileLists::new(
                                ProfileList::default(),
                                ProfileList::try_from(vec![profile]).unwrap(),
                            ),
                        ),
                        (CiDeviceId::FunctionBlock, ProfileLists::none()),
                    ] {
                        answers.extend(
                            CiMessage::ProfileReply {
                                header: at(address),
                                to: header.source,
                                profiles: lists,
                            }
                            .packets(MidiGroup::FIRST)
                            .unwrap(),
                        );
                    }
                }
                Decoded::Message(CiMessage::SetProfileOn { target, .. }) => {
                    set_on.push((target.address(), target.channels()));
                    answers.extend(
                        CiMessage::ProfileEnabled {
                            source: peer,
                            ci_version: CI_VERSION,
                            target,
                            profile,
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    );
                }
                _ => {}
            }
        }
        for ump in answers {
            assert!(fake.send_input(raw, ump, now));
        }
    }
    assert_eq!(
        set_on,
        [(
            CiDeviceId::Channel(MidiChannel::of(0)),
            ProfileChannels::of(2)
        )],
        "one Set Profile On, for Channel 1 and two Channels"
    );
    assert_eq!(
        enabled,
        Some(Ok(ProfileState::Enabled)),
        "the Enabled Report lands the job"
    );
    let info = port.info();
    let listed: Vec<_> = info.profiles()[0].listed().collect();
    assert_eq!(listed[0].state(), ProfileState::Enabled);
    assert_eq!(listed[0].channels(), Some(ProfileChannels::of(2)));

    let [ListedProfile::Channel(c)] = listed.as_slice() else {
        panic!("one Profile on a Channel: {listed:?}");
    };
    // The peer lists a second Profile and then takes it off: a switch of the
    // handle made while it was listed fails at the call once it is not.
    let second = ProfileId::Standard(StandardProfile::new(
        U7::of(0x21),
        U7::of(3),
        U7::of(1),
        U7::of(1),
    ));
    let say = |s: &mut Session, message: CiMessage| {
        let now = Instant::now();
        for ump in message.packets(MidiGroup::FIRST).unwrap() {
            assert!(fake.send_input(raw, ump, now));
        }
        for ms in 0..4 {
            let _ = update_at(s, now + Duration::from_millis(ms));
        }
    };
    let header = at(CiDeviceId::Channel(c.channel()));
    say(
        &mut s,
        CiMessage::ProfileAdded {
            header,
            profile: second,
        },
    );
    let info = port.info();
    let stale = info.profiles()[0]
        .listed()
        .find(|listed| listed.profile() == second)
        .expect("the second Profile is listed");
    say(
        &mut s,
        CiMessage::ProfileRemoved {
            header,
            profile: second,
        },
    );
    assert_eq!(
        stale.disable().wait(&mut s),
        Err(tutti_session::PeerError::Disconnected),
        "a Profile the peer no longer lists fails at the call"
    );
    // A request the call accepts and the device then leaves before the next
    // update ends as `Disconnected`.
    let gone = c.disable();
    fake.unplug(raw);
    assert_eq!(
        gone.wait(&mut s),
        Err(tutti_session::PeerError::Disconnected)
    );
    // A request on a lost port fails at the call.
    assert_eq!(
        c.disable().wait(&mut s),
        Err(tutti_session::PeerError::Disconnected)
    );
    // The device returns with no peers found yet: a request made from the old
    // listing names a peer the port does not have, and fails at the call.
    plug(&fake, 7, "Wire", PortDirection::Duplex);
    let _ = s.update();
    assert_eq!(port.presence(), tutti_session::Presence::Live);
    assert_eq!(
        c.disable().wait(&mut s),
        Err(tutti_session::PeerError::Disconnected)
    );
    // A request on a released port fails at the call.
    s.pool(Vec::<
        tutti_session::Declared<&str, tutti_session::MidiInputEntry>,
    >::new())
        .expect("declares nothing");
    let _ = s.update();
    assert_eq!(port.presence(), tutti_session::Presence::Closed);
    assert_eq!(
        c.disable().wait(&mut s),
        Err(tutti_session::PeerError::Released)
    );
}

/// How a fake MIDI-CI peer answers a request.
#[cfg(feature = "midi-hardware")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    /// As the spec has it: the Report, or the Reply, the report's message and the
    /// End.
    Answer,
    /// A NAK.
    Refuse,
    /// Nothing.
    Silent,
}

/// What the session asked a fake MIDI-CI peer, in the order it arrived.
#[cfg(feature = "midi-hardware")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Asked {
    /// A Set Profile On, with the Number of Channels.
    On(tutti_midi_ci::ProfileChannels),
    /// A Set Profile Off.
    Off,
    /// An Inquiry: MIDI Message Report, with the scope asked.
    Report(tutti_midi_ci::ReportScope),
}

/// A MIDI-CI peer that lists the MPE Profile (M2-120 section 3.2) disabled on
/// Channel 1, enables it when asked with the Number of Channels (appendix A.3
/// and A.4) and disables it when asked, and sends a MIDI Message Report of one
/// Note On when asked for one, over the fake OS. `switch` and `report` say how
/// it answers those requests.
#[cfg(feature = "midi-hardware")]
pub(crate) struct MpePeer {
    device: tutti_midi_device::Endpoint,
    assembler: tutti_ump::SysexAssembler<Box<[u8]>>,
    pub(crate) muid: tutti_midi_ci::Muid,
    pub(crate) switch: Reply,
    pub(crate) report: Reply,
    /// What the session asked of it so far.
    pub(crate) asked: Vec<Asked>,
}

#[cfg(feature = "midi-hardware")]
impl MpePeer {
    pub(crate) const RAW: u64 = 7;
    /// The Note Number of the one message its report holds.
    pub(crate) const REPORTED_NOTE: u8 = 61;

    pub(crate) fn new() -> Self {
        use tutti_midi_device::{Capabilities, Endpoint, FunctionBlock, Supports};
        use tutti_ump::{BlockDirection, FirstGroup, FunctionBlockId, GroupSpan, SysexAssembler};
        let block = FunctionBlock::builder(
            FunctionBlockId::new(0).unwrap(),
            BlockDirection::Bidirectional,
            FirstGroup::new(0).unwrap(),
            GroupSpan::new(1).unwrap(),
        )
        .midi_ci(2, 0)
        .name(tutti_ump::BlockName::new("Main").unwrap())
        .build()
        .unwrap();
        let room = SysexAssembler::room_for(256, 1, 16);
        Self {
            device: Endpoint::new(Supports::of(Capabilities {
                midi1: true,
                midi2: true,
                ..Capabilities::default()
            }))
            .block(block),
            assembler: SysexAssembler::new(vec![0; room].into_boxed_slice(), 256, 16),
            muid: tutti_midi_ci::Muid::of(0x0012_3456),
            switch: Reply::Answer,
            report: Reply::Answer,
            asked: Vec::new(),
        }
    }

    /// What the session sent the peer, answered as the peer would.
    pub(crate) fn answer(
        &mut self,
        fake: &tutti_midi_hardware::test_support::FakeCounters,
        now: Instant,
    ) {
        use tutti_midi_ci::{
            CI_VERSION, CiCategories, CiDeviceId, CiHeader, CiMessage, Decoded, DiscoveryData,
            MaxSysexSize, Nak, NakStatus, PeTerminator, ProfileId, ProfileList, ProfileLists,
            ProfileTarget, SUB_ID2_MESSAGE_REPORT_INQUIRY, SUB_ID2_SET_PROFILE_OFF,
            SUB_ID2_SET_PROFILE_ON,
        };
        use tutti_ump::{DeviceIdentity, ManufacturerId, SysexStep, U7};
        let at = |device_id| CiHeader {
            device_id,
            ci_version: CI_VERSION,
            source: self.muid,
        };
        let mut answers: Vec<Ump> = Vec::new();
        for ump in fake.take_sent(EndpointId::from_raw(Self::RAW)) {
            if let Ok(message @ tutti_ump::Message::Stream(_)) = ump.decode() {
                let _ = self.device.reply(message, &mut answers);
                continue;
            }
            self.assembler.begin_block();
            let SysexStep::Complete(done) = self.assembler.push(ump) else {
                continue;
            };
            let decoded = CiMessage::decode(self.assembler.message(done).payload());
            match &decoded {
                Decoded::Message(CiMessage::SetProfileOn { target, .. }) => {
                    self.asked.push(Asked::On(target.channels()));
                }
                Decoded::Message(CiMessage::SetProfileOff { .. }) => self.asked.push(Asked::Off),
                Decoded::Message(CiMessage::MessageReportInquiry { scope, .. }) => {
                    self.asked.push(Asked::Report(*scope));
                }
                _ => {}
            }
            match decoded {
                Decoded::Message(CiMessage::Discovery { header, .. }) => {
                    let data = DiscoveryData::new(
                        DeviceIdentity {
                            manufacturer: ManufacturerId::of(0x41),
                            family: [U7::of(1); 2],
                            model: [U7::of(2); 2],
                            revision: tutti_ump::SoftwareRevision::from([U7::of(3); 4]),
                        },
                        CiCategories::PROFILE_CONFIGURATION | CiCategories::PROCESS_INQUIRY,
                        MaxSysexSize::of(256),
                    );
                    answers.extend(
                        CiMessage::DiscoveryReply {
                            header: CiHeader::new(self.muid),
                            to: header.source,
                            data,
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    );
                }
                Decoded::Message(CiMessage::ProfileInquiry { header, .. }) => {
                    for (address, lists) in [
                        (
                            CiDeviceId::Channel(MidiChannel::of(0)),
                            ProfileLists::new(
                                ProfileList::default(),
                                ProfileList::try_from(vec![ProfileId::MPE]).unwrap(),
                            ),
                        ),
                        (CiDeviceId::FunctionBlock, ProfileLists::none()),
                    ] {
                        answers.extend(
                            CiMessage::ProfileReply {
                                header: at(address),
                                to: header.source,
                                profiles: lists,
                            }
                            .packets(MidiGroup::FIRST)
                            .unwrap(),
                        );
                    }
                }
                Decoded::Message(CiMessage::SetProfileOn { source, target, .. }) => {
                    match self.switch {
                        Reply::Answer => answers.extend(
                            CiMessage::ProfileEnabled {
                                source: self.muid,
                                ci_version: CI_VERSION,
                                target,
                                profile: ProfileId::MPE,
                            }
                            .packets(MidiGroup::FIRST)
                            .unwrap(),
                        ),
                        Reply::Refuse => answers.extend(
                            CiMessage::Nak {
                                header: at(target.address()),
                                to: source,
                                nak: Nak::new(
                                    U7::of(SUB_ID2_SET_PROFILE_ON),
                                    NakStatus::ProfileNotSupported,
                                ),
                            }
                            .packets(MidiGroup::FIRST)
                            .unwrap(),
                        ),
                        Reply::Silent => {}
                    }
                }
                Decoded::Message(CiMessage::SetProfileOff { header, .. }) => match self.switch {
                    Reply::Answer => answers.extend(
                        CiMessage::ProfileDisabled {
                            source: self.muid,
                            ci_version: CI_VERSION,
                            target: match header.device_id {
                                CiDeviceId::Channel(channel) => ProfileTarget::Channel {
                                    channel,
                                    channels: tutti_midi_ci::ProfileChannels::of(4),
                                },
                                CiDeviceId::Group => ProfileTarget::Group,
                                CiDeviceId::FunctionBlock => ProfileTarget::FunctionBlock,
                            },
                            profile: ProfileId::MPE,
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    ),
                    Reply::Refuse => answers.extend(
                        CiMessage::Nak {
                            header: at(header.device_id),
                            to: header.source,
                            nak: Nak::new(
                                U7::of(SUB_ID2_SET_PROFILE_OFF),
                                NakStatus::ProfileNotSupported,
                            ),
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    ),
                    Reply::Silent => {}
                },
                Decoded::Message(CiMessage::ProcessCapabilitiesInquiry { header, .. }) => answers
                    .extend(
                        CiMessage::ProcessCapabilitiesReply {
                            header: at(CiDeviceId::FunctionBlock),
                            to: header.source,
                            features: tutti_midi_ci::ProcessFeatures::MIDI_MESSAGE_REPORT,
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    ),
                Decoded::Message(CiMessage::MessageReportInquiry { header, scope, .. }) => {
                    match self.report {
                        Reply::Answer => {
                            answers.extend(
                                CiMessage::MessageReportReply {
                                    header: at(header.device_id),
                                    to: header.source,
                                    scope,
                                }
                                .packets(MidiGroup::FIRST)
                                .unwrap(),
                            );
                            answers.push(midi::m1_note_on(
                                MidiChannel::of(0),
                                Self::REPORTED_NOTE,
                                U7::of(100),
                            ));
                            answers.extend(
                                CiMessage::MessageReportEnd {
                                    header: at(header.device_id),
                                    to: header.source,
                                }
                                .packets(MidiGroup::FIRST)
                                .unwrap(),
                            );
                        }
                        Reply::Refuse => answers.extend(
                            CiMessage::Nak {
                                header: at(header.device_id),
                                to: header.source,
                                nak: Nak::new(
                                    U7::of(SUB_ID2_MESSAGE_REPORT_INQUIRY),
                                    NakStatus::Terminate(PeTerminator::Responder),
                                ),
                            }
                            .packets(MidiGroup::FIRST)
                            .unwrap(),
                        ),
                        Reply::Silent => {}
                    }
                }
                _ => {}
            }
        }
        for ump in answers {
            assert!(fake.send_input(EndpointId::from_raw(Self::RAW), ump, now));
        }
    }
}

/// **A device that enables the MPE Profile switches its input's input node to the
/// Profile's rules, and disabling it switches back** (M2-120 sections 3.3,
/// 3.5 and Table 15). The session asks a peer to enable the Profile on
/// Channel 1 with four Channels; the Enabled Report is the zone, so the zone
/// channels' Reset All Controllers and Pitch Bend Sensitivities of 48 reach
/// the synth, and Omni Off on a Member Channel, which the Profile prohibits
/// and which passes with no zone, is dropped. Disabling is the same stop,
/// and Omni Off passes again. Closing the input clears its zones.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_devices_enabling_the_mpe_profile_switches_its_inputs_rules() {
    use tutti_fixtures::midi::BendRanges;
    use tutti_midi_ci::{ProfileChange, ProfileState};
    use tutti_midi_hardware::{ChannelCount, ProfilesChange};
    use tutti_midi_protocol::{MpeZoneConfig, MpeZones, PitchBendSensitivity};
    use tutti_session::ListedProfile;
    use tutti_ump::{CCNumber, Message, Voice2};

    let (mut s, fake) = hot_plug_session_ci(tutti_ump::DeviceIdentity {
        manufacturer: tutti_ump::ManufacturerId::NON_COMMERCIAL,
        family: [tutti_ump::U7::of(1); 2],
        model: [tutti_ump::U7::of(2); 2],
        revision: tutti_ump::SoftwareRevision::from([tutti_ump::U7::of(3); 4]),
    });
    let (synth, seen) = tap(&mut s);
    s.midi()
        .route(MidiRouteRule::any_channel().to(synth))
        .expect("routes");
    plug(&fake, MpePeer::RAW, "Wire", PortDirection::Duplex);
    let _ = s.update();
    connect(&mut s, &["wire"]);
    let port = s.midi_input(&"wire").expect("loaded");
    let mut peer = MpePeer::new();
    play(&s);
    let start = Instant::now();
    let clock = std::cell::Cell::new(0u64);
    let outputs = std::cell::RefCell::new(Vec::<tutti_session::JobOutput>::new());
    // Runs the session and the peer for `ms` milliseconds; the profile
    // changes the session reported.
    let mut run = |s: &mut Session, ms: u64| {
        let mut changes = Vec::new();
        for _ in 0..ms {
            clock.set(clock.get() + 1);
            let now = start + Duration::from_millis(clock.get());
            for event in update_at(s, now) {
                match event {
                    SessionEvent::Midi(MidiReport::CiProfiles { device, change, .. }) => {
                        changes.push((device, change));
                    }
                    SessionEvent::JobDone(l) => outputs.borrow_mut().push(l),
                    _ => {}
                }
            }
            peer.answer(&fake, now);
            blocks(s, 1);
        }
        changes
    };
    let changes = run(&mut s, 60);
    assert!(
        changes
            .iter()
            .any(|(_, c)| matches!(c, ProfilesChange::Known)),
        "the peer's Profiles are known"
    );
    let all: Vec<_> = port.info().profiles()[0].listed().collect();
    let [ListedProfile::Channel(listed)] = all.as_slice() else {
        panic!("one Profile on a Channel: {all:?}");
    };
    assert_eq!(all[0].state(), ProfileState::Disabled);

    // The device speaks MIDI 1.0, which the input node widens.
    let omni_off = |channel: u8| {
        midi::m1_cc(
            MidiChannel::of(channel),
            CCNumber::of(124),
            tutti_ump::U7::of(0),
        )
    };
    let omni_offs = |seen: &Seen| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e.decode(),
                    Ok(Message::Midi2(a, Voice2::ControlChange { controller, .. }))
                        if controller == CCNumber::of(124) && a.channel.get() == 2
                )
            })
            .count()
    };
    let raw = EndpointId::from_raw(MpePeer::RAW);

    // Not enabled: Omni Off on channel 3 passes.
    assert!(fake.send_input(raw, omni_off(2), Instant::now()));
    let _ = run(&mut s, 5);
    assert_eq!(omni_offs(&seen), 1, "no zone, no rule");

    // Enabled with four Channels from Channel 1.
    let enable = listed.enable(ChannelCount::new(4).unwrap());
    let changes = run(&mut s, 60);
    assert_eq!(
        outputs
            .borrow_mut()
            .iter_mut()
            .find_map(|l| l.take(&enable)),
        Some(Ok(ProfileState::Enabled))
    );
    assert!(
        changes.iter().any(|(_, c)| matches!(
            c,
            ProfilesChange::Changed {
                change: ProfileChange::Enabled,
                ..
            }
        )),
        "{changes:?}"
    );
    let info = port.info();
    assert_eq!(
        info.profiles()[0].profiles().mpe_zones(),
        Some(MpeZones::single(MpeZoneConfig::profile(
            MpeZoneSpan::new(MidiChannel::of(0), MpeChannels::of(4)).unwrap(),
            PitchBendSensitivity::MPE_PROFILE_DEFAULT
        )))
    );
    let mut ranges = BendRanges::default();
    for (_, ump) in seen.lock().unwrap().iter() {
        ranges.observe(ump);
    }
    for channel in 1..=3 {
        assert_eq!(
            ranges.semitones(MidiChannel::of(channel)),
            tutti_units::Semitones::new(48.0),
            "member {channel}"
        );
    }
    assert!(fake.send_input(raw, omni_off(2), Instant::now()));
    let _ = run(&mut s, 5);
    assert_eq!(omni_offs(&seen), 1, "the Profile prohibits it on a Member");

    // Disabled: the rules are MPE-less again.
    let listed: Vec<_> = port.info().profiles()[0].listed().collect();
    let [ListedProfile::Channel(listed)] = listed.as_slice() else {
        panic!("one Profile on a Channel: {listed:?}");
    };
    let disable = listed.disable();
    let _ = run(&mut s, 60);
    assert_eq!(
        outputs
            .borrow_mut()
            .iter_mut()
            .find_map(|l| l.take(&disable)),
        Some(Ok(ProfileState::Disabled))
    );
    assert_eq!(port.info().profiles()[0].profiles().mpe_zones(), None);
    assert!(fake.send_input(raw, omni_off(2), Instant::now()));
    let _ = run(&mut s, 5);
    assert_eq!(omni_offs(&seen), 2, "no zone, no rule");
}

/// **A MIDI-CI peer's MIDI Message Report is asked for through the session**
/// (M2-101 section 9): a peer whose Discovery Reply declares Process Inquiry is
/// sent an Inquiry: Process Inquiry Capabilities by the session's tick; its reply
/// with MIDI Message Report makes it a `MidiInputInfo::report_peers` handle
/// (`MidiReport::CiProcess`); `ReportPeer::request` sends an Inquiry: MIDI
/// Message Report (Sub-ID#2 `0x42`) with the bitmaps asked, and the peer's End
/// lands the job.
#[cfg(feature = "midi-hardware")]
#[test]
fn a_midi_ci_peers_message_report_is_asked_for_through_the_session() {
    use tutti_midi_ci::Muid;
    use tutti_midi_ci::{
        CI_VERSION, ChannelControllers, CiCategories, CiDeviceId, CiHeader, CiMessage, DataControl,
        Decoded, DiscoveryData, MaxSysexSize, NoteData, ProcessFeatures, ReportRequest,
        ReportScope, SystemMessages,
    };
    use tutti_midi_device::{Capabilities, Endpoint, FunctionBlock, Supports};
    use tutti_midi_hardware::ProcessChange;
    use tutti_ump::BlockName;
    use tutti_ump::{
        BlockDirection, DeviceIdentity, FirstGroup, FunctionBlockId, GroupSpan, ManufacturerId,
        SysexAssembler, SysexStep, U7,
    };

    let identity = |manufacturer| DeviceIdentity {
        manufacturer,
        family: [U7::of(1); 2],
        model: [U7::of(2); 2],
        revision: tutti_ump::SoftwareRevision::from([U7::of(3); 4]),
    };
    let (mut s, fake) = hot_plug_session_ci(identity(ManufacturerId::NON_COMMERCIAL));
    plug(&fake, 7, "Wire", PortDirection::Duplex);
    let _ = s.update();
    connect(&mut s, &["wire"]);
    let port = s.midi_input(&"wire").expect("loaded");
    let block = FunctionBlock::builder(
        FunctionBlockId::new(0).unwrap(),
        BlockDirection::Bidirectional,
        FirstGroup::new(0).unwrap(),
        GroupSpan::new(1).unwrap(),
    )
    .midi_ci(2, 0)
    .name(BlockName::new("Main").unwrap())
    .build()
    .unwrap();
    let mut device = Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        midi2: true,
        ..Capabilities::default()
    }))
    .block(block);
    let raw = EndpointId::from_raw(7);
    let peer = Muid::of(0x0012_3456);
    let at = |device_id| CiHeader {
        device_id,
        ci_version: CI_VERSION,
        source: peer,
    };
    let asked = ReportScope::FunctionBlock {
        system: SystemMessages::SONG_POSITION,
        controllers: ChannelControllers::CONTROL_CHANGE,
        notes: NoteData::empty(),
    };
    let room = SysexAssembler::room_for(256, 1, 16);
    let mut assembler = SysexAssembler::new(vec![0; room].into_boxed_slice(), 256, 16);
    let start = Instant::now();
    let mut seen = Vec::new();
    let mut inquiries = Vec::new();
    let mut request = None;
    let mut ended = None;
    for ms in 0..80 {
        let now = start + Duration::from_millis(ms);
        for event in update_at(&mut s, now) {
            match event {
                SessionEvent::Midi(MidiReport::CiProcess { change, .. }) => seen.push(change),
                SessionEvent::JobDone(mut output) => {
                    ended = request.as_ref().and_then(|job| output.take(job)).or(ended);
                }
                _ => {}
            }
        }
        if request.is_none()
            && seen
                .iter()
                .any(|c| matches!(c, ProcessChange::Capabilities(_)))
        {
            let info = port.info();
            let [handle] = info.report_peers() else {
                panic!("one peer that can be asked: {:?}", info.report_peers());
            };
            request = Some(handle.request(ReportRequest::new(DataControl::Full, asked)));
        }
        let mut answers: Vec<Ump> = Vec::new();
        for ump in fake.take_sent(raw) {
            if let Ok(message @ tutti_ump::Message::Stream(_)) = ump.decode() {
                let _ = device.reply(message, &mut answers);
                continue;
            }
            assembler.begin_block();
            let SysexStep::Complete(done) = assembler.push(ump) else {
                continue;
            };
            match CiMessage::decode(assembler.message(done).payload()) {
                Decoded::Message(CiMessage::Discovery { header, .. }) => {
                    let data = DiscoveryData::new(
                        identity(ManufacturerId::of(0x41)),
                        CiCategories::PROCESS_INQUIRY,
                        MaxSysexSize::of(256),
                    );
                    answers.extend(
                        CiMessage::DiscoveryReply {
                            header: CiHeader::new(peer),
                            to: header.source,
                            data,
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    );
                }
                Decoded::Message(CiMessage::ProcessCapabilitiesInquiry { header, .. }) => {
                    answers.extend(
                        CiMessage::ProcessCapabilitiesReply {
                            header: at(CiDeviceId::FunctionBlock),
                            to: header.source,
                            features: ProcessFeatures::MIDI_MESSAGE_REPORT,
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    );
                }
                Decoded::Message(CiMessage::MessageReportInquiry {
                    header,
                    control,
                    scope,
                    ..
                }) => {
                    inquiries.push((header.device_id, control, scope));
                    answers.extend(
                        CiMessage::MessageReportReply {
                            header: at(header.device_id),
                            to: header.source,
                            scope,
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    );
                    answers.extend(
                        CiMessage::MessageReportEnd {
                            header: at(header.device_id),
                            to: header.source,
                        }
                        .packets(MidiGroup::FIRST)
                        .unwrap(),
                    );
                }
                _ => {}
            }
        }
        for ump in answers {
            assert!(fake.send_input(raw, ump, now));
        }
    }
    assert_eq!(
        inquiries,
        [(CiDeviceId::FunctionBlock, DataControl::Full, asked)],
        "one Inquiry: MIDI Message Report, on the Function Block, as asked"
    );
    assert_eq!(ended, Some(Ok(())), "the peer's End lands the job");

    // A request the call accepts and the device then leaves before the next
    // update ends as `Disconnected`; on a lost port it fails at the call; and
    // once the device is back with no peers found, a request made from the old
    // listing fails at the call.
    let info = port.info();
    let handle = &info.report_peers()[0];
    let gone = handle.request(ReportRequest::new(DataControl::Full, asked));
    fake.unplug(raw);
    assert_eq!(
        gone.wait(&mut s),
        Err(tutti_session::PeerError::Disconnected)
    );
    assert_eq!(
        handle
            .request(ReportRequest::new(DataControl::Full, asked))
            .wait(&mut s),
        Err(tutti_session::PeerError::Disconnected)
    );
    plug(&fake, 7, "Wire", PortDirection::Duplex);
    let _ = s.update();
    assert_eq!(
        handle
            .request(ReportRequest::new(DataControl::Full, asked))
            .wait(&mut s),
        Err(tutti_session::PeerError::Disconnected)
    );
}

/// Runs the Stream negotiation of the open duplex device `raw`, the input port
/// `"dev"`, to its end, answering as an Endpoint that receives no JR does.
/// Returns the time it started at.
#[cfg(feature = "midi-hardware")]
fn settle(
    s: &mut Session,
    fake: &tutti_midi_hardware::test_support::FakeCounters,
    raw: EndpointId,
) -> Instant {
    use tutti_midi_device::{Capabilities, Endpoint, Supports};
    use tutti_ump::Message;
    // The device answers the session's Stream messages as an Endpoint does.
    let mut device = Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        midi2: true,
        rx_jr: true,
        tx_jr: false,
    }));
    let start = Instant::now();
    let mut settled = false;
    for ms in 0..8 {
        let now = start + Duration::from_millis(ms);
        let _ = update_at(s, now);
        settled |= s
            .midi_input(&"dev")
            .is_ok_and(|port| port.info().stream().is_some());
        let mut answers = Vec::new();
        for ump in fake.take_sent(raw) {
            if let Ok(message @ Message::Stream(_)) = ump.decode() {
                let _ = device.reply(message, &mut answers);
            }
        }
        for ump in answers {
            assert!(fake.send_input(raw, ump, now));
        }
    }
    assert!(settled, "the negotiation settled");
    start
}

/// Declares the duplex device `Dev` as the input port `"dev"` and the output
/// port `"dev"`, and updates.
#[cfg(feature = "midi-hardware")]
fn declare_dev(s: &mut Session) {
    let dev = s
        .midi()
        .devices()
        .into_iter()
        .find(|d| d.name == "Dev")
        .expect("Dev is listed");
    s.pool((
        tutti_session::midi_input("dev", dev.clone()),
        tutti_session::midi_output("dev out", dev),
    ))
    .expect("declares");
    let _ = s.update();
}

/// **An output port JR-stamps only as its device's configuration says**
/// (M2-104 §7.1.6.2: JR Timestamps go to a device only once a Stream
/// Configuration Notification has said it receives them). The
/// session asks for JR off, so the output is unstamped when the device
/// settles; when the device notifies that it receives JR the port's info says so
/// and the wire carries the JR messages; when it notifies the reverse the
/// stamps stop.
#[cfg(feature = "midi-hardware")]
#[test]
fn an_output_port_stamps_as_the_devices_configuration_says() {
    use tutti_ump::{Message, Protocol, Stream, StreamConfig};

    let (mut s, fake) = hot_plug_session();
    plug(&fake, 7, "Dev", PortDirection::Duplex);
    let _ = s.update();
    let raw = EndpointId::from_raw(7);
    declare_dev(&mut s);
    let out = s.midi_output(&"dev out").expect("loaded");
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });

    let start = settle(&mut s, &fake, raw);
    assert!(!out.info().config().rx_jr, "asked for JR off");

    // A stretch of the session's clock, which the output sends.
    let wire_after = |s: &mut Session, notification: StreamConfig, ms: u64| {
        let ump = Ump::from(Message::Stream(Stream::StreamConfigurationNotification(
            notification,
        )));
        assert!(fake.send_input(raw, ump, start));
        update_at(s, start + Duration::from_millis(ms));
        fake.take_sent(raw);
        roll(s, 4_096);
        update_at(s, start + Duration::from_millis(ms + 1));
        fake.take_sent(raw)
    };
    let config = |rx_jr| StreamConfig {
        protocol: Protocol::Midi2,
        rx_jr,
        tx_jr: false,
    };
    let sent = wire_after(&mut s, config(true), 20);
    assert!(out.info().config().rx_jr);
    assert!(
        sent.iter().any(|u| u.is_jr()) && sent.iter().any(|u| u.message_type() == 1),
        "{sent:?}"
    );
    let sent = wire_after(&mut s, config(false), 40);
    assert!(!out.info().config().rx_jr);
    assert!(!sent.iter().any(|u| u.is_jr()), "{sent:?}");
    assert!(!sent.is_empty(), "the clock still goes out");
}

/// M2-104 §7.1.6.2: a device that asked to receive JR Timestamps (RXJR) has
/// every message prefixed with one. Once it notifies RXJR, each message the
/// clock master sends it follows a JR Timestamp directly, and the stamps never
/// go backwards.
/// Spec: m2-104/7.1.6.2#4
#[cfg(feature = "midi-hardware")]
#[test]
fn a_device_that_receives_jr_has_every_message_prefixed_by_a_timestamp() {
    use tutti_ump::{Message, Protocol, Stream, StreamConfig, Utility};

    let (mut s, fake) = hot_plug_session();
    plug(&fake, 7, "Dev", PortDirection::Duplex);
    let _ = s.update();
    let raw = EndpointId::from_raw(7);
    declare_dev(&mut s);
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    let start = settle(&mut s, &fake, raw);
    let on = Ump::from(Message::Stream(Stream::StreamConfigurationNotification(
        StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: true,
            tx_jr: false,
        },
    )));
    assert!(fake.send_input(raw, on, start));
    update_at(&mut s, start + Duration::from_millis(20));
    fake.take_sent(raw);
    roll(&s, 4_096);
    update_at(&mut s, start + Duration::from_millis(21));
    let sent = fake.take_sent(raw);
    let stamp = |u: &Ump| match u.decode() {
        Ok(Message::Utility(Utility::JrTimestamp(t))) => Some(t),
        _ => None,
    };
    let messages: Vec<usize> = (0..sent.len())
        .filter(|&i| sent[i].message_type() != 0)
        .collect();
    assert!(!messages.is_empty(), "the clock goes out: {sent:?}");
    let mut last = 0;
    for i in messages {
        let before = i.checked_sub(1).and_then(|j| stamp(&sent[j]));
        let t = before.unwrap_or_else(|| panic!("message {i} has no stamp: {sent:?}"));
        assert!(t >= last, "stamps do not go back: {sent:?}");
        last = t;
    }
}

/// The outputs' latency and offset are a target: the default is nothing, and
/// what is written is read back whole.
#[test]
fn the_output_latency_is_written_and_read_through_its_target() {
    use tutti_midi_runtime::{OutputLatency, OutputOffset};
    use tutti_units::Seconds;

    let mut s = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .build()
        .expect("builds");
    let target = s.midi().output_latency();
    assert_eq!(s.get(target).unwrap(), OutputLatency::default());
    let latency = OutputLatency::new(
        tutti_units::Seconds::new(0.007),
        OutputOffset::new(Seconds::new(-0.002)).unwrap(),
    )
    .unwrap();
    s.set(target.to(latency)).unwrap();
    assert_eq!(s.get(target).unwrap(), latency);
    assert_eq!(s.midi().output_terms(), None, "offline has no terms");
}
