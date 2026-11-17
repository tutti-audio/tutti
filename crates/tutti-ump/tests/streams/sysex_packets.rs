//! `Sysex7Packets` (`SysexMessage::packets` and `checked`): one payload as SysEx7 packets
//! (M2-104 section 7.7), and `Sysex7Packet::data`.

use crate::support::payload;
use tutti_ump::{
    Message, MidiGroup, Sysex7Packet, Sysex7Packets, SysexAssembler, SysexError, SysexMessage,
    SysexPart, SysexStatus, SysexStep, U7, Ump,
};

/// The packets of a payload of 7-bit values: the plain-bytes door.
fn sysex7_packets(group: MidiGroup, payload: &[u8]) -> Sysex7Packets<'_> {
    Sysex7Packets::from_payload(group, payload).expect("7-bit bytes")
}

fn raw(payload: &[u8]) -> Vec<u8> {
    payload.to_vec()
}

/// Each packet as its status and bytes, read back through `decode`.
fn decoded(group: MidiGroup, p: &[u8]) -> Vec<(SysexStatus, Vec<u8>)> {
    sysex7_packets(group, p)
        .map(|ump| {
            let Ok(Message::Sysex7(g, packet)) = ump.decode() else {
                panic!("not a SysEx7 packet");
            };
            assert_eq!(g, group);
            (packet.status(), packet.data().to_vec())
        })
        .collect()
}

use SysexStatus::{Complete, Continue, End, Start};

/// Up to six bytes is one `Complete`; empty is one `Complete` of no bytes.
/// Spec: m2-104/7.7#1
#[test]
fn a_short_payload_is_one_complete_packet() {
    assert_eq!(decoded(MidiGroup::of(0), &[]), [(Complete, vec![])]);
    let one = payload(1);
    assert_eq!(decoded(MidiGroup::of(0), &one), [(Complete, raw(&one))]);
    let six = payload(6);
    assert_eq!(decoded(MidiGroup::of(0), &six), [(Complete, raw(&six))]);
}

/// Seven bytes: `Start` with six, `End` with one.
/// Spec: m2-104/7.7#1
#[test]
fn seven_bytes_are_start_and_end() {
    let p = payload(7);
    let r = raw(&p);
    assert_eq!(
        decoded(MidiGroup::of(0), &p),
        [(Start, r[..6].to_vec()), (End, r[6..].to_vec())]
    );
}

/// Twelve bytes: `Start` and `End`, six each; thirteen: a `Continue` between.
/// Spec: m2-104/7.7#1, m2-104/7.7#2
#[test]
fn twelve_and_thirteen_bytes() {
    let p = payload(12);
    let r = raw(&p);
    assert_eq!(
        decoded(MidiGroup::of(0), &p),
        [(Start, r[..6].to_vec()), (End, r[6..].to_vec())]
    );
    let p = payload(13);
    let r = raw(&p);
    assert_eq!(
        decoded(MidiGroup::of(0), &p),
        [
            (Start, r[..6].to_vec()),
            (Continue, r[6..12].to_vec()),
            (End, r[12..].to_vec())
        ]
    );
}

/// A long payload: `Start`, `Continue`*, `End`, six bytes each but the last,
/// and nothing else between the `Start` and the `End`.
/// Spec: m2-104/7.7#1, m2-104/7.7#2, m2-104/7.7.1#1, m2-104/7.7.1#2
#[test]
fn a_long_payload_runs_start_continue_end() {
    let p = payload(1000);
    let got = decoded(MidiGroup::of(0), &p);
    assert_eq!(got.len(), 1000usize.div_ceil(6));
    assert_eq!(got[0].0, Start);
    assert_eq!(got.last().unwrap().0, End);
    assert!(got[1..got.len() - 1].iter().all(|(s, _)| *s == Continue));
    assert!(got[..got.len() - 1].iter().all(|(_, b)| b.len() == 6));
    assert_eq!(got.last().unwrap().1.len(), 1000 - 6 * 166);
    let joined: Vec<u8> = got.into_iter().flat_map(|(_, b)| b).collect();
    assert_eq!(joined, raw(&p));
}

/// `len()` is the packet count at every point, and the iterator is fused.
#[test]
fn len_counts_the_packets_left_and_the_iterator_is_fused() {
    for len in 0..=40 {
        let p = payload(len);
        let mut it = sysex7_packets(MidiGroup::of(0), &p);
        let mut left = len.div_ceil(6).max(1);
        assert_eq!(it.len(), left, "length {len}");
        while it.next().is_some() {
            left -= 1;
            assert_eq!(it.len(), left, "length {len}");
        }
        assert_eq!(it.next(), None);
        assert_eq!(it.next(), None);
        assert_eq!(it.len(), 0);
    }
}

/// Every packet carries the group it was asked for.
#[test]
fn the_group_is_on_every_packet() {
    let p = payload(20);
    for g in [0u8, 5, 15] {
        let group = MidiGroup::of(g);
        let umps: Vec<Ump> = sysex7_packets(group, &p).collect();
        assert_eq!(umps.len(), 4);
        for ump in umps {
            assert_eq!((ump.words()[0] >> 24) & 0xF, u32::from(g));
            let Ok(Message::Sysex7(got, _)) = ump.decode() else {
                panic!()
            };
            assert_eq!(got, group);
        }
    }
}

/// The writer's packets assemble back into the framed message, for every
/// length, through `SysexMessage::packets`.
#[test]
fn the_assembler_gives_back_the_message() {
    for len in 0..=40 {
        let p = payload(len);
        let mut framed = vec![0xF0];
        framed.extend(raw(&p));
        framed.push(0xF7);
        let message = SysexMessage::from_framed(&framed).unwrap();
        let group = MidiGroup::of(3);

        let mut asm = SysexAssembler::new(vec![0u8; 128], 64, 1);
        asm.begin_block();
        let mut out = None;
        for ump in message.packets(group) {
            if let SysexStep::Complete(r) = asm.push(ump) {
                assert!(out.replace(asm.message(r).bytes().to_vec()).is_none());
            }
        }
        assert_eq!(out.as_deref(), Some(framed.as_slice()), "length {len}");
        assert_eq!(asm.dropped(), 0);
        // The two entry points agree.
        assert!(message.packets(group).eq(sysex7_packets(group, &p)));
    }
}

/// `data` is the same bytes as `bytes`.
#[test]
fn data_is_bytes_as_plain_bytes() {
    for len in 0..=6 {
        let p = payload(len);
        let packet = Sysex7Packet::new(Complete, &raw(&p)).unwrap();
        assert_eq!(packet.data().len(), len);
        assert!(
            packet
                .data()
                .iter()
                .zip(packet.bytes())
                .all(|(a, b)| *a == b.get())
        );
    }
}

/// A `SysexRef` made by hand reads back its range and length.
#[test]
fn a_ref_made_by_hand_names_its_range() {
    let r = tutti_ump::SysexRef::new(10, 4);
    assert_eq!(r.range(), 10..14);
    assert_eq!(r.len(), 4);
    assert!(!r.is_empty());
}

/// The words of a single packet from M2-104 Table 18: type 3, group, status 0,
/// byte count, then the bytes.
/// Spec: m2-104/msg/3/0
#[test]
fn a_single_packet_is_the_spec_words() {
    let words = |group, payload: &[u8]| {
        let mut packets = sysex7_packets(MidiGroup::of(group), payload);
        assert_eq!(packets.len(), 1);
        packets.next().unwrap().words().to_vec()
    };
    assert_eq!(
        words(3, &[0x7E, 0x7F, 0x06, 0x01]),
        [0x3304_7E7F, 0x0601_0000]
    );
    assert_eq!(words(0, &[]), [0x3000_0000, 0]);
    assert_eq!(words(15, &[1, 2, 3, 4, 5, 6]), [0x3F06_0102, 0x0304_0506]);
}

/// `Sysex7Packets::from_payload` refuses a byte of 128 or more instead of masking
/// it, naming its index, and accepts every 7-bit payload.
#[test]
fn a_high_bit_is_refused_not_masked() {
    assert_eq!(
        Sysex7Packets::from_payload(MidiGroup::of(0), &[1, 0x80, 2]).unwrap_err(),
        SysexError::NotSevenBit { index: 1 }
    );
    assert_eq!(
        Sysex7Packets::from_payload(MidiGroup::of(0), &[0xF7]).unwrap_err(),
        SysexError::NotSevenBit { index: 0 }
    );
    assert!(Sysex7Packets::from_payload(MidiGroup::of(0), &[0x7F]).is_ok());
    for len in 0..=20 {
        assert!(Sysex7Packets::from_payload(MidiGroup::of(2), &raw(&payload(len))).is_ok());
    }
}

/// Section 7.7, `# of bytes`: the bytes after the valid ones are zero, and a
/// packet may hold fewer than six (the last of a message, any length).
/// Spec: m2-104/7.7#3, m2-104/7.7#4
#[test]
fn unused_bytes_of_a_packet_are_zero() {
    for len in 0..=13 {
        let body = vec![0x7F; len];
        for ump in sysex7_packets(MidiGroup::of(0), &body) {
            let count = ((ump.0[0] >> 16) & 0xF) as usize;
            assert!(count <= 6);
            let bytes: Vec<u8> = [
                ump.0[0] >> 8,
                ump.0[0],
                ump.0[1] >> 24,
                ump.0[1] >> 16,
                ump.0[1] >> 8,
                ump.0[1],
            ]
            .iter()
            .map(|w| *w as u8)
            .collect();
            assert!(bytes[..count].iter().all(|b| *b == 0x7F), "{len}");
            assert!(bytes[count..].iter().all(|b| *b == 0), "{len}");
            assert_eq!(ump.0[2..], [0, 0], "{len}");
        }
    }
}

/// One part of a message as its packets' status and bytes.
fn part_packets(data: &[u8], part: SysexPart) -> Vec<(SysexStatus, Vec<u8>)> {
    let data: Vec<U7> = data.iter().map(|&b| U7::new(b).expect("7-bit")).collect();
    Sysex7Packets::part(MidiGroup::of(0), &data, part)
        .map(|ump| {
            let Ok(Message::Sysex7(_, packet)) = ump.decode() else {
                panic!("not a SysEx7 packet");
            };
            (packet.status(), packet.data().to_vec())
        })
        .collect()
}

/// A message sent whole is one `Complete` of up to six bytes, none included;
/// seven bytes are a `Start` of six and an `End` of one.
/// Spec: m2-104/7.7#1, m2-104/7.7#2
#[test]
fn a_whole_message_part_is_complete_up_to_six_bytes() {
    assert_eq!(part_packets(&[], SysexPart::Only), [(Complete, vec![])]);
    let six = raw(&payload(6));
    assert_eq!(
        part_packets(&six, SysexPart::Only),
        [(Complete, six.clone())]
    );
    let seven = raw(&payload(7));
    assert_eq!(
        part_packets(&seven, SysexPart::Only),
        [(Start, seven[..6].to_vec()), (End, seven[6..].to_vec())]
    );
}

/// The first part of a divided message opens with a `Start` and never ends the
/// message; a middle part is `Continue`s; the last part closes with an `End`.
/// Spec: m2-104/7.7#1, m2-104/7.7#2
#[test]
fn the_parts_of_a_divided_message_are_start_continue_end() {
    let seven = raw(&payload(7));
    assert_eq!(
        part_packets(&seven, SysexPart::First),
        [
            (Start, seven[..6].to_vec()),
            (Continue, seven[6..].to_vec())
        ]
    );
    assert_eq!(
        part_packets(&seven, SysexPart::Middle),
        [
            (Continue, seven[..6].to_vec()),
            (Continue, seven[6..].to_vec())
        ]
    );
    assert_eq!(
        part_packets(&seven, SysexPart::Last),
        [(Continue, seven[..6].to_vec()), (End, seven[6..].to_vec())]
    );
    let three = raw(&payload(3));
    assert_eq!(
        part_packets(&three, SysexPart::First),
        [(Start, three.clone())]
    );
    assert_eq!(
        part_packets(&three, SysexPart::Middle),
        [(Continue, three.clone())]
    );
    assert_eq!(part_packets(&three, SysexPart::Last), [(End, three)]);
}

/// An empty part is one packet of no bytes, so a divided message keeps the
/// pause between its parts.
/// Spec: m2-104/7.7#1
#[test]
fn an_empty_part_is_one_packet_of_no_bytes() {
    assert_eq!(part_packets(&[], SysexPart::First), [(Start, vec![])]);
    assert_eq!(part_packets(&[], SysexPart::Middle), [(Continue, vec![])]);
    assert_eq!(part_packets(&[], SysexPart::Last), [(End, vec![])]);
}

/// `len()` of a part counts its packets.
#[test]
fn a_part_reports_its_packet_count() {
    let data: Vec<U7> = (0..13).map(U7::of).collect();
    let mut it = Sysex7Packets::part(MidiGroup::of(0), &data, SysexPart::Middle);
    assert_eq!(it.len(), 3);
    it.next();
    assert_eq!(it.len(), 2);
}
