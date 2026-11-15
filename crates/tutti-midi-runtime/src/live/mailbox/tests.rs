use super::*;
use std::num::NonZeroUsize;
use tutti_graph::{Offset, UmpAt};

use tutti_fixtures::midi;
use tutti_midi_protocol::widen_velocity;
use tutti_ump::{MidiChannel, U7};

/// A note-on with a 7-bit velocity, widened (M2-115), at the block's first
/// frame: what a host's outbound events are.
fn note_on(note: u8, vel_u7: u8) -> UmpAt {
    (
        Offset::ZERO,
        midi::note_on(
            MidiChannel::FIRST,
            note,
            widen_velocity(U7::of(vel_u7)).get(),
        ),
    )
}

/// An event as the ring holds it: its offset as the raw stamp.
fn raw(e: UmpAt) -> (u32, Ump) {
    (e.0.get(), e.1)
}

fn note_off(note: u8) -> UmpAt {
    (Offset::ZERO, midi::note_off(MidiChannel::FIRST, note, 0))
}

#[test]
fn sender_pushes_to_receiver() {
    let (sender, receiver) = MidiMailbox::pair();
    let _ = sender.queue(&[note_on(60, 100), note_off(60)]);
    let mut buf = [(0, Ump([0; 4])); 16];
    assert_eq!(receiver.poll_into(&mut buf), 2);
    assert_eq!(buf[..2], [raw(note_on(60, 100)), raw(note_off(60))]);
}

#[test]
fn cloned_sender_pushes_to_same_receiver() {
    let (sender, receiver) = MidiMailbox::pair();
    let s2 = sender.clone();
    let _ = sender.queue(&[note_on(60, 100)]);
    let _ = s2.queue(&[note_on(64, 100)]);
    let mut buf = [(0, Ump([0; 4])); 16];
    assert_eq!(receiver.poll_into(&mut buf), 2);
}

/// A push into a ring whose receiver is gone still lands (the count says
/// so), rather than silently reporting zero.
#[test]
fn dropped_receiver_keeps_sender_pushable() {
    let (sender, receiver) = MidiMailbox::pair();
    drop(receiver);
    assert_eq!(
        sender
            .queue(&[note_on(60, 100), note_on(64, 100)])
            .packets(),
        2
    );
    assert_eq!(sender.queue_stamped(&[raw(note_on(67, 100))]).packets(), 1);
}

/// A partial accept reports the prefix that landed, not the count offered.
#[test]
fn a_partial_accept_reports_how_many_landed() {
    let (sender, _receiver) = MidiMailbox::with_capacity(NonZeroUsize::new(5).unwrap());
    assert_eq!(sender.queue(&[note_on(1, 1), note_on(2, 1)]).packets(), 2);
    let batch = [
        note_on(60, 100),
        note_on(61, 100),
        note_on(62, 100),
        note_on(63, 100),
    ];
    assert_eq!(sender.queue(&batch).packets(), 3);
}

/// A System Exclusive message of three packets, as a host sends it.
fn sysex() -> [UmpAt; 3] {
    use tutti_ump::{Message, MidiGroup, Sysex7Packet, SysexStatus};
    [SysexStatus::Start, SysexStatus::Continue, SysexStatus::End].map(|status| {
        (
            Offset::ZERO,
            Ump::from(Message::Sysex7(
                MidiGroup::FIRST,
                Sysex7Packet::new(status, &[1]).unwrap(),
            )),
        )
    })
}

/// A message of several packets goes into a ring whole or not at all: with
/// room for two of its three packets, none of it lands, and nothing after it.
#[test]
fn a_message_that_does_not_fit_whole_is_not_taken_and_nothing_after_it() {
    let (sender, receiver) = MidiMailbox::with_capacity(NonZeroUsize::new(4).unwrap());
    let _ = sender.queue(&[note_on(60, 100), note_on(61, 100)]);
    let batch = [sysex().to_vec(), vec![note_on(62, 100)]].concat();
    assert_eq!(sender.queue(&batch), Taken::NONE);
    let mut buf = [(0, Ump([0; 4])); 16];
    assert_eq!(receiver.poll_into(&mut buf), 2, "only the two notes are in");
}

/// With room for the message it lands whole, after the events before it.
#[test]
fn a_message_that_fits_lands_whole() {
    let (sender, receiver) = MidiMailbox::with_capacity(NonZeroUsize::new(4).unwrap());
    let batch = [
        vec![note_on(60, 100)],
        sysex().to_vec(),
        vec![note_on(62, 100)],
    ]
    .concat();
    assert_eq!(
        sender.queue(&batch).packets(),
        4,
        "the note after the sysex has no room"
    );
    let mut buf = [(0, Ump([0; 4])); 16];
    assert_eq!(receiver.poll_into(&mut buf), 4);
}

/// Two clones writing at once never put a packet between another's: every
/// System Exclusive message reaches the ring as its packets in a row, whatever
/// a contended write refused.
#[test]
fn clones_writing_at_once_do_not_interleave_a_messages_packets() {
    use tutti_ump::{Message, MidiGroup, Sysex7Packet, SysexStatus};
    let (sender, receiver) = MidiMailbox::with_capacity(NonZeroUsize::new(100_000).unwrap());
    let message = |group| {
        [SysexStatus::Start, SysexStatus::Continue, SysexStatus::End].map(|status| {
            (
                Offset::ZERO,
                Ump::from(Message::Sysex7(
                    MidiGroup::of(group),
                    Sysex7Packet::new(status, &[1]).unwrap(),
                )),
            )
        })
    };
    std::thread::scope(|scope| {
        for group in [1, 2] {
            let sender = sender.clone();
            scope.spawn(move || {
                for _ in 0..5_000 {
                    let _ = sender.queue(&message(group));
                }
            });
        }
    });
    let mut buf = vec![(0, Ump([0; 4])); 100_000];
    let got = receiver.poll_into(&mut buf);
    assert!(got > 0 && got % 3 == 0);
    for run in buf[..got].chunks(3) {
        let groups: Vec<_> = run.iter().map(|(_, ump)| ump.group()).collect();
        assert!(
            groups.windows(2).all(|w| w[0] == w[1]),
            "interleaved: {groups:?}"
        );
    }
}
