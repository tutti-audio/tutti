//! The session follows MIDI Machine Control (RP-013) from a fake device, and
//! sends it from its transport to an output.
//!
//! The oracles are the specification's: the command bytes of RP-013 pp.15 and
//! 20 to 43, the device ID addressing of p.5, the time code of p.8 (a time of
//! `00:00:02:00` is 2 s, beat 4 at 120 bpm; drop-frame labels skip two a minute)
//! and the time code a locate states.

use crate::common::OpenPorts as _;
use crate::common::Wire;
use crate::common::{Keyed, blocks, next_block};
use std::sync::Arc;

use tutti_midi_device::EndpointId;
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint};
use tutti_midi_protocol::{MtcRate, Recipient, SysexDeviceId};
use tutti_midi_runtime::SyncMessages;
use tutti_session::ClockKind;
use tutti_session::{
    FollowClock, FollowKind, FollowMachineControl, MidiError, MidiEvent, Session, SessionEvent,
    UnfollowCause,
};
use tutti_timeline::FadeOut;
use tutti_ump::{Message, MidiGroup, Sysex7Packets};
use tutti_units::{Beat, SampleRate};

const RATE: f64 = 48_000.0;
const KEYS: u64 = 1;
const G: MidiGroup = MidiGroup::FIRST;
const OURS: SysexDeviceId = SysexDeviceId::of(5);

fn rig() -> (Keyed, FakeCounters) {
    let (backend, fake) = FakeBackend::build_with(vec![endpoint(KEYS, "Keys")], vec![], usize::MAX);
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    let _ = s.update();
    let keys = s.midi().devices().remove(0);
    let keys = s.open_input(keys).expect("opens");
    (Keyed::new(s, keys), fake)
}

fn follow(s: &mut Keyed) {
    let input = s.input();
    s.midi()
        .follow_machine_control(FollowMachineControl::new(input, G, OURS))
        .expect("follows");
    let _ = s.update();
}

/// Sends `F0 7F <device> 06 <commands> F7`.
fn command(fake: &FakeCounters, device: u8, commands: &[u8]) {
    let mut payload = vec![0x7F, device, 0x06];
    payload.extend_from_slice(commands);
    for ump in Sysex7Packets::from_payload(G, &payload).expect("a 7-bit payload") {
        assert!(fake.send_input(EndpointId::from_raw(KEYS), ump, next_block()));
    }
}

fn pump(s: &mut Session) -> Vec<MidiEvent> {
    blocks(s, 1);
    s.update()
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::Midi(e) => Some(e),
            _ => None,
        })
        .collect()
}

/// Pumps until the playhead's rolling state is `rolling`, at most 50 times.
fn until_rolling(s: &mut Session, rolling: bool) {
    for _ in 0..50 {
        pump(s);
        if s.playhead().playing() == rolling {
            return;
        }
    }
    panic!("the transport never became rolling = {rolling}");
}

/// Pumps until the playhead is at `beat`, at most 50 times.
fn until_beat(s: &mut Session, beat: f64) {
    for _ in 0..50 {
        pump(s);
        if (s.playhead().beat().get() - beat).abs() < 1e-4 {
            return;
        }
    }
    panic!("beat {:?}, not {beat}", s.playhead().beat());
}

/// **PLAY plays and STOP stops** (RP-013 p.20: `02` and `01`), to the device ID
/// the follow was given.
#[test]
fn play_plays_and_stop_stops_a_transport_addressed_by_id() {
    let (mut s, fake) = rig();
    follow(&mut s);
    command(&fake, 5, &[0x02]);
    until_rolling(&mut s, true);
    command(&fake, 5, &[0x01]);
    until_rolling(&mut s, false);
}

/// **Off by default**: without a follow a PLAY changes nothing.
#[test]
fn without_a_follow_a_play_changes_nothing() {
    let (mut s, fake) = rig();
    command(&fake, 5, &[0x02]);
    for _ in 0..40 {
        pump(&mut s);
        assert!(!s.playhead().playing());
    }
}

/// **Addressing (RP-013 p.5)**: a command to another device is not ours; the
/// all-call (`7F`) always is, whatever ID we have.
#[test]
fn another_device_is_not_followed_and_the_all_call_is() {
    let (mut s, fake) = rig();
    follow(&mut s);
    command(&fake, 6, &[0x02]);
    for _ in 0..40 {
        pump(&mut s);
        assert!(!s.playhead().playing(), "a command to device 6");
    }
    command(&fake, 0x7F, &[0x02]);
    until_rolling(&mut s, true);
}

/// **A group (RP-013 pp.5 and 39)**: GROUP [ASSIGN] to the all-call puts device 5
/// in group `21`; a PAUSE to the group then stops the transport; after GROUP
/// [DIS-ASSIGN] of every group the same PLAY to the group is not ours.
#[test]
fn a_group_assigned_to_us_is_followed_until_it_is_dis_assigned() {
    let (mut s, fake) = rig();
    follow(&mut s);
    command(&fake, 0x7F, &[0x52, 0x03, 0x00, 0x21, 0x05]);
    command(&fake, 0x21, &[0x02]);
    until_rolling(&mut s, true);
    command(&fake, 0x21, &[0x09]);
    until_rolling(&mut s, false);
    command(&fake, 0x7F, &[0x52, 0x03, 0x01, 0x7F, 0x7F]);
    command(&fake, 0x21, &[0x02]);
    for _ in 0..40 {
        pump(&mut s);
        assert!(!s.playhead().playing(), "no longer in the group");
    }
}

/// **LOCATE [TARGET]** (RP-013 pp.28 and 29) to `00:00:02:00` at 30 frames
/// is 2 s, beat 4 at the project's 120 bpm; the transport stays stopped.
#[test]
fn a_locate_target_moves_a_stopped_transport_to_its_time() {
    let (mut s, fake) = rig();
    follow(&mut s);
    command(&fake, 5, &[0x44, 0x06, 0x01, 0x60, 0x00, 0x02, 0x00, 0x00]);
    until_beat(&mut s, 4.0);
    assert!(!s.playhead().playing());
}

/// **A drop-frame label skips two frames a minute** (RP-013 p.10): `00:01:00:02`
/// at 30 drop is frame 1800, which is 1800 x 1001 / 30000 = 60.06 s, beat 120.12.
#[test]
fn a_drop_frame_locate_counts_the_dropped_labels() {
    let (mut s, fake) = rig();
    follow(&mut s);
    command(&fake, 5, &[0x44, 0x06, 0x01, 0x40, 0x01, 0x00, 0x02, 0x00]);
    until_beat(&mut s, 120.12);
}

/// **Subframes are hundredths of a frame** (RP-013 p.8): `00:00:02:00.50` at 25
/// frames is 2.02 s, beat 4.04.
#[test]
fn subframes_add_their_hundredths_of_a_frame() {
    let (mut s, fake) = rig();
    follow(&mut s);
    command(&fake, 5, &[0x44, 0x06, 0x01, 0x20, 0x00, 0x02, 0x00, 50]);
    until_beat(&mut s, 4.04);
}

/// **LOCATE then DEFERRED PLAY in one message** (RP-013 p.76) is one motion:
/// the transport plays from the time, not from where it was.
#[test]
fn a_locate_and_a_deferred_play_roll_from_the_time() {
    let (mut s, fake) = rig();
    follow(&mut s);
    command(
        &fake,
        5,
        &[0x44, 0x06, 0x01, 0x60, 0x00, 0x0A, 0x00, 0x00, 0x03],
    );
    until_rolling(&mut s, true);
    let beat = s.playhead().beat().get();
    assert!((beat - 20.0).abs() < 1.0, "10 s is beat 20, at {beat}");
}

/// **LOCATE [I/F] reads a register WRITE loaded** (RP-013 p.28): GP0 written
/// `00:00:01:00`, then a locate to `08` goes to beat 2.
#[test]
fn a_locate_by_register_goes_where_a_write_put_the_register() {
    let (mut s, fake) = rig();
    follow(&mut s);
    command(
        &fake,
        5,
        &[
            0x40, 0x06, 0x08, 0x60, 0x00, 0x01, 0x00, 0x00, 0x44, 0x02, 0x00, 0x08,
        ],
    );
    until_beat(&mut s, 2.0);
}

/// **A command with no counterpart is reported, not dropped silently**: RECORD
/// STROBE (`06`) is a `MachineControlIgnored`, and the transport does not move.
#[test]
fn a_command_the_transport_cannot_do_is_reported() {
    let (mut s, fake) = rig();
    follow(&mut s);
    command(&fake, 5, &[0x06]);
    let mut seen = Vec::new();
    for _ in 0..40 {
        seen.extend(pump(&mut s));
    }
    assert!(
        seen.iter().any(|e| matches!(
            e,
            MidiEvent::MachineControlIgnored(tutti_midi_protocol::MmcCommand::RecordStrobe)
        )),
        "{seen:?}"
    );
    assert!(!s.playhead().playing());
}

/// **A segmented string acts whole, in order, and a segment that does not belong
/// is reported (RP-013 p.40)**: a later segment with no first is a
/// `MachineControlSegmentation(MissingFirst)`; then, from a rolling transport, a
/// LOCATE to beat 4.0 (`44 06 01 60 00 02 00 00`) and a STOP cut across two
/// COMMAND SEGMENTs do nothing until the last arrives, and then both run: the
/// transport stands at beat 4.0, stopped.
#[test]
fn segmented_commands_act_whole_and_a_stray_segment_is_reported() {
    use tutti_midi_runtime::SegmentFault;
    let (mut s, fake) = rig();
    follow(&mut s);
    command(&fake, 5, &[0x53, 0x02, 0x00, 0x02]);
    let mut seen = Vec::new();
    for _ in 0..40 {
        seen.extend(pump(&mut s));
    }
    assert!(
        seen.iter().any(|e| matches!(
            e,
            MidiEvent::MachineControlSegmentation(SegmentFault::String(
                tutti_midi_protocol::MmcSegmentError::MissingFirst
            ))
        )),
        "{seen:?}"
    );
    command(&fake, 5, &[0x02]);
    until_rolling(&mut s, true);
    let string = [0x44, 0x06, 0x01, 0x60, 0x00, 0x02, 0x00, 0x00, 0x01];
    let mut first = vec![0x53, 0x05, 0x41];
    first.extend_from_slice(&string[..4]);
    command(&fake, 5, &first);
    for _ in 0..40 {
        pump(&mut s);
    }
    assert!(s.playhead().playing(), "half a string does nothing");
    let mut last = vec![0x53, 0x06, 0x00];
    last.extend_from_slice(&string[4..]);
    command(&fake, 5, &last);
    until_rolling(&mut s, false);
    until_beat(&mut s, 4.0);
}

/// **MMC follows beside a Beat Clock follower** and takes no part of the tempo
/// map: following both is allowed, and a PLAY still plays.
#[test]
fn machine_control_follows_beside_a_beat_clock_and_owns_no_tempo() {
    let (mut s, fake) = rig();
    let input = s.input();
    s.midi()
        .follow_clock(FollowClock::new(input, G, ClockKind::BeatClock))
        .expect("follows the clock");
    follow(&mut s);
    command(&fake, 5, &[0x02]);
    until_rolling(&mut s, true);
}

/// **Unfollowing** reports and leaves the transport as it was; unfollowing
/// again is refused by name.
#[test]
fn unfollowing_is_reported_and_refused_when_not_following() {
    let (mut s, _fake) = rig();
    assert!(matches!(
        s.midi().unfollow_machine_control(),
        Err(MidiError::NotFollowingMachineControl)
    ));
    follow(&mut s);
    s.midi().unfollow_machine_control().expect("was following");
    let events = pump(&mut s);
    assert!(
        events.iter().any(|e| matches!(
            e,
            MidiEvent::Unfollowed {
                follower: FollowKind::MachineControl,
                cause: UnfollowCause::Requested
            }
        )),
        "{events:?}"
    );
}

/// **A Group the input does not hear is refused** at the call.
#[test]
fn a_group_the_input_does_not_hear_is_refused() {
    let (mut s, _fake) = rig();
    let far = MidiGroup::of(9);
    let input = s.input();
    assert!(matches!(
        s.midi().follow_machine_control(FollowMachineControl::new(input, far, OURS)),
        Err(MidiError::GroupNotHeard(g)) if g == far
    ));
}

// --- Sending ---

/// The SysEx payloads sent since the last call, one per message.
fn messages(wire: &Wire) -> Vec<Vec<u8>> {
    let mut messages = Vec::new();
    let mut current = Vec::new();
    for (_, ump) in wire.batches().into_iter().flatten() {
        let Ok(Message::Sysex7(_, packet)) = ump.decode() else {
            continue;
        };
        current.extend(packet.bytes().iter().map(|b| b.get()));
        if matches!(
            packet.status(),
            tutti_ump::SysexStatus::Complete | tutti_ump::SysexStatus::End
        ) {
            messages.push(std::mem::take(&mut current));
        }
    }
    messages
}

fn sender(wire: &Wire, to: Recipient) -> (Session, tutti_session::MidiSink) {
    let mut s = Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds");
    let _ = s.update();
    let sink = s.open_midi_sink(Arc::new(wire.clone())).open();
    let _ = s.update();
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(SyncMessages {
            mmc: Some(to),
            rate: MtcRate::Fps30,
            ..SyncMessages::OFF
        });
    blocks(&mut s, 1);
    let _ = s.update();
    (s, sink)
}

/// **The transport's play, locate and stop go out as MMC** (RP-013 pp.20 and
/// 28): PLAY is `7F <id> 06 02`; a locate while rolling is LOCATE [TARGET]
/// `44 06 01 60 00 02 00 00` (2 s at 30 frames) with DEFERRED PLAY `03`;
/// STOP is `01`. The first block after the setting sends nothing.
#[test]
fn the_transport_is_sent_as_play_locate_and_stop() {
    let wire = Wire::default();
    let (mut s, _sink) = sender(&wire, Recipient::Device(SysexDeviceId::of(3)));
    assert!(messages(&wire).is_empty(), "nothing before anything moves");

    s.play().unwrap();
    blocks(&mut s, 1);
    let _ = s.update();
    assert_eq!(messages(&wire), [vec![0x7F, 0x03, 0x06, 0x02]]);

    s.locate(Beat::new(4.0)).unwrap();
    blocks(&mut s, 1);
    let _ = s.update();
    assert_eq!(
        messages(&wire),
        [vec![
            0x7F, 0x03, 0x06, 0x44, 0x06, 0x01, 0x60, 0x00, 0x02, 0x00, 0x00, 0x03
        ]]
    );

    // Rolling on in order sends nothing.
    blocks(&mut s, 3);
    let _ = s.update();
    assert!(messages(&wire).is_empty(), "no change, no message");

    s.stop(FadeOut::Immediate).unwrap();
    blocks(&mut s, 1);
    let _ = s.update();
    assert_eq!(messages(&wire), [vec![0x7F, 0x03, 0x06, 0x01]]);
}

/// **A locate of a stopped transport is LOCATE alone**, to the all-call.
#[test]
fn a_locate_while_stopped_sends_a_locate_and_no_play() {
    let wire = Wire::default();
    let (mut s, _sink) = sender(&wire, Recipient::AllCall);
    s.locate(Beat::new(4.0)).unwrap();
    blocks(&mut s, 1);
    let _ = s.update();
    assert_eq!(
        messages(&wire),
        [vec![
            0x7F, 0x7F, 0x06, 0x44, 0x06, 0x01, 0x60, 0x00, 0x02, 0x00, 0x00
        ]]
    );
}

/// **A locate to a time no time code names sends nothing and is counted**
/// (RP-013 p.8: hours are 0 to 23): a day of beats out is refused by the
/// sync node, and its refused count rises by exactly the one look; standing
/// there afterwards adds nothing.
#[test]
fn a_locate_no_time_code_names_is_counted_and_sends_nothing() {
    let wire = Wire::default();
    let (mut s, _sink) = sender(&wire, Recipient::AllCall);
    let refused = |s: &Session| s.midi_nodes().unwrap().sync_controls.losses().refused;
    let before = refused(&s);
    s.locate(Beat::new(4_000_000.0)).unwrap();
    blocks(&mut s, 1);
    let _ = s.update();
    assert!(messages(&wire).is_empty());
    assert_eq!(refused(&s), before + 1);
    blocks(&mut s, 1);
    let _ = s.update();
    assert_eq!(refused(&s), before + 1);
}

/// **Off by default and stoppable**: with nothing set nothing is sent; set back
/// to nothing the transport's motion is not sent.
#[test]
fn nothing_is_sent_unless_asked_and_it_can_be_stopped() {
    let wire = Wire::default();
    let (mut s, _sink) = sender(&wire, Recipient::AllCall);
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(SyncMessages::OFF);
    s.play().unwrap();
    blocks(&mut s, 2);
    let _ = s.update();
    assert!(messages(&wire).is_empty());
}
