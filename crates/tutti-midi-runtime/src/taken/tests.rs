use tutti_graph::Offset;
use tutti_ump::{Message, MidiGroup, Sysex7Packet, SysexStatus, Ump};

use super::*;
use crate::SendAt;

fn packet(status: SysexStatus) -> Outgoing {
    Outgoing::new(
        Offset::ZERO,
        SendAt::Now,
        Ump::from(Message::Sysex7(
            MidiGroup::FIRST,
            Sysex7Packet::new(status, &[1]).unwrap(),
        )),
    )
}

/// A sysex of three packets, a complete one, a sysex of two.
fn batch() -> [Outgoing; 6] {
    use SysexStatus::{Complete, Continue, End, Start};
    [
        packet(Start),
        packet(Continue),
        packet(End),
        packet(Complete),
        packet(Start),
        packet(End),
    ]
}

#[test]
fn room_inside_a_message_takes_the_messages_before_it() {
    let batch = batch();
    let at = |room| Taken::within(&batch, room).packets();
    assert_eq!(
        (0..=7).map(at).collect::<Vec<_>>(),
        [0, 0, 0, 3, 4, 4, 6, 6],
        "a count of room never ends inside a message"
    );
}

#[test]
fn a_sink_that_sends_message_by_message_takes_what_it_sent() {
    let batch = batch();
    let mut taken = Taken::NONE;
    for message in Outgoing::messages(&batch).take(2) {
        taken = taken.through(message);
    }
    assert_eq!(taken.packets(), 4);
    assert_eq!(taken.left_of(&batch), 2);
    assert!(!taken.is_all(&batch));
}
