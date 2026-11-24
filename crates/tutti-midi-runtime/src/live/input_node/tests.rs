use super::*;
use tutti_fixtures::midi;

use std::collections::BTreeSet;
use tutti_ump::{NoteVelocity, U7, Utility};

/// The node adopts the mode last published to its cell on its next block.
#[test]
fn the_node_adopts_the_mode_last_published() {
    let lower = tutti_fixtures::midi::lower_zone(15);
    let upper = tutti_fixtures::midi::upper_zone(7);
    let cell = Publisher::new(MpeMode::Disabled);
    let mut node = MidiInputNode::new().mpe(&cell);
    assert_eq!(node.device.mode, MpeMode::Disabled);
    cell.publish(lower);
    cell.publish(upper);

    let mode = node.mpe_mode.as_mut().map(|reader| *reader.lend());
    assert!(node.device.adopt(mode), "a new mode is adopted");
    assert_eq!(node.device.mode, upper);
    let mode = node.mpe_mode.as_mut().map(|reader| *reader.lend());
    assert!(
        !node.device.adopt(mode),
        "the mode in force is not adopted again"
    );
}

/// **A request the node held across the next `set_mpe_profile` is freed by
/// the next `reclaim`**, with no further publish.
#[test]
fn reclaim_frees_a_request_a_block_held_across_the_next() {
    let mut node = MidiInputNode::new();
    let controls = MidiInputControls {
        request: node.mpe_request_cell.clone(),
        serial: Arc::clone(&node.request_serial),
        dropped: node.dropped_reader.clone(),
    };
    {
        // A block in progress across the request.
        let _held = node.mpe_request.lend();
        controls.set_mpe_profile(
            MidiGroup::FIRST,
            Some(tutti_midi_protocol::ProfileZones::single(
                tutti_midi_protocol::ProfileZone::new(
                    tutti_ump::MpeZoneSpan::new(MidiChannel::of(0), tutti_ump::MpeChannels::of(4))
                        .unwrap(),
                    tutti_midi_protocol::PitchBendSensitivity::MPE_PROFILE_DEFAULT,
                ),
            )),
        );
    }
    assert_eq!(controls.request.retired_len(), 1, "pinned until a scan");
    assert_eq!(tutti_rt::Reclaim::reclaim(&controls), 0);
    assert_eq!(controls.request.retired_len(), 0, "freed by the reclaim");
}

const NODE: NodeKey = NodeKey(1);

/// A listener on one channel of a Group takes that channel's port and the
/// Group's channelless one; an omni listener of a Group takes its seventeen.
#[test]
fn a_listener_takes_its_channel_and_the_channelless_port() {
    let first = MidiInputPorts::FIRST_GROUP;
    let one: Vec<_> = first
        .listening(NODE, Some(MidiGroup::FIRST), Some(MidiChannel::of(3)))
        .collect();
    assert_eq!(
        one,
        vec![
            first
                .channel(NODE, MidiGroup::FIRST, MidiChannel::of(3))
                .unwrap(),
            first.channelless(NODE, MidiGroup::FIRST).unwrap()
        ]
    );
    let all: Vec<_> = first.listening(NODE, None, None).collect();
    assert_eq!(all.len(), usize::from(first.count()));
    let every: BTreeSet<_> = MidiChannel::all()
        .map(|c| first.channel(NODE, MidiGroup::FIRST, c).unwrap())
        .chain(first.channelless(NODE, MidiGroup::FIRST))
        .collect();
    assert_eq!(all.into_iter().collect::<BTreeSet<_>>(), every);
}

/// The output of `ports`' shape an event leaves from.
fn leaves(ports: MidiInputPorts, event: &Ump) -> Option<OutEvents> {
    let shape = MidiInputNode::declared(ports, MIDI_INPUT_PORT_CAPACITY);
    ports.output_of(&shape, event)
}

/// The output of `ports`' shape that is the port `port` names.
fn out_of(ports: MidiInputPorts, port: Option<EventOutputPort>) -> Option<OutEvents> {
    let shape = MidiInputNode::declared(ports, MIDI_INPUT_PORT_CAPACITY);
    port.and_then(|p| shape.event_output(p.port()))
}

/// A voice message leaves on its channel's port; a system message on
/// the channelless one.
#[test]
fn events_leave_on_their_channels_port() {
    let first = MidiInputPorts::FIRST_GROUP;
    // 7-bit velocity 100, upscaled (M2-115).
    let on = midi::note_on(
        MidiChannel::of(9),
        60,
        NoteVelocity::from(U7::of(100)).get(),
    );
    let nine = first.channel(NODE, MidiGroup::FIRST, MidiChannel::of(9));
    assert_eq!(leaves(first, &on), out_of(first, nine));
    let clock = Ump::from(Message::System(
        tutti_ump::MidiGroup::FIRST,
        System::TimingClock,
    ));
    let channelless = first.channelless(NODE, MidiGroup::FIRST);
    assert_eq!(leaves(first, &clock), out_of(first, channelless));
}

/// **A Group is a channel space of its own** (M2-104 section 2.1.2: a Group
/// is 16 Channels; Appendix B.1: what a message does in one Group does not
/// apply to Channels in any other Group): channel 1 of Group 2 leaves from a
/// port of its own, not channel 1 of Group 1's; a Group's System message
/// leaves from that Group's channelless port; a message with no Group
/// (Utility) from the first Group's; a Group the node was not built to hear
/// has no port.
/// Spec: m2-104/2.1.2#2
#[test]
fn a_group_has_ports_of_its_own() {
    let two = MidiInputPorts::through(MidiGroup::of(1));
    assert_eq!(two.count(), 2 * MidiInputPorts::FIRST_GROUP.count());
    let note = |group: u8, channel: u8| {
        midi::note_on(
            tutti_ump::Address::new(MidiGroup::of(group), MidiChannel::of(channel)),
            60,
            NoteVelocity::from(U7::of(100)).get(),
        )
    };
    let channel = |group: u8, channel: u8| {
        out_of(
            two,
            two.channel(NODE, MidiGroup::of(group), MidiChannel::of(channel)),
        )
    };
    let channelless = |group: u8| out_of(two, two.channelless(NODE, MidiGroup::of(group)));
    assert_eq!(leaves(two, &note(0, 1)), channel(0, 1));
    assert_eq!(leaves(two, &note(1, 1)), channel(1, 1));
    assert_ne!(channel(0, 1), channel(1, 1), "a port of its own");
    // The layout: 16 channel ports then the channelless one, per Group.
    let port = |p: Option<EventOutputPort>| p.map(EventOutputPort::port);
    assert_eq!(
        port(two.channel(NODE, MidiGroup::of(1), MidiChannel::of(1))),
        Some(18)
    );
    assert_eq!(port(two.channelless(NODE, MidiGroup::of(0))), Some(16));
    assert_eq!(port(two.channelless(NODE, MidiGroup::of(1))), Some(33));
    assert_eq!(
        leaves(
            two,
            &Ump::from(Message::System(MidiGroup::of(1), System::TimingClock))
        ),
        channelless(1)
    );
    // A Utility NOOP has no Group.
    assert_eq!(
        leaves(two, &Ump::from(Message::Utility(Utility::Noop))),
        channelless(0)
    );
    assert_eq!(leaves(two, &note(2, 1)), None, "not heard");
    // A listener on Group 2's channel 1 takes that Group's ports only.
    let ports: Vec<_> = two
        .listening(NODE, Some(MidiGroup::of(1)), Some(MidiChannel::of(1)))
        .collect();
    assert_eq!(
        ports,
        vec![
            two.channel(NODE, MidiGroup::of(1), MidiChannel::of(1))
                .unwrap(),
            two.channelless(NODE, MidiGroup::of(1)).unwrap()
        ]
    );
    // One that names a Group not heard takes nothing.
    assert_eq!(two.listening(NODE, Some(MidiGroup::of(2)), None).count(), 0);
}

/// How many events a following node sends at the block in which a device
/// arrives, after it was sent a note-on and was `reset` or not.
fn sent_at_arrival(reset: bool) -> usize {
    use tutti_graph::Event;
    use tutti_graph::contract::Direct;
    let (mut writer, generation) = DeviceGeneration::new();
    let node = MidiInputNode::new().following(&generation);
    let mut d = Direct::new(node, SampleRate::SR_48K, 64);
    let shape = tutti_graph::Node::<f32>::shape(&*d.node);
    let input = shape.event_input(0).expect("an event input");
    let on = midi::note_on(
        MidiChannel::of(0),
        60,
        NoteVelocity::from(U7::of(100)).get(),
    );
    d.events(input, &[Event::midi(Offset::ZERO, on)]);
    d.block();
    if reset {
        tutti_graph::Node::<f32>::reset(&mut *d.node);
    }
    d.events(input, &[]);
    writer.arrive();
    d.block();
    shape.event_outputs().map(|p| d.event_output(p).len()).sum()
}

/// **`reset` returns the node to a freshly prepared one's state**: a device
/// that arrives releases the notes the one before held, and a reset node
/// holds none (the executor closed what a parked node held downstream).
#[test]
fn reset_forgets_the_notes_the_device_held() {
    assert_eq!(sent_at_arrival(false), 1, "the held note is released");
    assert_eq!(sent_at_arrival(true), 0, "a reset node holds nothing");
}
