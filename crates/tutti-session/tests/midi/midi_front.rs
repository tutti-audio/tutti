//! The session's MIDI front: hardware through `s.midi()`, the session's
//! one MPE mode, a take recorded with `s.midi().record`, its placement, and a
//! saved route rule.

#[cfg(feature = "midi-hardware")]
use crate::common::OpenPorts as _;
use crate::common::{Seen, Tap, blocks, offline};
use tutti_buffers::ChannelLayout;
use tutti_fixtures::midi;
use tutti_graph::{
    Cx, EventCapacity, ForkByClone, Instrument, IntoNode, Io, Node, NodeParts, Prepare, Shape,
    Status,
};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_session::{MidiError, Session, SessionNode};
#[cfg(feature = "serde")]
use tutti_ump::MidiGroup;
use tutti_ump::{MidiChannel, NoteNumber, NoteVelocity, Voice2};
use tutti_units::{Beat, Frame, SampleRate, Samples};

/// Plays as an instrument (five outputs and an event input).
struct Probe(ForkByClone<Five>);

#[derive(Clone)]
struct Five;
impl Node for Five {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::STEREO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}
impl IntoNode for Probe {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        self.0.into_parts()
    }
}
impl Instrument<f32> for Probe {}
impl SessionNode for Probe {
    fn reclaimer(_: &()) -> tutti_session::Reclaims {
        tutti_session::Reclaims::Nothing
    }
}

// --- Recording a take --------------------------------------------------------

/// A silent stereo source that declares 256 frames of latency, so the graph's
/// output time is 256 frames behind its nodes: what an aligned tap would delay by.
#[derive(Clone)]
struct Latent;
impl Node for Latent {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::STEREO)
            .with_latency(tutti_units::Latency::new(Samples::new(256)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// **A take reads what was played through the live input, frame for frame, and
/// the target still hears it.** A note on at frame 600 and off at 900, sent
/// through `input`, are what `record`'s reader drains: the packets as
/// sent, on those frames, in order (the frames are the graph's own, **not**
/// delayed by the 256 frames of latency another path of this graph has); a
/// second drain is empty; and the tap target heard both as well.
#[test]
fn a_take_records_the_live_input_on_its_frames_and_the_target_still_hears_it() {
    let mut s = offline();
    let seen = tutti_fixtures::Record::new();
    let target = s.graph().add(Tap(seen.clone()));
    let latent = s.graph().add(ForkByClone(Latent));
    s.graph().connect(latent, tutti_session::Root).unwrap();
    let mut live = s.midi().input(target).expect("a live input");
    let mut take = s
        .midi()
        .tap(target, EventCapacity::of(64))
        .expect("the tap takes MIDI");
    let _ = s.update();
    let (c, k) = (MidiChannel::of(2), NoteNumber::of(60));
    live.note_on(Frame::new(600), c, k, NoteVelocity::CENTER)
        .unwrap();
    live.note_off(Frame::new(900), c, k).unwrap();
    blocks(&s, 3);

    let got: Vec<_> = take.drain().map(|e| (e.frame, e.ump)).collect();
    assert_eq!(
        got,
        [
            (Frame::new(600), midi::note_on(c, 60, 0x8000)),
            (Frame::new(900), midi::note_off(c, 60, 0)),
        ]
    );
    assert_eq!(take.drain().count(), 0, "a second drain is empty");
    assert_eq!(take.take_lost(), 0);
    let heard: Vec<u64> = seen.lock().unwrap().iter().map(|(f, _)| *f).collect();
    assert_eq!(heard, [600, 900], "recording took nothing from the target");
}

/// **A take of an instrument records what its handle's keys play**, frame for
/// frame, though the keys are not `Midi::input`'s: the tap is on the inlet
/// that feeds the instrument, wherever its keys came from.
#[test]
fn a_take_of_an_instrument_records_what_its_handle_plays() {
    let mut s = offline();
    let mut synth = s
        .add_instrument(Probe(ForkByClone(Five)))
        .expect("an instrument");
    let mut take = s
        .midi()
        .tap(synth.node(), EventCapacity::of(64))
        .expect("an instrument takes MIDI");
    let _ = s.update();
    let (c, k) = (MidiChannel::of(2), NoteNumber::of(60));
    synth
        .keys_mut()
        .note_on(Frame::new(600), c, k, NoteVelocity::CENTER)
        .unwrap();
    synth.keys_mut().note_off(Frame::new(900), c, k).unwrap();
    blocks(&s, 3);
    let got: Vec<_> = take.drain().map(|e| (e.frame, e.ump)).collect();
    assert_eq!(
        got,
        [
            (Frame::new(600), midi::note_on(c, 60, 0x8000)),
            (Frame::new(900), midi::note_off(c, 60, 0)),
        ]
    );
}

/// **An instrument has one live input, its handle's**: asking `Midi::input`
/// for a second is refused by name, and nothing is opened.
#[test]
fn an_instrument_has_its_handles_keys_and_no_second_live_input() {
    let mut s = offline();
    let synth = s
        .add_instrument(Probe(ForkByClone(Five)))
        .expect("an instrument");
    let key = synth.node().erase();
    assert!(matches!(
        s.midi().input(key),
        Err(MidiError::InstrumentKeys(n)) if n == key
    ));
    assert!(
        s.graph_ref()
            .feed(key, tutti_session::Feeder::LiveInput)
            .unwrap()
            .is_empty(),
        "no second inlet was opened"
    );
}

/// **A node's keys have one owner, whichever was asked first**: a node whose
/// live input handed out its keys ([`Midi::input`](tutti_session::Midi::input))
/// is not made an instrument with a second pair, and an instrument's node
/// hands out no live keys (the test above). Nothing is opened by the refusal.
#[test]
fn a_node_whose_live_input_has_its_keys_is_not_made_an_instrument() {
    let mut s = offline();
    let node = s.graph().add(Probe(ForkByClone(Five)));
    let key = node.erase();
    let _keys = s.midi().input(key).expect("the live input's keys");
    assert!(matches!(
        s.instrument(key),
        Err(tutti_session::Error::NotInstrument {
            why: tutti_session::NotInstrument::LiveInput,
            ..
        })
    ));
    assert!(
        s.graph_ref()
            .feed(key, tutti_session::Feeder::Instrument)
            .unwrap()
            .is_empty(),
        "no instrument queue was opened"
    );
}

/// **The instrument's key is typed, so a source of the caller's own wires into
/// its event input** (`events_in` is a method of `Key`, which an erased
/// `NodeKey` has not).
#[test]
fn an_instruments_typed_key_takes_a_wire_of_the_callers_own() {
    let mut s = offline();
    let synth = s
        .add_instrument(Probe(ForkByClone(Five)))
        .expect("an instrument");
    let (inlet, _writer) = s.graph().open_event_inlet(EventCapacity::of(8));
    s.graph()
        .connect(inlet.events_out(), synth.node().events_in())
        .expect("the instrument's main event input takes a second source");
    assert!(
        s.graph_ref()
            .sources(synth.node().events_in())
            .unwrap()
            .contains(&inlet.events_out()),
        "the caller's wire is one of the input's sources"
    );
}

/// **A target with no live input is refused, never given one.**
#[test]
fn tapping_a_target_with_no_live_input_is_refused() {
    let mut s = offline();
    let target = s.graph().add(Tap(tutti_fixtures::Record::new()));
    assert!(matches!(
        s.midi().tap(target, EventCapacity::of(8)),
        Err(MidiError::NoLiveInput(n)) if n == target.erase()
    ));
}

/// A target that takes no MIDI cannot be recorded.
#[test]
fn recording_a_target_with_no_event_input_is_refused() {
    let mut s = offline();
    let dc = s
        .graph()
        .add(ForkByClone(tutti_nodes::testing::Const::mono(0.5)));
    assert!(matches!(
        s.midi().tap(dc, EventCapacity::of(8)),
        Err(MidiError::NoEventInput(n)) if n == dc.erase()
    ));
}

// --- MPE ---------------------------------------------------------------------

/// A clip of the first Group's events: each `(beat, packet)`.
fn clip(events: impl IntoIterator<Item = (f64, tutti_ump::Ump)>) -> Vec<TimedMidiEvent> {
    events
        .into_iter()
        .map(|(beat, ump)| TimedMidiEvent::new(Beat::new(beat), ump).unwrap())
        .collect()
}

/// What a tap heard, as `(channel, message)` of MIDI 2.0 channel voice.
fn voices(seen: &Seen) -> Vec<(u8, Voice2)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|(_, e)| {
            let (address, voice) = tutti_midi_protocol::channel_voice(*e)?;
            Some((address.channel.get(), voice))
        })
        .collect()
}

/// A member channel's note and bend, then the manager's Damper Pedal, of a
/// lower zone of three members: what the session's mode makes of a clip.
fn mpe_clip() -> Vec<TimedMidiEvent> {
    let member = MidiChannel::of(1);
    clip([
        (0.0, midi::note_on(member, 60, 0x8000)),
        (0.1, midi::pitch_bend(member, 0xC000_0000)),
        (
            0.2,
            midi::cc(MidiChannel::FIRST, tutti_ump::CCNumber::SUSTAIN, u32::MAX),
        ),
    ])
}

fn play_for(s: &mut Session, blocks: usize) {
    let _ = s.update();
    crate::common::play(s);
    crate::common::blocks(s, blocks);
}

/// **A clip is transformed in the session's MPE mode**: `Midi::play_clip`
/// wires the clip through an MPE transform that starts in the session's mode
/// (the builder's here), so a member channel's bend reaches the target as a
/// per-note bend on the note that channel holds, and the manager's Damper Pedal
/// as the pedal of every channel of its zone (M1-100 section 2.3.1).
#[test]
fn a_clip_plays_through_the_sessions_mpe_mode() {
    let mut s = Session::builder()
        .mpe_mode(midi::lower_zone(3))
        .sample_rate(SampleRate::SR_48K)
        .build()
        .expect("builds");
    assert_eq!(s.get(s.mpe()).unwrap(), midi::lower_zone(3));
    let (target, seen) = crate::common::tap(&mut s);
    s.midi()
        .play_clip(target, Beat::new(0.0), mpe_clip())
        .unwrap();
    play_for(&mut s, 40);
    let got = voices(&seen);
    assert!(
        got.iter().any(|(c, v)| *c == 1
            && matches!(v, Voice2::PerNotePitchBend { note, .. } if note.get() == 60)),
        "{got:?}"
    );
    let pedals: Vec<u8> = got
        .iter()
        .filter(|(_, v)| {
            matches!(v, Voice2::ControlChange { controller, .. }
                if *controller == tutti_ump::CCNumber::SUSTAIN)
        })
        .map(|(c, _)| *c)
        .collect();
    assert_eq!(pedals, [0, 1, 2, 3]);
}

/// **A write of the MPE mode reaches a clip already in the graph**: a session that began with
/// no MPE plays its clip with the manager's pedal on one channel; after
/// the write the same clip's pedal is the zone's, since the transform the clip
/// was wired through took the new mode. (The mode used to reach only a synth
/// added afterwards.)
#[test]
fn the_mpe_mode_reaches_a_clip_already_in_the_graph() {
    let mut s = crate::common::offline();
    let (target, seen) = crate::common::tap(&mut s);
    s.midi()
        .play_clip(target, Beat::new(0.0), mpe_clip())
        .unwrap();
    let _ = s.update();
    s.set(s.mpe().to(midi::lower_zone(3))).unwrap();
    assert_eq!(s.get(s.mpe()).unwrap(), midi::lower_zone(3));
    play_for(&mut s, 40);
    let got = voices(&seen);
    let pedals = got
        .iter()
        .filter(|(_, v)| {
            matches!(v, Voice2::ControlChange { controller, .. }
                if *controller == tutti_ump::CCNumber::SUSTAIN)
        })
        .count();
    assert_eq!(pedals, 4, "{got:?}");
    assert!(
        got.iter()
            .any(|(_, v)| matches!(v, Voice2::PerNotePitchBend { .. })),
        "{got:?}"
    );
}

/// **The transform is the clip's wiring**: removing the clip takes its
/// transform out of the graph, and `retarget` moves both to the new target (the
/// transform still feeds it).
#[test]
fn a_clips_transform_goes_with_the_clip_and_follows_a_retarget() {
    let mut s = crate::common::offline();
    let (first, _) = crate::common::tap(&mut s);
    let (second, seen) = crate::common::tap(&mut s);
    let before = s.graph().node_count();
    s.midi()
        .play_clip(first, Beat::new(0.0), mpe_clip())
        .unwrap();
    assert_eq!(
        s.graph().node_count(),
        before + 2,
        "the clip and its transform"
    );
    let clip = s.midi().clip(first).expect("the clip").erase();
    s.midi().retarget(clip, second).unwrap();
    assert!(s.midi().clip(first).is_none());
    assert_eq!(s.midi().clip(second).map(|k| k.erase()), Some(clip));
    s.set(s.mpe().to(midi::lower_zone(3))).unwrap();
    play_for(&mut s, 40);
    assert!(
        !voices(&seen).is_empty(),
        "the second target hears the clip"
    );

    s.remove(clip).unwrap();
    assert_eq!(s.graph().node_count(), before, "the transform went too");
}

// --- A saved route rule ------------------------------------------------------

/// **A saved route rule keyed by an app's own id reads back, and a channel the
/// wire cannot hold is refused**, not clamped: the rule's Group and channel are
/// range-checked values.
#[cfg(feature = "serde")]
#[test]
fn a_route_rule_keyed_by_the_apps_id_round_trips_and_refuses_a_bad_channel() {
    use tutti_session::MidiRouteRule;
    let rule = MidiRouteRule::<String>::new()
        .channel(MidiChannel::of(4))
        .in_group(MidiGroup::of(2))
        .to("lead".to_string())
        .to("pad".to_string())
        .disabled();
    let json = serde_json::to_string(&rule).unwrap();
    let back: MidiRouteRule<String> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, rule);
    // Both states of `enabled` survive, so neither is a default filled in.
    let on = MidiRouteRule::<String>::new()
        .channel(MidiChannel::of(4))
        .to("lead".to_string());
    let on_back: MidiRouteRule<String> =
        serde_json::from_str(&serde_json::to_string(&on).unwrap()).unwrap();
    assert_eq!(on_back, on);

    let bad = json.replace("\"channel\":4", "\"channel\":16");
    assert_ne!(bad, json, "the channel was in the text");
    assert!(serde_json::from_str::<MidiRouteRule<String>>(&bad).is_err());
}

// --- Hardware through the view -----------------------------------------------

#[cfg(feature = "midi-hardware")]
mod hardware {
    use super::*;
    use tutti_midi_device::{EndpointId, OsEndpointIdentity, PortDirection};
    use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint};
    use tutti_session::{MidiPortError, Peer, PortCause, Presence};
    use tutti_ump::ProductInstanceId;

    fn session() -> (Session, FakeCounters) {
        let (backend, fake) = FakeBackend::build_with(vec![], vec![], usize::MAX);
        let s = Session::builder()
            .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
            .sample_rate(SampleRate::SR_48K)
            .build()
            .expect("builds");
        (s, fake)
    }

    /// A device the OS names by a unit, so its saved identity is exact.
    fn unit(raw: u64, name: &str, unit: &str) -> tutti_midi_hardware::OsEndpoint {
        let mut e = endpoint(raw, name);
        e.identity = OsEndpointIdentity::Product(ProductInstanceId::new(unit).unwrap());
        e
    }

    /// **The view lists the devices, and a session opens and closes them as
    /// ports**: an input and an output are opened; the output's node is in the
    /// graph; dropping the input closes its connection.
    #[test]
    fn the_view_lists_devices_and_a_session_opens_and_closes_ports() {
        let (mut s, fake) = session();
        fake.plug(endpoint(1, "Keys"), PortDirection::Input);
        fake.plug(endpoint(10, "Synth"), PortDirection::Output);
        let _ = s.update();

        let all = s.midi().devices();
        let names: Vec<&str> = all.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Keys", "Synth"], "inputs and outputs, by id");

        let keys = s.open_input(all[0].clone()).expect("opens");
        let out = s.open_output(all[1].clone()).expect("opens");
        assert_eq!(fake.connected_inputs(), [EndpointId::from_raw(1)], "Keys");
        assert_eq!(out.presence(), Presence::Live);
        assert!(s.graph_ref().contains(out.node()), "the output's node");
        assert_eq!(out.info().device(), &all[1]);

        drop(keys);
        let _ = s.update();
        assert!(
            fake.connected_inputs().is_empty(),
            "the input's connection closed with its last handle"
        );
        assert_eq!(out.presence(), Presence::Live, "the output is still held");
    }

    /// **A session with no OS layer lists nothing**; opening a port of a device
    /// fails by name (`midi_ports`).
    #[test]
    fn a_session_with_no_os_layer_lists_nothing() {
        let mut bare = offline();
        assert!(bare.midi().devices().is_empty());
    }

    /// **A saved device is bound at open when it is the one, and fails with its
    /// candidates when it could be several.** A device named by its unit binds; a
    /// saved output of a unit whose port went, with another port of the unit
    /// present, is ambiguous with that port as the one candidate; none present
    /// opens absent.
    #[test]
    fn a_saved_device_binds_when_it_is_the_one_and_asks_when_it_is_not() {
        let (mut s, fake) = session();
        fake.plug(unit(1, "Pad In", "unit-1"), PortDirection::Input);
        fake.plug(unit(2, "Pad Out", "unit-1"), PortDirection::Output);
        fake.plug(endpoint(3, "Keys"), PortDirection::Input);
        let _ = s.update();
        let all = s.midi().devices();
        let saved = |name: &str| all.iter().find(|d| d.name == name).cloned().unwrap();
        let (pad_out, keys) = (saved("Pad Out"), saved("Keys"));

        let (pad_port, keys_port) = (
            s.open_output(pad_out.clone()).expect("opens"),
            s.open_input(keys.clone()).expect("opens"),
        );
        assert_eq!(pad_port.presence(), Presence::Live, "named by its unit");
        assert_eq!(
            keys_port.presence(),
            Presence::Live,
            "the only device of its name"
        );

        // The unit's output is gone: the saved output now names a port of a unit
        // with one other present device, so it can no longer be told for sure.
        fake.unplug(EndpointId::from_raw(2));
        let _ = s.update();
        let error = s
            .open_output(pad_out.clone())
            .expect_err("it can no longer be told for sure");
        assert!(matches!(
            &error,
            MidiPortError::Ambiguous(c) if c.len() == 1 && c[0].name == "Pad In"
        ));

        fake.unplug(EndpointId::from_raw(1));
        fake.unplug(EndpointId::from_raw(3));
        let _ = s.update();
        let gone = s.open_output(pad_out).expect("opens absent");
        assert_eq!(gone.presence(), Presence::Lost(PortCause::Unplugged));
    }

    /// **What a device says of itself is its port's info**: a device that has
    /// said nothing knows nothing, so its saved form has nothing learned and
    /// confirms as `Unknown` (never as the same unit).
    #[test]
    fn a_device_that_has_said_nothing_is_unknown() {
        use tutti_midi_device::{Learned, Match};
        let (mut s, fake) = session();
        fake.plug(endpoint(3, "Keys"), PortDirection::Input);
        let _ = s.update();
        let keys = s.midi().devices().into_iter().next().unwrap();

        let info = s.open_input(keys.clone()).expect("opens").info();
        assert!(info.learned().is_empty());
        let saved = info.saved();
        assert_eq!(saved.info(), &keys);
        assert_eq!(saved.learned(), &Learned::new());
        assert_eq!(info.confirm(&saved), Match::Unknown);
    }
}
