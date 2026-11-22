use tutti_graph::Offset;
use tutti_midi_runtime::SendAt;
use tutti_ump::{Message, MidiGroup, Sysex7Packet, SysexStatus, Ump};

use super::*;

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

/// A complete packet, then a message of three, then a complete packet.
fn batch() -> [Outgoing; 5] {
    use SysexStatus::{Complete, Continue, End, Start};
    [
        packet(Complete),
        packet(Start),
        packet(Continue),
        packet(End),
        packet(Complete),
    ]
}

/// The OS takes `fits` messages and refuses the next, with `sent` of its
/// packets out.
fn os(
    fits: usize,
    sent: usize,
) -> impl FnMut(PacketSpan<'_, Outgoing>) -> Result<(), Refused<crate::MidiHardwareError>> {
    let mut seen = 0;
    move |_| {
        seen += 1;
        if seen <= fits {
            Ok(())
        } else {
            Err(Refused {
                error: crate::MidiHardwareError::OutputPoolFull { free: 0, needed: 1 },
                sent,
            })
        }
    }
}

#[test]
fn every_message_the_os_takes_is_taken() {
    let sent = by_message(&batch(), "test", os(3, 0));
    assert_eq!(
        sent,
        Sent {
            taken: Taken::within(&batch(), 5),
            losses: Losses::NONE
        }
    );
}

#[test]
fn a_message_the_os_refuses_whole_is_not_taken_and_nothing_after_it() {
    let sent = by_message(&batch(), "test", os(1, 0));
    assert_eq!(sent.taken.packets(), 1);
    assert_eq!(sent.losses, Losses::NONE);
}

#[test]
fn a_message_the_os_cut_is_taken_and_its_tail_is_lost() {
    let sent = by_message(&batch(), "test", os(1, 2));
    assert_eq!(sent.taken.packets(), 4, "the cut message is consumed");
    assert_eq!(sent.losses, Losses::lost(1), "its third packet did not go");
}
