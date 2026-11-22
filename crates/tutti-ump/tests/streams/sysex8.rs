//! System Exclusive 8 (M2-104 section 7.8): the packet shapes of Table 19, the
//! Stream ID's independence, section 7.8.1's abort and no-prohibition note,
//! and the assembler's drop counting, checked against packets built from the
//! spec's fields as well as against the writer.

use tutti_ump::{
    Data128, Message, MidiChannel, MidiGroup, Sysex7Packet, Sysex8Assembler, Sysex8Step,
    Sysex8StreamCount, Sysex8StreamIds, SysexStatus, Ump, Voice1,
};

const G0: MidiGroup = MidiGroup::of(0);

/// The packets of `payload` on stream `id`, from a receiver that takes every
/// stream: the streams below `id` are held open meanwhile.
fn written(id: u8, group: MidiGroup, payload: &[u8]) -> Vec<Ump> {
    let ids = Sysex8StreamIds::discovered(Sysex8StreamCount::new(255));
    let held: Vec<_> = (0..id)
        .map(|_| ids.open().unwrap().packets(group, &[]))
        .collect();
    let umps = ids.open().unwrap().packets(group, payload).collect();
    drop(held);
    umps
}

fn assembler(lanes: usize) -> Sysex8Assembler<Vec<u8>> {
    Sysex8Assembler::new(vec![0; 64 * lanes], 64, lanes)
}

/// A packet from the spec's bit positions (Figure 85): message type 5, group,
/// status in 23-20, `# of bytes` in 19-16, stream id in byte 3, data after.
fn raw(group: u32, status: u32, count: u32, stream: u32, data: &[u8]) -> Ump {
    let mut bytes = [0u8; 13];
    bytes[..data.len()].copy_from_slice(data);
    let w = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    Ump([
        0x5000_0000
            | (group << 24)
            | (status << 20)
            | (count << 16)
            | (stream << 8)
            | u32::from(bytes[0]),
        w(1),
        w(5),
        w(9),
    ])
}

const COMPLETE: u32 = 0;
const START: u32 = 1;
const CONTINUE: u32 = 2;
const END: u32 = 3;

/// A packet with the right `# of bytes` for `data`.
fn pkt(status: u32, stream: u32, data: &[u8]) -> Ump {
    raw(0, status, data.len() as u32 + 1, stream, data)
}

/// The messages a stream of UMPs completes.
fn feed(
    a: &mut Sysex8Assembler<Vec<u8>>,
    umps: impl IntoIterator<Item = Ump>,
) -> Vec<(u8, u8, Vec<u8>)> {
    umps.into_iter()
        .filter_map(|u| match a.push(u) {
            Sysex8Step::Complete(m) => Some((m.group.get(), m.stream_id.get(), m.payload.to_vec())),
            _ => None,
        })
        .collect()
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| 0x80 | i as u8).collect()
}

/// Section 7.8, Table 19: a payload that fits one UMP (13 bytes after the
/// Stream ID) is a Complete; otherwise a Start, Continues and an End, 13 bytes
/// each but the last. The Stream ID is in every packet, and `# of bytes`
/// counts it.
/// Spec: m2-104/7.8#1, m2-104/7.8#2
#[test]
fn packets_follow_table_19_at_boundary_sizes() {
    // (payload length, statuses, `# of bytes` of each packet)
    let cases: [(usize, &[u32], &[u32]); 8] = [
        (0, &[COMPLETE], &[1]),
        (1, &[COMPLETE], &[2]),
        (13, &[COMPLETE], &[14]),
        (14, &[START, END], &[14, 2]),
        (26, &[START, END], &[14, 14]),
        (27, &[START, CONTINUE, END], &[14, 14, 2]),
        (39, &[START, CONTINUE, END], &[14, 14, 14]),
        (40, &[START, CONTINUE, CONTINUE, END], &[14, 14, 14, 2]),
    ];
    for (len, statuses, counts) in cases {
        let data = payload(len);
        let (group, id, data) = (MidiGroup::of(3), 9, &data);
        let umps = written(id, group, data);

        let words: Vec<(u32, u32, u32)> = umps
            .iter()
            .map(|u| {
                let w = u.0[0];
                ((w >> 20) & 0xF, (w >> 16) & 0xF, (w >> 8) & 0xFF)
            })
            .collect();
        let want: Vec<(u32, u32, u32)> = statuses
            .iter()
            .zip(counts)
            .map(|(&s, &c)| (s, c, 9))
            .collect();
        assert_eq!(words, want, "{len}");
        assert!(umps.iter().all(|u| u.0[0] >> 24 == 0x53), "{len}");
    }
}

/// Split then assemble gives the original at every boundary size, whatever the
/// byte values: SysEx8 data is 8-bit.
#[test]
fn a_written_message_reads_back_whole() {
    let mut a = assembler(1);
    for len in [0, 1, 12, 13, 14, 15, 26, 27, 28, 63, 64] {
        let data = payload(len);
        let (group, id, data) = (MidiGroup::of(5), 200, &data);
        assert_eq!(
            feed(&mut a, written(id, group, data)),
            [(5, 200, data.clone())],
            "{len}"
        );
    }
    assert_eq!(a.dropped(), 0);
}

/// Section 7.8: Start and Continue with fewer than 14 bytes are not a message
/// end, and a Start or Continue with only the Stream ID is allowed.
/// Spec: m2-104/7.8#4
#[test]
fn a_short_start_or_continue_does_not_end_the_message() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            pkt(START, 0, &[1, 2, 3]),
            pkt(CONTINUE, 0, &[]),
            pkt(CONTINUE, 0, &[4]),
            pkt(END, 0, &[5]),
        ],
    );
    assert_eq!(done, [(0, 0, vec![1, 2, 3, 4, 5])]);
}

/// Section 7.8.1: an End whose `# of bytes` is 1 (the Stream ID alone, data
/// zero) closes a message whose data is valid.
/// Spec: m2-104/7.8.1#1
#[test]
fn an_end_of_one_byte_closes_a_valid_message() {
    let mut a = assembler(1);
    let done = feed(&mut a, [pkt(START, 0, &[9; 13]), raw(0, END, 1, 0, &[])]);
    assert_eq!(done, [(0, 0, vec![9; 13])]);
    assert_eq!(a.dropped(), 0);
}

/// Section 7.8.1: an End with `# of bytes` 0xF aborts the stream's message:
/// nothing is delivered and it is counted; the stream is usable after.
/// Spec: m2-104/7.8.1#1
#[test]
fn an_end_of_fifteen_aborts_the_message_and_counts_it() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            pkt(START, 4, &[1; 13]),
            pkt(CONTINUE, 4, &[2; 13]),
            raw(0, END, 0xF, 4, &[]),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 1);
    assert_eq!(feed(&mut a, [pkt(COMPLETE, 4, &[7])]), [(0, 4, vec![7])]);
    // An abort of a stream with nothing open has nothing to drop.
    feed(&mut a, [raw(0, END, 0xF, 4, &[])]);
    assert_eq!(a.dropped(), 1);
}

/// Section 7.8: `# of bytes` is 1 to 14 (the Stream ID is mandatory), 0xF only
/// in an End. A packet that breaks it is no packet: an open message goes on.
#[test]
fn a_packet_with_an_invalid_byte_count_is_ignored() {
    let mut a = assembler(1);
    let mut umps = vec![pkt(START, 0, &[1])];
    umps.push(raw(0, CONTINUE, 0, 0, &[]));
    umps.push(raw(0, CONTINUE, 0xF, 0, &[]));
    umps.push(raw(0, COMPLETE, 0, 0, &[]));
    umps.push(pkt(END, 0, &[2]));
    assert_eq!(feed(&mut a, umps), [(0, 0, vec![1, 2])]);
    assert_eq!(a.dropped(), 0);
}

/// Section 7.8, Stream id: streams of one Group interleave, each its own
/// message; so do Groups, with the same Stream ID.
#[test]
fn streams_and_groups_are_independent() {
    let mut a = assembler(4);
    let at = |group, status, stream, data: &[u8]| {
        raw(group, status, data.len() as u32 + 1, stream, data)
    };
    let done = feed(
        &mut a,
        [
            at(0, START, 0, &[1]),
            at(0, START, 1, &[10]),
            at(1, START, 0, &[100]),
            at(0, CONTINUE, 1, &[11]),
            at(0, CONTINUE, 0, &[2]),
            at(1, END, 0, &[101]),
            at(0, END, 0, &[3]),
            at(0, END, 1, &[12]),
        ],
    );
    assert_eq!(
        done,
        [
            (1, 0, vec![100, 101]),
            (0, 0, vec![1, 2, 3]),
            (0, 1, vec![10, 11, 12])
        ]
    );
    assert_eq!(a.dropped(), 0);
}

/// Section 7.8.1, note: SysEx8 may be interspersed with other UMPs, unlike
/// SysEx7 (section 7.7.1): a voice message, a SysEx7 packet and a Mixed Data Set
/// header of the same Group inside a message end nothing.
#[test]
fn other_messages_inside_a_message_end_nothing() {
    let mut a = assembler(1);
    let note = Ump::from(Message::Midi1(
        tutti_ump::Address {
            group: G0,
            channel: MidiChannel::of(0),
        },
        Voice1::NoteOn {
            note: tutti_ump::NoteNumber::of(60),
            velocity: tutti_ump::U7::of(64),
        },
    ));
    let sx7 = Ump::from(Message::Sysex7(
        G0,
        Sysex7Packet::new(SysexStatus::Complete, &[1]).unwrap(),
    ));
    let mds = Ump([0x5081_0010, 1, 0x0041_0000, 0]);
    let mut done = Vec::new();
    for ump in [pkt(START, 0, &[1]), note, sx7, mds, pkt(END, 0, &[2])] {
        match a.push(ump) {
            Sysex8Step::Complete(m) => done.push(m.payload.to_vec()),
            Sysex8Step::NotSysex8 | Sysex8Step::Held => {}
        }
    }
    assert_eq!(done, [vec![1, 2]]);
    assert_eq!(a.dropped(), 0);
}

/// Section 7.8: a message is a Complete, or a Start through an End. A Start
/// while the stream is open drops the open message, counted, and begins anew.
#[test]
fn a_start_inside_a_message_drops_it() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [pkt(START, 0, &[1]), pkt(START, 0, &[2]), pkt(END, 0, &[3])],
    );
    assert_eq!(done, [(0, 0, vec![2, 3])]);
    assert_eq!(a.dropped(), 1);
}

/// A Complete inside an open message on its stream drops the open one,
/// counted, and is itself delivered.
#[test]
fn a_complete_inside_a_message_drops_it() {
    let mut a = assembler(1);
    let done = feed(&mut a, [pkt(START, 0, &[1]), pkt(COMPLETE, 0, &[2])]);
    assert_eq!(done, [(0, 0, vec![2])]);
    assert_eq!(a.dropped(), 1);
    // And nothing is left open: an End now is an orphan.
    assert!(feed(&mut a, [pkt(END, 0, &[3])]).is_empty());
    assert_eq!(a.dropped(), 2);
}

/// Section 7.8, Stream id: a packet belongs to the open message of its own
/// stream id alone. A Continue or End of another id, while one stream is open,
/// has no message of its own to join: it is dropped, and the open message
/// ends with its own End carrying only its own bytes.
#[test]
fn a_packet_joins_only_the_message_of_its_stream_id() {
    let mut a = assembler(2);
    let done = feed(
        &mut a,
        [
            pkt(START, 3, &[1]),
            // Stream 4 has no open message: its packets are one dropped run.
            pkt(CONTINUE, 4, &[0xAA]),
            pkt(END, 4, &[0xBB]),
            pkt(END, 3, &[2]),
        ],
    );
    assert_eq!(done, [(0, 3, vec![1, 2])]);
    assert_eq!(a.dropped(), 1);
}

/// Section 7.8: a message begins with a Start. A Continue or End with none
/// open is dropped; a message's orphan packets are counted once.
#[test]
fn packets_with_no_start_are_dropped_and_counted_once() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            pkt(CONTINUE, 0, &[1]),
            pkt(CONTINUE, 0, &[2]),
            pkt(END, 0, &[3]),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 1);
    assert!(feed(&mut a, [pkt(END, 0, &[4])]).is_empty());
    assert_eq!(a.dropped(), 2);
    assert_eq!(feed(&mut a, [pkt(COMPLETE, 0, &[5])]), [(0, 0, vec![5])]);
}

/// A message longer than the buffer is dropped whole, never cut, counted once,
/// its remaining packets skipped up to its End.
#[test]
fn an_over_long_message_is_dropped_whole_and_counted_once() {
    let mut a = Sysex8Assembler::new(vec![0; 20], 20, 1);
    let data = payload(21);
    let (group, id, data) = (G0, 0, &data);
    assert!(feed_any(&mut a, written(id, group, data)).is_empty());
    assert_eq!(a.dropped(), 1);
    // The limit itself fits.
    let fits = payload(20);
    let (group, id, data) = (G0, 0, &fits);
    assert_eq!(
        feed_any(&mut a, written(id, group, data)),
        std::slice::from_ref(&fits)
    );
    assert_eq!(a.dropped(), 1);
}

fn feed_any(a: &mut Sysex8Assembler<Vec<u8>>, umps: impl IntoIterator<Item = Ump>) -> Vec<Vec<u8>> {
    feed(a, umps).into_iter().map(|(_, _, d)| d).collect()
}

/// With one lane, a second stream's Start is dropped, counted; the first
/// message is intact.
#[test]
fn a_start_with_no_free_lane_is_dropped_and_counted() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [pkt(START, 0, &[1]), pkt(START, 1, &[9]), pkt(END, 0, &[2])],
    );
    assert_eq!(done, [(0, 0, vec![1, 2])]);
    assert_eq!(a.dropped(), 1);
}

/// A message open at `lost_tail` or `reset` is dropped, counted; nothing is
/// spliced onto it afterwards.
#[test]
fn lost_tail_and_reset_drop_open_messages() {
    let mut a = assembler(2);
    feed(&mut a, [pkt(START, 0, &[1]), pkt(START, 1, &[2])]);
    a.lost_tail();
    assert_eq!(a.dropped(), 2);
    assert!(feed(&mut a, [pkt(END, 0, &[3])]).is_empty());
    feed(&mut a, [pkt(START, 0, &[1])]);
    a.reset();
    assert_eq!(a.dropped(), 4);
}

/// UMPs that are not SysEx8 are for the caller; a SysEx8 packet is not.
#[test]
fn only_sysex8_packets_are_taken() {
    let mut a = assembler(1);
    assert_eq!(a.push(Ump([0x0000_0000; 4])), Sysex8Step::NotSysex8);
    assert_eq!(
        a.push(Ump([0x5081_0010, 1, 0x0041_0000, 0])),
        Sysex8Step::NotSysex8
    );
    assert_eq!(a.push(pkt(START, 0, &[1])), Sysex8Step::Held);
    // Round trips through `Message` agree with the raw builder.
    let Ok(Message::Data128(_, Data128::Sysex8(p))) = pkt(START, 0, &[1]).decode() else {
        unreachable!()
    };
    assert_eq!(
        (p.status(), p.stream_id().get(), p.bytes()),
        (SysexStatus::Start, 0, &[1][..])
    );
}

/// Section 7.8, `# of bytes`: the bytes after the valid ones, the Stream ID
/// included in the count, are zero.
/// Spec: m2-104/7.8#3
#[test]
fn unused_bytes_of_a_packet_are_zero() {
    for len in 0..=40 {
        let data = vec![0xFF; len];
        let (group, id, data) = (G0, 0x55, &data);
        for ump in written(id, group, data) {
            let count = ((ump.0[0] >> 16) & 0xF) as usize;
            let mut bytes = Vec::new();
            bytes.extend([ump.0[0] >> 8, ump.0[0]].map(|w| w as u8));
            for w in &ump.0[1..] {
                bytes.extend(w.to_be_bytes());
            }
            // The first byte is the Stream ID, counted by `# of bytes`.
            assert_eq!(bytes[0], 0x55, "{len}");
            assert!(bytes[1..count].iter().all(|b| *b == 0xFF), "{len}");
            assert!(bytes[count..].iter().all(|b| *b == 0), "{len}");
        }
    }
}

/// The Stream ID in the first word of a packet (Figure 85, byte 3).
fn wire_id(ump: Ump) -> u8 {
    ((ump.0[0] >> 8) & 0xFF) as u8
}

/// The first packet of `payload` on a freshly opened stream of `ids`, the
/// message left open.
fn start<'a>(
    ids: &'a Sysex8StreamIds,
    payload: &'a [u8],
) -> Option<(Ump, tutti_ump::Sysex8Packets<'a>)> {
    let mut packets = ids.open()?.packets(G0, payload);
    Some((packets.next().unwrap(), packets))
}

/// Section 7.8, Stream id: a device with only one stream uses Stream ID 0,
/// whether it declared one stream or has not been discovered.
/// Spec: m2-104/7.8#5
#[test]
fn a_one_stream_device_uses_stream_id_0() {
    for ids in [
        Sysex8StreamIds::undiscovered(),
        Sysex8StreamIds::discovered(Sysex8StreamCount::new(1)),
    ] {
        assert_eq!(ids.count(), Sysex8StreamCount::new(1));
        for ump in ids.open().unwrap().packets(G0, &[0x80; 40]) {
            assert_eq!(wire_id(ump), 0);
        }
    }
}

/// Section 7.8, Stream id: without discovery of multi-stream support only one
/// stream is sent at a time: a second is refused while the first message is
/// open, never interleaved, and is available once the first is written.
/// Spec: m2-104/7.8#6
#[test]
fn without_discovery_a_second_concurrent_stream_is_refused() {
    let ids = Sysex8StreamIds::undiscovered();
    let (_, first) = start(&ids, &[0x80; 40]).unwrap();
    assert!(ids.open().is_none());
    drop(first.collect::<Vec<_>>());
    assert_eq!(wire_id(start(&ids, &[1]).unwrap().0), 0);
}

/// Section 7.8, Stream id: a receiver of N streams takes Stream IDs 0 to N-1
/// only, and a stream beyond N waits until a message is written.
/// Spec: m2-104/7.8#7
#[test]
fn n_streams_use_only_stream_ids_0_to_n_minus_1() {
    let ids = Sysex8StreamIds::discovered(Sysex8StreamCount::new(3));
    let open: Vec<_> = (0..3).map(|_| start(&ids, &[0x80; 40]).unwrap()).collect();
    assert_eq!(
        open.iter().map(|(u, _)| wire_id(*u)).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert!(ids.open().is_none(), "a fourth waits");
    let mut open = open;
    let (_, middle) = open.remove(1);
    drop(middle.collect::<Vec<_>>());
    assert_eq!(wire_id(start(&ids, &[1]).unwrap().0), 1);
    let all = Sysex8StreamIds::discovered(Sysex8StreamCount::new(255));
    let streams: Vec<_> = std::iter::from_fn(|| all.open()).collect();
    assert_eq!(streams.len(), 255);
}

/// Function Block Info: `Max Number of SysEx 8 Streams` 0 is a receiver that
/// takes no System Exclusive 8, so it hands out no stream.
#[test]
fn a_receiver_declaring_zero_streams_gets_none() {
    assert!(
        Sysex8StreamIds::discovered(Sysex8StreamCount::new(0))
            .open()
            .is_none()
    );
}

/// Section 7.8.1: the abort is an End with `# of bytes` 0xF and zero data on
/// the message's Stream ID, and it frees the ID.
/// Spec: m2-104/7.8.1#1
#[test]
fn an_abort_ends_the_message_and_frees_its_id() {
    let ids = Sysex8StreamIds::discovered(Sysex8StreamCount::new(2));
    let (_, _held) = start(&ids, &[0x80; 40]).unwrap();
    let (_, packets) = start(&ids, &[0x80; 40]).unwrap();
    assert_eq!(packets.abort(), Ump([0x503F_0100, 0, 0, 0]));
    assert_eq!(wire_id(start(&ids, &[1]).unwrap().0), 1);
}

/// Section 7.8.1: a message dropped part-way is open on the receiver, so its
/// Stream ID is not reopened; the abort it owes (an End with `# of bytes` 0xF
/// on the message's Group) is taken, and the ID is free again.
/// Spec: m2-104/7.8.1#1
#[test]
fn an_unfinished_message_keeps_its_id_until_its_abort_is_taken() {
    let ids = Sysex8StreamIds::discovered(Sysex8StreamCount::new(2));
    let g5 = MidiGroup::of(5);
    let mut packets = ids.open().unwrap().packets(g5, &[0x80; 40]);
    packets.next().unwrap();
    drop(packets);
    // ID 0 is owed an abort: the next open is ID 1.
    assert_eq!(wire_id(start(&ids, &[1]).unwrap().0), 1);
    assert_eq!(ids.take_abort(), Some(Ump([0x553F_0000, 0, 0, 0])));
    assert_eq!(ids.take_abort(), None);
    assert_eq!(wire_id(start(&ids, &[1]).unwrap().0), 0);
}

/// An opened stream dropped without writing frees its ID: nothing was sent.
#[test]
fn an_unused_stream_frees_its_id() {
    let ids = Sysex8StreamIds::undiscovered();
    drop(ids.open().unwrap());
    assert_eq!(wire_id(start(&ids, &[1]).unwrap().0), 0);
    drop(ids.open().unwrap().packets(G0, &[1]));
    assert!(ids.open().is_some());
}

/// Section 7.8.1: a sender out of data ends the message with an End of zero
/// data and `# of bytes` 0x1 (the data so far is valid) or 0xF (incomplete or
/// unknown); either frees the ID.
/// Spec: m2-104/7.8.1#1
#[test]
fn an_early_end_is_an_end_of_one_or_f_bytes_and_frees_the_id() {
    let ids = Sysex8StreamIds::undiscovered();
    let (_, packets) = start(&ids, &[0x80; 40]).unwrap();
    assert_eq!(packets.end_early(), Ump([0x5031_0000, 0, 0, 0]));
    let (_, packets) = start(&ids, &[0x80; 40]).unwrap();
    assert_eq!(packets.abort(), Ump([0x503F_0000, 0, 0, 0]));
    assert!(ids.open().is_some());
}
