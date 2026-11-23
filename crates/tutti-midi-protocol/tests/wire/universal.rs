//! Universal System Exclusive: the grammar of M1 pp.40, 52 and 57 and CA-025,
//! and the instrument's edge that reassembles, filters by device id and reads.

use tutti_midi_protocol::{
    Addressed, Addressee, GeneralMidiMode, Heard, MtcFullMessage, MtcRate, MtcTimeCode,
    NotSysexDeviceId, QuarterFrame, Recipient, SmpteFrameRate, SmpteTimecode, SysexDeviceId,
    TimecodeHours, TimecodeMinutes, TimecodeSeconds, Universal, UniversalRead, UniversalReader,
    read,
};
use tutti_ump::Voice1;
use tutti_ump::{
    Address, DeviceIdentity, ManufacturerId, Message, MidiChannel, MidiGroup, NoteNumber,
    SoftwareRevision, Sysex7Packets, SysexMessage, U7, Ump,
};
use tutti_units::{Losses, Seconds};

fn parse(bytes: &[u8]) -> UniversalRead {
    read(SysexMessage::from_framed(bytes).expect("a framed message"))
}

fn heard(bytes: &[u8]) -> Addressed {
    match parse(bytes) {
        UniversalRead::Universal(a) => a,
        other => panic!("{other:?}"),
    }
}

fn u7s<const N: usize>(bytes: [u8; N]) -> [U7; N] {
    bytes.map(U7::of)
}

/// M1 p.40: `F0 7E <device ID> 06 01 F7` is the Identity Request.
#[test]
fn an_identity_request_is_the_inquiry_of_p40() {
    let a = heard(&[0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7]);
    assert_eq!(a.message, Universal::IdentityRequest);
    assert_eq!(a.to, Recipient::AllCall);
    let a = heard(&[0xF0, 0x7E, 0x05, 0x06, 0x01, 0xF7]);
    assert_eq!(a.to, Recipient::Device(SysexDeviceId::of(5)));
}

/// M1 p.40: `06 02 mm ff ff dd dd ss ss ss ss`, the 14-bit family and member
/// codes LSB first, and a manufacturer id that begins with 00 extended by two
/// bytes.
#[test]
fn an_identity_reply_has_a_one_or_three_byte_manufacturer() {
    let one = heard(&[
        0xF0, 0x7E, 0x00, 0x06, 0x02, 0x41, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0xF7,
    ]);
    assert_eq!(
        one.message,
        Universal::IdentityReply(DeviceIdentity {
            manufacturer: ManufacturerId::of(0x41),
            family: u7s([1, 2]),
            model: u7s([3, 4]),
            revision: SoftwareRevision::from(u7s([5, 6, 7, 8])),
        })
    );
    let three = heard(&[
        0xF0, 0x7E, 0x00, 0x06, 0x02, 0x00, 0x21, 0x09, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
        0x08, 0xF7,
    ]);
    let Universal::IdentityReply(id) = three.message else {
        panic!()
    };
    assert_eq!(id.manufacturer, ManufacturerId::of(0xA109));
    assert_eq!(id.revision.get(), u7s([5, 6, 7, 8]));
}

/// M1 p.52: `09 01` is General MIDI On and `09 02` Off.
#[test]
fn general_midi_on_and_off() {
    assert_eq!(
        heard(&[0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7]).message,
        Universal::GeneralMidi(GeneralMidiMode::On)
    );
    assert_eq!(
        heard(&[0xF0, 0x7E, 0x7F, 0x09, 0x02, 0xF7]).message,
        Universal::GeneralMidi(GeneralMidiMode::Off)
    );
}

fn value(message: Universal) -> u16 {
    match message {
        Universal::MasterVolume(v)
        | Universal::MasterBalance(v)
        | Universal::MasterFineTuning(v) => v.get(),
        Universal::MasterCoarseTuning(v) => u16::from(v.get()),
        other => panic!("{other:?}"),
    }
}

/// M1 p.57: Master Volume and Balance carry two data bytes, least significant first; `00 00` is
/// off or hard left and `7F 7F` hard right.
#[test]
fn master_volume_and_balance_are_fourteen_bits_lsb_first() {
    let volume = |lsb, msb| heard(&[0xF0, 0x7F, 0x7F, 0x04, 0x01, lsb, msb, 0xF7]).message;
    assert!(matches!(volume(0, 0), Universal::MasterVolume(_)));
    assert_eq!(value(volume(0x00, 0x00)), 0);
    assert_eq!(value(volume(0x7F, 0x7F)), 0x3FFF);
    // lsb 0x01, msb 0x02 is 2 * 128 + 1: the lsb comes first.
    assert_eq!(value(volume(0x01, 0x02)), 0x0101);
    let balance = heard(&[0xF0, 0x7F, 0x7F, 0x04, 0x02, 0x00, 0x40, 0xF7]).message;
    assert!(matches!(balance, Universal::MasterBalance(_)));
    assert_eq!(value(balance), 0x2000);
}

/// CA-025: Master Fine Tuning is `04 03 lsb msb`, `00 40` the centre; Master
/// Coarse Tuning is `04 04 00 msb`, `00 40` the centre and `00 7F` +63.
#[test]
fn master_fine_and_coarse_tuning_follow_ca25() {
    let fine = heard(&[0xF0, 0x7F, 0x7F, 0x04, 0x03, 0x00, 0x40, 0xF7]).message;
    assert!(matches!(fine, Universal::MasterFineTuning(_)));
    assert_eq!(value(fine), 0x2000);
    let coarse = |msb| heard(&[0xF0, 0x7F, 0x7F, 0x04, 0x04, 0x00, msb, 0xF7]).message;
    assert!(matches!(coarse(0), Universal::MasterCoarseTuning(_)));
    assert_eq!(value(coarse(0x40)), 0x40);
    assert_eq!(value(coarse(0x7F)), 0x7F);
}

/// M1 p.53 and T-9: the MTC Full Message is `F0 7F <id> 01 01 hr mn sc fr F7`
/// with `hr` as `0rrhhhhh`: the two rate bits sit above the five hour bits.
#[test]
fn an_mtc_full_message_reads_its_position_and_rate_code() {
    let full = |hr, mn, sc, fr| heard(&[0xF0, 0x7F, 0x7F, 0x01, 0x01, hr, mn, sc, fr, 0xF7]);
    for (code, rate) in [
        (0u8, MtcRate::Fps24),
        (1, MtcRate::Fps25),
        (2, MtcRate::Fps2997Df),
        (3, MtcRate::Fps30),
    ] {
        let a = full(code << 5 | 23, 59, 58, 17);
        assert_eq!(a.to, Recipient::AllCall);
        assert_eq!(
            a.message,
            Universal::MtcFullMessage(
                MtcFullMessage::new(
                    rate,
                    TimecodeHours::of(23),
                    TimecodeMinutes::of(59),
                    TimecodeSeconds::of(58),
                    17
                )
                .unwrap()
            )
        );
    }
}

/// M1 p.53 and T-9: the Full Message the generator writes is the bytes the
/// reader reads, with `hr` as `0rrhhhhh`; the payload of 25 fps `01:02:03:04`
/// is `7F 7F 01 01 21 02 03 04`.
#[test]
fn an_mtc_full_message_writes_the_bytes_of_t9_and_reads_back() {
    let message = MtcFullMessage::new(
        MtcRate::Fps25,
        TimecodeHours::of(1),
        TimecodeMinutes::of(2),
        TimecodeSeconds::of(3),
        4,
    )
    .unwrap();
    assert_eq!(
        message.payload(),
        [0x7F, 0x7F, 0x01, 0x01, 0x21, 0x02, 0x03, 0x04]
    );
    for rate in MtcRate::ALL {
        let message = MtcFullMessage::new(
            rate,
            TimecodeHours::of(23),
            TimecodeMinutes::of(59),
            TimecodeSeconds::of(58),
            17,
        )
        .unwrap();
        let mut framed = vec![0xF0];
        framed.extend(message.payload());
        framed.push(0xF7);
        assert_eq!(heard(&framed).message, Universal::MtcFullMessage(message));
    }
}

/// M1 p.53 and T-9: the hours byte is `0rrhhhhh` and the last Quarter Frame
/// carries its high half as `0rrh`, so one layout states the Full Message, the
/// MMC time code and the cycle, for every rate and every hour. The expected
/// bytes are built from the spec's rate codes (0 to 3) and the hour, not from
/// the encoder.
#[test]
fn the_hours_byte_is_the_rate_over_the_hours_in_every_encoding() {
    for (code, rate) in [
        (0u8, MtcRate::Fps24),
        (1, MtcRate::Fps25),
        (2, MtcRate::Fps2997Df),
        (3, MtcRate::Fps30),
    ] {
        for hours in 0..24u8 {
            let time = MtcTimeCode::new(
                rate,
                TimecodeHours::of(hours),
                TimecodeMinutes::of(0),
                TimecodeSeconds::of(0),
                0,
            )
            .unwrap();
            let spec = (code << 5) | hours;
            assert_eq!(time.hours_byte(), spec, "{rate:?} hour {hours}");
            assert_eq!(MtcFullMessage::from(time).payload()[4], spec);
            let nibbles = time.nibbles();
            assert_eq!(nibbles[6], hours & 0x0F, "hours low nibble");
            assert_eq!(
                nibbles[7],
                (code << 1) | (hours >> 4),
                "rate over hours high"
            );
            assert_eq!(time.rate(), rate);
            assert_eq!(MtcTimeCode::from_nibbles(nibbles), Some(time));
        }
    }
}

/// A time code holds its rate once: the SMPTE position it reads as is counted
/// at the rate it was made with, and the position's labels are its own.
#[test]
fn an_mtc_time_code_reads_as_a_smpte_position_at_its_one_rate() {
    for rate in MtcRate::ALL {
        let time = MtcTimeCode::new(
            rate,
            TimecodeHours::of(1),
            TimecodeMinutes::of(2),
            TimecodeSeconds::of(3),
            4,
        )
        .unwrap();
        assert_eq!(time.timecode().frame_rate(), rate.frame_rate());
        assert_eq!(
            (
                time.timecode().hours().get(),
                time.timecode().minutes().get(),
                time.timecode().seconds().get(),
                time.timecode().frames()
            ),
            (1, 2, 3, 4)
        );
        assert_eq!(MtcTimeCode::from_timecode(time.timecode()), Some(time));
    }
}

/// M1 p.53: a frame number is below the rate's frame count (24, 25, 30), and
/// MTC has no code for 29.97 non-drop.
#[test]
fn a_frame_at_or_past_the_rate_count_and_a_non_mtc_rate_are_refused() {
    for rate in MtcRate::ALL {
        let count = rate.labels_per_second();
        assert!(
            MtcTimeCode::new(
                rate,
                TimecodeHours::of(0),
                TimecodeMinutes::of(0),
                TimecodeSeconds::of(0),
                count - 1
            )
            .is_some(),
            "{rate:?}"
        );
        assert_eq!(
            MtcTimeCode::new(
                rate,
                TimecodeHours::of(0),
                TimecodeMinutes::of(0),
                TimecodeSeconds::of(0),
                count
            ),
            None,
            "{rate:?}"
        );
        assert_eq!(
            MtcTimeCode::new(
                rate,
                TimecodeHours::of(0),
                TimecodeMinutes::of(0),
                TimecodeSeconds::of(0),
                31
            ),
            None,
            "{rate:?}"
        );
    }
    let non_drop = SmpteTimecode::new(
        TimecodeHours::of(0),
        TimecodeMinutes::of(0),
        TimecodeSeconds::of(1),
        5,
        SmpteFrameRate::Fps2997Ndf,
    )
    .unwrap();
    assert_eq!(MtcTimeCode::from_timecode(non_drop), None);
}

/// UMP section 7.7 (System Exclusive 7): the Full Message of 25 fps
/// `01:02:03:04` is a Start of `7F 7F 01 01 21 02` and an End of `03 04`, in
/// words `3616_7F7F 0101_2102` and `3632_0304 0`, on Group 6.
#[test]
fn an_mtc_full_message_is_a_start_and_an_end_packet() {
    let message = MtcFullMessage::new(
        MtcRate::Fps25,
        TimecodeHours::of(1),
        TimecodeMinutes::of(2),
        TimecodeSeconds::of(3),
        4,
    )
    .unwrap();
    let [start, end] = message.packets(tutti_ump::MidiGroup::of(6));
    assert_eq!(start.0, [0x3616_7F7F, 0x0101_2102, 0, 0]);
    assert_eq!(end.0, [0x3632_0304, 0, 0, 0]);
}

/// M1 p.53 (Quarter Frame Messages): piece `n` carries the nibbles frames
/// low, frames high, seconds low, seconds high, minutes low, minutes high,
/// hours low, hours high over the rate code (`0rrh`), so `19:20:21:22` at 30 fps
/// is the nibbles `6 1 5 1 4 1 3 7` and the data bytes `0n` through `7n`.
#[test]
fn the_quarter_frame_pieces_of_a_position_are_the_nibbles_of_t9() {
    let at = MtcFullMessage::new(
        MtcRate::Fps30,
        TimecodeHours::of(19),
        TimecodeMinutes::of(20),
        TimecodeSeconds::of(21),
        22,
    )
    .unwrap()
    .time_code();
    assert_eq!(at.nibbles(), [6, 1, 5, 1, 4, 1, 3, 7]);
    assert_eq!(
        at.cycle().frames().map(|q| q.data().get()),
        [0x06, 0x11, 0x25, 0x31, 0x44, 0x51, 0x63, 0x77]
    );
    // Each frame carries its own piece number and that piece's nibble.
    for (piece, frame) in at.cycle().frames().into_iter().enumerate() {
        assert_eq!(usize::from(frame.piece.get()), piece);
        assert_eq!(frame.nibble.get(), at.nibbles()[piece]);
    }
    assert_eq!(MtcTimeCode::from_nibbles(at.nibbles()), Some(at));
}

/// **A Quarter Frame's data byte is `0nnn dddd`** (M1 p.53): the piece is the
/// three bits above the nibble, so every seven-bit value is one Quarter Frame and
/// reads back as its own byte.
#[test]
fn a_quarter_frame_is_the_piece_over_the_nibble_of_its_data_byte() {
    for byte in 0..=0x7F {
        let frame = QuarterFrame::from_data(U7::of(byte));
        assert_eq!(frame.piece.get(), byte >> 4, "{byte:#04x}");
        assert_eq!(frame.nibble.get(), byte & 0x0F, "{byte:#04x}");
        assert_eq!(frame.data(), U7::of(byte));
    }
}

/// Every position at every rate reads back from its own nibbles; a nibble
/// set whose fields no clock reads (frame 31) is `None`, and the reserved
/// bits above a field are not read.
#[test]
fn nibbles_invert_for_every_rate_and_refuse_a_position_no_clock_reads() {
    for rate in MtcRate::ALL {
        for (h, m, s, f) in [(0, 0, 0, 0), (23, 59, 59, rate.labels_per_second() - 1)] {
            let at = MtcTimeCode::new(
                rate,
                TimecodeHours::of(h),
                TimecodeMinutes::of(m),
                TimecodeSeconds::of(s),
                f,
            )
            .unwrap();
            assert_eq!(MtcTimeCode::from_nibbles(at.nibbles()), Some(at));
        }
    }
    assert_eq!(MtcTimeCode::from_nibbles([15, 1, 0, 0, 0, 0, 0, 6]), None);
    let origin = MtcTimeCode::origin(MtcRate::Fps24);
    let mut n = origin.nibbles();
    n[1] |= 0x0E;
    n[3] |= 0x0C;
    assert_eq!(MtcTimeCode::from_nibbles(n), Some(origin));
}

/// A position no clock reads is not a Full Message: the frame field is below
/// the rate's label count, and the hours below 24.
#[test]
fn an_mtc_full_message_out_of_range_is_unread() {
    let bytes = |hr, mn, sc, fr| [0xF0, 0x7F, 0x7F, 0x01, 0x01, hr, mn, sc, fr, 0xF7];
    for (hr, mn, sc, fr) in [
        (24u8, 0u8, 0u8, 0u8),
        (0, 60, 0, 0),
        (0, 0, 60, 0),
        (0, 0, 0, 24),
        (1 << 5, 0, 0, 25),
        (3 << 5, 0, 0, 30),
    ] {
        assert_eq!(
            parse(&bytes(hr, mn, sc, fr)),
            UniversalRead::Unread,
            "{hr} {mn} {sc} {fr}"
        );
    }
    assert!(
        MtcFullMessage::new(
            MtcRate::Fps25,
            TimecodeHours::of(0),
            TimecodeMinutes::of(0),
            TimecodeSeconds::of(0),
            24
        )
        .is_some()
    );
    assert!(
        MtcFullMessage::new(
            MtcRate::Fps25,
            TimecodeHours::of(0),
            TimecodeMinutes::of(0),
            TimecodeSeconds::of(0),
            25
        )
        .is_none()
    );
}

/// A message that is not one this crate reads is a typed outcome, never a
/// refusal that drops a stream: a manufacturer's SysEx is `Foreign`, another
/// Universal message (GM2 On, MIDI-CI) or a cut or overlong one is `Unread`.
#[test]
fn what_is_not_read_is_foreign_or_unread() {
    assert_eq!(
        parse(&[0xF0, 0x41, 0x10, 0x42, 0xF7]),
        UniversalRead::Foreign,
        "a manufacturer"
    );
    assert_eq!(
        parse(&[0xF0, 0xF7]),
        UniversalRead::Foreign,
        "an empty message"
    );
    for unread in [
        &[0xF0, 0x7E, 0x7F, 0x09, 0x03, 0xF7][..],
        &[0xF0, 0x7E, 0x7F, 0x0D, 0x70, 0x01, 0xF7],
        &[0xF0, 0x7F, 0x7F, 0x04, 0x01, 0x00, 0xF7],
        &[0xF0, 0x7F, 0x7F, 0x04, 0x01, 0x00, 0x40, 0x01, 0xF7],
        &[0xF0, 0x7E, 0x7F, 0x09, 0x01, 0x00, 0xF7],
        &[0xF0, 0x7E, 0x7F, 0x06, 0x02, 0x41, 0x01, 0xF7],
        &[0xF0, 0x7E, 0xF7],
        &[0xF0, 0x7F, 0x7F, 0x04, 0x09, 0x00, 0x00, 0xF7],
    ] {
        assert_eq!(parse(unread), UniversalRead::Unread, "{unread:02X?}");
    }
}

fn packets(group: u8, bytes: &[u8]) -> Vec<Ump> {
    SysexMessage::from_framed(bytes)
        .unwrap()
        .packets(MidiGroup::of(group))
        .collect()
}

fn reader(own: u8) -> UniversalReader {
    UniversalReader::new(MidiGroup::of(0), SysexDeviceId::of(own))
}

fn feed(reader: &mut UniversalReader, umps: impl IntoIterator<Item = Ump>) -> Vec<Heard> {
    umps.into_iter().filter_map(|u| reader.push(u)).collect()
}

const VOLUME: [u8; 8] = [0xF0, 0x7F, 0x7F, 0x04, 0x01, 0x00, 0x40, 0xF7];
/// An Identity Reply: a 13-byte payload, so three SysEx7 packets.
const REPLY: [u8; 15] = [
    0xF0, 0x7E, 0x7F, 0x06, 0x02, 0x41, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0xF7,
];

/// M1 p.40: device id 7FH makes every device respond whatever id it is set to;
/// otherwise only the device with that id does.
#[test]
fn the_all_call_and_the_own_id_are_heard_and_no_other() {
    let mut r = reader(5);
    let to = |id: u8| {
        let mut bytes = VOLUME;
        bytes[2] = id;
        bytes
    };
    let all = feed(&mut r, packets(0, &to(0x7F)));
    assert_eq!(
        all.iter().map(|h| h.to).collect::<Vec<_>>(),
        [Addressee::All]
    );
    let own = feed(&mut r, packets(0, &to(5)));
    assert_eq!(
        own.iter().map(|h| h.to).collect::<Vec<_>>(),
        [Addressee::Own]
    );
    assert!(feed(&mut r, packets(0, &to(6))).is_empty());
}

/// A message longer than one packet is reassembled (M2-104 section 7.7): it is
/// heard when its End arrives, once.
#[test]
fn a_message_in_several_packets_is_heard_at_its_end() {
    let mut r = reader(0);
    let umps = packets(0, &REPLY);
    assert!(umps.len() > 1, "the test needs a divided message");
    let (last, first) = umps.split_last().unwrap();
    assert!(feed(&mut r, first.iter().copied()).is_empty());
    assert_eq!(feed(&mut r, [*last]).len(), 1);
}

/// M2-104 section 7.7.1: a message of the Group that is not a Continue ends an
/// open SysEx, so a Note On in the middle drops it; a message of another
/// Group is interspersed freely.
#[test]
fn a_message_of_the_group_ends_an_open_one_and_another_group_does_not() {
    let note = Ump::from(Message::midi1(
        Address {
            group: MidiGroup::of(0),
            channel: MidiChannel::of(0),
        },
        Voice1::note_on(NoteNumber::of(60), U7::of(64)),
    ));
    let other_group_note = Ump::from(Message::midi1(
        Address {
            group: MidiGroup::of(1),
            channel: MidiChannel::of(0),
        },
        Voice1::note_on(NoteNumber::of(60), U7::of(64)),
    ));
    let umps = packets(0, &REPLY);
    let mut r = reader(0);
    let cut = [umps[0], note].into_iter().chain(umps[1..].iter().copied());
    assert!(feed(&mut r, cut).is_empty());
    assert_eq!(r.losses(), Losses::lost(1));
    let mut r = reader(0);
    let kept = [umps[0], other_group_note]
        .into_iter()
        .chain(umps[1..].iter().copied());
    assert_eq!(feed(&mut r, kept).len(), 1);
    // The same message on another Group is not this reader's.
    assert!(feed(&mut reader(0), packets(1, &REPLY)).is_empty());
}

/// A message longer than the reader's room is dropped whole and counted, and
/// the next one is read.
#[test]
fn an_overlong_message_is_dropped_counted_and_the_next_is_read() {
    let mut long = vec![0xF0, 0x41];
    long.extend(std::iter::repeat_n(0x11, 600));
    long.push(0xF7);
    let mut r = reader(0);
    assert!(feed(&mut r, packets(0, &long)).is_empty());
    assert_eq!(r.losses(), Losses::lost(1));
    assert_eq!(feed(&mut r, packets(0, &VOLUME)).len(), 1);
}

/// A Key-Based Tuning Dump is 409 bytes with its framing: it crosses the SysEx7
/// packets whole and is read; a copy with a bad checksum is counted and not
/// passed on, but only when it was for this device or for all.
#[test]
fn a_tuning_dump_crosses_the_reader_and_a_corrupt_one_is_counted() {
    let mut payload = vec![0x7E, 0x7F, 0x08, 0x04, 0x00, 0x00];
    payload.extend_from_slice(b"Equal           ");
    for k in 0..128u8 {
        payload.extend([k, 0, 0]);
    }
    payload.push(payload.iter().fold(0, |a, b| a ^ b) & 0x7F);
    let mut framed = vec![0xF0];
    framed.extend(&payload);
    framed.push(0xF7);
    assert_eq!(framed.len(), 409);

    let mut r = reader(0);
    let heard = feed(&mut r, packets(0, &framed));
    assert_eq!(heard.len(), 1);
    assert!(matches!(heard[0].message, Universal::Tuning(_)));
    assert_eq!(r.losses(), Losses::NONE);

    // The checksum is wrong, for all devices: counted, not passed on.
    let n = framed.len();
    framed[n - 2] ^= 0x01;
    assert!(feed(&mut r, packets(0, &framed)).is_empty());
    assert_eq!(r.losses(), Losses::lost(1));
    // For another device it is none of this reader's business.
    framed[2] = 9;
    assert!(feed(&mut r, packets(0, &framed)).is_empty());
    assert_eq!(r.losses(), Losses::lost(1));
}

/// **A device's own id is `00` to `7E`: `7F` is the all-call** (M1 p.40), which
/// every device answers and none has as its own, so it is refused as an id and
/// read as `Recipient::AllCall` in a header.
#[test]
fn the_all_call_is_no_devices_own_id() {
    assert_eq!(SysexDeviceId::new(0x7F), None);
    assert_eq!(SysexDeviceId::try_from(0x7F), Err(NotSysexDeviceId(0x7F)));
    assert_eq!(SysexDeviceId::new(0x7E).map(SysexDeviceId::get), Some(0x7E));
    assert_eq!(Recipient::of_byte(U7::of(0x7F)), Recipient::AllCall);
    assert_eq!(
        Recipient::of_byte(U7::of(0x7E)),
        Recipient::Device(SysexDeviceId::of(0x7E))
    );
}

// --- MIDI Machine Control (RP-013) ---

mod machine_control {
    use super::*;
    use tutti_midi_protocol::{
        MMC_MAX_DATA, McpSuccess, McsSuccess, MmcCodeStatus, MmcCommand, MmcCommandReassembler,
        MmcCommands, MmcDirection, MmcErrorCode, MmcErrorKind, MmcEventDirection, MmcEventFlags,
        MmcFields, MmcGroup, MmcInfoField, MmcLocate, MmcMantissa, MmcName, MmcPosition,
        MmcProcedure, MmcResponseReassembler, MmcSegment, MmcSegmentBytes, MmcSegmentCount,
        MmcSegmentError, MmcSegmentPlace, MmcSegments, MmcSegmentsRefused, MmcSelection, MmcShift,
        MmcShortTimeCode, MmcSign, MmcSlot, MmcSpeed, MmcStep, MmcStepCount, MmcSubframes,
        MmcTimeCode, MmcTimeCodeName, MmcTimeLabel, MmcTimeTail, MmcUpdate, MotionControlState,
        MotionControlTally, MotionProcess, MtcRate, MtcTimeCode,
    };

    fn framed(device: u8, sub_id: u8, data: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xF0, 0x7F, device, sub_id];
        bytes.extend_from_slice(data);
        bytes.push(0xF7);
        bytes
    }

    fn to_commands(device: u8, data: &[u8]) -> (Recipient, Vec<MmcCommand>) {
        match heard(&framed(device, 0x06, data)) {
            Addressed {
                to,
                message: Universal::MachineControl(commands),
            } => (to, commands.iter().collect()),
            other => panic!("{other:?}"),
        }
    }

    fn one_command(data: &[u8]) -> MmcCommand {
        let (_, mut commands) = to_commands(0x7F, data);
        assert_eq!(commands.len(), 1, "{commands:?}");
        commands.remove(0)
    }

    fn fields(source: u8, data: &[u8]) -> Vec<MmcInfoField> {
        match heard(&framed(source, 0x07, data)).message {
            Universal::MachineControlResponse(fields) => fields.iter().collect(),
            other => panic!("{other:?}"),
        }
    }

    /// The label of `h:m:s:f` at 30 frames.
    fn label(h: u8, m: u8, s: u8, f: u8) -> MmcTimeLabel {
        MmcTimeLabel::new(
            MtcTimeCode::new(
                MtcRate::Fps30,
                TimecodeHours::of(h),
                TimecodeMinutes::of(m),
                TimecodeSeconds::of(s),
                f,
            )
            .unwrap(),
        )
    }

    /// RP-013 p.14 and M1 p.53: the Time Standard field's byte is the rate code
    /// in bits 5 and 6 over zero hours, so each code reads as its rate.
    #[test]
    fn the_time_standard_byte_is_the_rate_code_over_zero_hours() {
        for (code, rate) in [
            (0u8, MtcRate::Fps24),
            (1, MtcRate::Fps25),
            (2, MtcRate::Fps2997Df),
            (3, MtcRate::Fps30),
        ] {
            assert_eq!(
                fields(1, &[0x45, 0x01, code << 5]),
                [MmcInfoField::TimeStandard(rate)],
                "{rate:?}"
            );
        }
    }

    /// RP-013 p.15: the commands without data are the codes `01` to `0D`, each
    /// one byte.
    #[test]
    fn the_commands_without_data_are_their_table_codes() {
        let table = [
            (0x01, MmcCommand::Stop),
            (0x02, MmcCommand::Play),
            (0x03, MmcCommand::DeferredPlay),
            (0x04, MmcCommand::FastForward),
            (0x05, MmcCommand::Rewind),
            (0x06, MmcCommand::RecordStrobe),
            (0x07, MmcCommand::RecordExit),
            (0x08, MmcCommand::RecordPause),
            (0x09, MmcCommand::Pause),
            (0x0A, MmcCommand::Eject),
            (0x0B, MmcCommand::Chase),
            (0x0C, MmcCommand::CommandErrorReset),
            (0x0D, MmcCommand::MmcReset),
            (0x7C, MmcCommand::Wait),
            (0x7F, MmcCommand::Resume),
        ];
        for (code, command) in table {
            assert_eq!(one_command(&[code]), command, "{code:02X}");
            let mut list = MmcCommands::new();
            list.push(&command).unwrap();
            assert_eq!(list.as_bytes(), [code], "{command:?} encodes");
        }
    }

    /// RP-013 p.5: the device ID is the destination, `7F` is the all-call, and
    /// any other ID names a device or a group.
    #[test]
    fn a_command_is_addressed_to_a_device_a_group_or_all() {
        assert_eq!(to_commands(0x7F, &[0x02]).0, Recipient::AllCall);
        assert_eq!(
            to_commands(0x01, &[0x02]).0,
            Recipient::Device(SysexDeviceId::of(1))
        );
        let mut list = MmcCommands::new();
        list.push(&MmcCommand::Play).unwrap();
        assert_eq!(
            list.payload(Recipient::Device(SysexDeviceId::of(0x21))),
            [0x7F, 0x21, 0x06, 0x02]
        );
        assert_eq!(list.payload(Recipient::AllCall), [0x7F, 0x7F, 0x06, 0x02]);
    }

    /// RP-013 p.4: more than one command goes in one message: p.76's example
    /// sends RECORD EXIT and DEFERRED PLAY together.
    #[test]
    fn a_message_holds_several_commands_in_order() {
        let (to, commands) = to_commands(0x01, &[0x07, 0x03]);
        assert_eq!(to, Recipient::Device(SysexDeviceId::of(1)));
        assert_eq!(commands, [MmcCommand::RecordExit, MmcCommand::DeferredPlay]);
    }

    /// RP-013 p.75, "return to zero": `44 06 01 60 00 00 00 00` locates to
    /// `00:00:00:00` at 30 frames; p.76 locates to `01:02:08:20` with `61 02 08 14 00`.
    #[test]
    fn a_locate_target_is_the_time_of_the_examples() {
        let zero = one_command(&[0x44, 0x06, 0x01, 0x60, 0, 0, 0, 0]);
        assert_eq!(
            zero,
            MmcCommand::Locate(MmcLocate::Target(MmcPosition {
                label: label(0, 0, 0, 0),
                subframes: MmcSubframes::of(0),
            }))
        );
        let review = one_command(&[0x44, 0x06, 0x01, 0x61, 0x02, 0x08, 0x14, 0x00]);
        let MmcCommand::Locate(MmcLocate::Target(target)) = review else {
            panic!()
        };
        assert_eq!(target.label, label(1, 2, 8, 20));
        let mut list = MmcCommands::new();
        list.push(&review).unwrap();
        assert_eq!(
            list.as_bytes(),
            [0x44, 0x06, 0x01, 0x61, 0x02, 0x08, 0x14, 0x00]
        );
    }

    /// RP-013 p.8: the hours byte is `0 tt hhhhh`, minutes `0 c mmmmmm`, seconds
    /// `0 k ssssss`, frames `0 g i fffff`, then subframes `0 bbbbbbb`.
    #[test]
    fn a_time_code_is_its_flags_and_fields() {
        let target = one_command(&[
            0x44, 0x06, 0x01, 0x57, /* 0 10 10111 */
            0x7B, /* 0 1 111011 */
            0x3B, 0x5D, /* 0 1 0 11101 */
            99,
        ]);
        let MmcCommand::Locate(MmcLocate::Target(p)) = target else {
            panic!()
        };
        assert_eq!(p.label.time_code().rate(), MtcRate::Fps2997Df);
        assert_eq!(
            (
                p.label.time_code().timecode().hours().get(),
                p.label.time_code().timecode().minutes().get(),
                p.label.time_code().timecode().seconds().get(),
                p.label.time_code().timecode().frames()
            ),
            (23, 59, 59, 29)
        );
        assert!(
            p.label.color_frame() && p.label.sign() == MmcSign::Negative && !p.label.is_blank()
        );
        assert_eq!(p.subframes.get(), 99);
    }

    /// RP-013 p.8: a field out of range, subframes of 100, a blank code with
    /// numbers in it, a frame above the rate's labels and status where subframes
    /// are due do not read.
    #[test]
    fn a_time_code_out_of_range_is_not_read() {
        let locate =
            |h, m, s, f, ff| parse(&framed(0x7F, 0x06, &[0x44, 0x06, 0x01, h, m, s, f, ff]));
        assert!(matches!(
            locate(0x00, 0, 0, 0, 0),
            UniversalRead::Universal(_)
        ));
        assert_eq!(locate(0x18, 0, 0, 0, 0), UniversalRead::Unread, "hour 24");
        assert_eq!(
            locate(0x00, 60, 0, 0, 0),
            UniversalRead::Unread,
            "minute 60"
        );
        assert_eq!(
            locate(0x00, 0, 0, 24, 0),
            UniversalRead::Unread,
            "frame 24 at 24 fps"
        );
        assert_eq!(
            locate(0x00, 0, 0, 0, 100),
            UniversalRead::Unread,
            "subframes 100"
        );
        assert_eq!(
            locate(0x00, 0, 0x40, 1, 0),
            UniversalRead::Unread,
            "blank with a frame"
        );
        assert_eq!(
            locate(0x00, 0, 0, 0x20, 0),
            UniversalRead::Unread,
            "status where subframes are due"
        );
    }

    /// RP-013 p.28: LOCATE `00` takes a general purpose register, `08` to `0F`.
    #[test]
    fn a_locate_can_name_a_register() {
        assert_eq!(
            one_command(&[0x44, 0x02, 0x00, 0x08]),
            MmcCommand::Locate(MmcLocate::Field(MmcName::of(0x08)))
        );
    }

    /// RP-013 p.11: the standard speed is `0 g sss ppp`, `0 qqqqqqq`,
    /// `0 rrrrrrr`; shift 0 puts the binary point after three bits, so
    /// `01 00 00` is play speed and `41 00 00` the same in reverse.
    #[test]
    fn a_speed_is_the_standard_three_bytes() {
        assert_eq!(
            one_command(&[0x45, 0x03, 0x01, 0x00, 0x00]),
            MmcCommand::VariablePlay(MmcSpeed::PLAY)
        );
        let MmcCommand::Shuttle(reverse) = one_command(&[0x47, 0x03, 0x41, 0x00, 0x00]) else {
            panic!()
        };
        assert_eq!(reverse.direction(), MmcDirection::Reverse);
        assert_eq!(
            (reverse.shift().get(), reverse.mantissa().get()),
            (0, 1 << 14)
        );
        // Shift 7: ten integer bits, seven fraction bits.
        let MmcCommand::Search(fast) = one_command(&[0x46, 0x03, 0x3F, 0x7F, 0x7F]) else {
            panic!()
        };
        assert_eq!(
            (
                fast.shift().get(),
                fast.fraction_bits(),
                fast.mantissa().get()
            ),
            (7, 7, 0x1FFFF)
        );
        let mut list = MmcCommands::new();
        list.push(&MmcCommand::DeferredVariablePlay(MmcSpeed::PLAY))
            .unwrap();
        list.push(&MmcCommand::RecordStrobeVariable(reverse))
            .unwrap();
        assert_eq!(
            list.as_bytes(),
            [0x54, 0x03, 0x01, 0, 0, 0x55, 0x03, 0x41, 0, 0]
        );
    }

    /// RP-013 p.26 and p.27: READ lists names; UPDATE `00` begins and `01`
    /// ends; a `7F` in the end list stops every update.
    #[test]
    fn read_and_update_carry_names() {
        let MmcCommand::Read(names) = one_command(&[0x42, 0x01, 0x01]) else {
            panic!()
        };
        assert_eq!(names.iter().collect::<Vec<_>>(), [MmcName::of(0x01)]);
        let MmcCommand::Update(MmcUpdate::Begin(names)) = one_command(&[0x43, 0x02, 0x00, 0x48])
        else {
            panic!()
        };
        assert_eq!(names.iter().collect::<Vec<_>>(), [MmcName::of(0x48)]);
        let MmcCommand::Update(MmcUpdate::End(names)) = one_command(&[0x43, 0x02, 0x01, 0x7F])
        else {
            panic!()
        };
        assert_eq!(names.iter().collect::<Vec<_>>(), [MmcName::of(0x7F)]);
        assert_eq!(
            parse(&framed(0x7F, 0x06, &[0x43, 0x01, 0x02])),
            UniversalRead::Unread,
            "an UPDATE sub-command other than 00 and 01"
        );
    }

    /// RP-013 p.75: WRITE carries `name data` pairs: `40 06 01 60 00 00 20 00`
    /// loads SELECTED TIME CODE with status in place of subframes.
    #[test]
    fn a_write_loads_fields() {
        let MmcCommand::Write(fields) =
            one_command(&[0x40, 0x06, 0x01, 0x60, 0x00, 0x00, 0x20, 0x00])
        else {
            panic!()
        };
        assert_eq!(
            fields.iter().collect::<Vec<_>>(),
            [MmcInfoField::TimeCode(
                MmcTimeCodeName::SelectedTimeCode,
                MmcTimeCode {
                    label: label(0, 0, 0, 0),
                    tail: MmcTimeTail::Status(MmcCodeStatus {
                        estimated: false,
                        invalid: false,
                        field_one: false,
                        no_time_code: false
                    }),
                }
            )]
        );
    }

    /// RP-013 p.39: GROUP assigns devices to a group number, and
    /// `GROUP [DIS-ASSIGN] 7F 7F` takes every device out of every group.
    #[test]
    fn group_assigns_and_dis_assigns() {
        let MmcCommand::Group(MmcGroup::Assign { group, devices }) =
            one_command(&[0x52, 0x04, 0x00, 0x21, 0x01, 0x02])
        else {
            panic!()
        };
        assert_eq!(group, SysexDeviceId::of(0x21));
        assert_eq!(
            devices.iter().collect::<Vec<_>>(),
            [
                Recipient::Device(SysexDeviceId::of(1)),
                Recipient::Device(SysexDeviceId::of(2))
            ]
        );
        let MmcCommand::Group(MmcGroup::DisAssign { group, devices }) =
            one_command(&[0x52, 0x03, 0x01, 0x7F, 0x7F])
        else {
            panic!()
        };
        assert_eq!(group, Recipient::AllCall);
        assert_eq!(devices.iter().collect::<Vec<_>>(), [Recipient::AllCall]);
        assert_eq!(
            parse(&framed(0x7F, 0x06, &[0x52, 0x03, 0x00, 0x7F, 0x01])),
            UniversalRead::Unread,
            "7F is no group's number"
        );
    }

    /// RP-013 pp.34 to 38: PROCEDURE and EVENT, their sub-commands and their
    /// reserved `7F` names, with p.37's loop example (an event whose command is
    /// PROCEDURE EXECUTE).
    #[test]
    fn procedures_and_events_have_their_sub_commands() {
        let MmcCommand::Procedure(MmcProcedure::Assemble {
            procedure,
            commands,
        }) = one_command(&[0x50, 0x06, 0x00, 0x05, 0x44, 0x02, 0x00, 0x08])
        else {
            panic!()
        };
        assert_eq!(procedure, MmcSlot::of(5));
        assert_eq!(commands.iter().count(), 1);
        assert_eq!(
            one_command(&[0x50, 0x02, 0x01, 0x7F]),
            MmcCommand::Procedure(MmcProcedure::Delete(MmcSelection::All))
        );
        assert_eq!(
            one_command(&[0x50, 0x02, 0x03, 0x05]),
            MmcCommand::Procedure(MmcProcedure::Execute(MmcSlot::of(5)))
        );
        assert_eq!(
            parse(&framed(0x7F, 0x06, &[0x50, 0x02, 0x03, 0x7F])),
            UniversalRead::Unread,
            "7F is reserved, not a name to execute"
        );
        let MmcCommand::Event(event) = one_command(&[
            0x51, 0x09, 0x00, 0x01, 0x40, 0x01, 0x09, 0x50, 0x02, 0x03, 0x05,
        ]) else {
            panic!()
        };
        let tutti_midi_protocol::MmcEvent::Define { flags, command, .. } = event else {
            panic!()
        };
        assert_eq!(
            flags,
            MmcEventFlags {
                direction: MmcEventDirection::Forward,
                all_speeds: false,
                keep: true
            }
        );
        assert_eq!(command.iter().count(), 1);
    }

    /// RP-013 p.5: a command string too long for one message goes in segments;
    /// the first is flagged `40` and counts down the rest.
    #[test]
    fn a_segment_is_flagged_first_and_counts_down() {
        let MmcCommand::CommandSegment(segment) =
            one_command(&[0x53, 0x05, 0x42, 0x01, 0x02, 0x03, 0x04])
        else {
            panic!()
        };
        assert_eq!(segment.place(), MmcSegmentPlace::First);
        assert_eq!(segment.remaining().get(), 2);
        assert_eq!(segment.bytes().as_bytes(), [1, 2, 3, 4]);
        let MmcCommand::CommandSegment(last) = one_command(&[0x53, 0x04, 0x00, 0x0A, 0x0B, 0x0C])
        else {
            panic!()
        };
        assert!(last.place() == MmcSegmentPlace::Later && last.remaining().get() == 0);
    }

    /// RP-013 p.6: a code the specification leaves undefined is skipped by its
    /// length: none under `40`, a count from `40`, and extension sets `00 nn`.
    #[test]
    fn undefined_codes_are_skipped_by_their_length() {
        let (_, commands) = to_commands(0x7F, &[0x0E, 0x56, 0x02, 0x01, 0x02, 0x00, 0x01, 0x02]);
        assert_eq!(commands.len(), 4, "{commands:?}");
        assert!(matches!(commands[0], MmcCommand::Reserved(r) if r.name == MmcName::of(0x0E)));
        assert!(matches!(commands[1], MmcCommand::Reserved(r) if r.data.as_bytes() == [1, 2]));
        assert!(matches!(commands[2], MmcCommand::Reserved(r)
            if r.name.extension() == tutti_midi_protocol::MmcExtension::First));
        assert_eq!(commands[3], MmcCommand::Play);
        assert_eq!(
            parse(&framed(0x7F, 0x06, &[0x00, 0x00, 0x00])),
            UniversalRead::Unread,
            "a third extension level"
        );
    }

    /// RP-013 p.2: a message holds at most 48 bytes after the sub-ID, and a
    /// list cannot be built longer.
    #[test]
    fn a_message_is_at_most_48_bytes() {
        assert_eq!(MMC_MAX_DATA, 48);
        let long = |n: usize| framed(0x7F, 0x06, &vec![0x01; n]);
        assert!(matches!(parse(&long(48)), UniversalRead::Universal(_)));
        assert_eq!(parse(&long(49)), UniversalRead::Unread);
        assert_eq!(
            parse(&framed(0x7F, 0x06, &[])),
            UniversalRead::Unread,
            "no command"
        );
        let mut list = MmcCommands::new();
        for _ in 0..48 {
            list.push(&MmcCommand::Stop).unwrap();
        }
        let before = list;
        assert!(list.push(&MmcCommand::Stop).is_err());
        assert_eq!(list, before, "a refused push changes nothing");
    }

    /// RP-013 p.2: the sysex is `F0 7F <device> 06 <commands> F7`, and goes out
    /// as SysEx7 packets of six bytes.
    #[test]
    fn a_message_goes_out_as_sysex7_packets() {
        let mut list = MmcCommands::new();
        list.push(&MmcCommand::Locate(MmcLocate::Target(MmcPosition {
            label: label(1, 2, 8, 20),
            subframes: MmcSubframes::of(0),
        })))
        .unwrap();
        let payload = list.payload(Recipient::Device(SysexDeviceId::of(1)));
        assert_eq!(payload.len(), 11);
        let packets: Vec<Ump> = Sysex7Packets::from_payload(MidiGroup::FIRST, &payload)
            .unwrap()
            .collect();
        assert_eq!(packets.len(), 2, "eleven bytes: a start and an end");
    }

    /// RP-013 p.4: a response's device ID is its source, one device's own; a
    /// response from `7F` is not read, and a command with the sub-ID `07` is a
    /// response.
    #[test]
    fn a_response_has_a_source_device() {
        assert_eq!(fields(5, &[0x7C]), [MmcInfoField::Wait]);
        assert_eq!(fields(5, &[0x7F]), [MmcInfoField::Resume]);
        assert_eq!(
            heard(&framed(5, 0x07, &[0x7C])).to,
            Recipient::Device(SysexDeviceId::of(5))
        );
        assert_eq!(parse(&framed(0x7F, 0x07, &[0x7C])), UniversalRead::Unread);
    }

    /// RP-013 p.78: SELECTED TIME CODE answers `43 02 15 21 00`, which is
    /// `03:02:21:01` at 30 drop frame with status in the last byte.
    #[test]
    fn a_time_code_response_reads_as_the_example_says() {
        let [MmcInfoField::TimeCode(name, code)] =
            fields(1, &[0x01, 0x43, 0x02, 0x15, 0x21, 0x00])[..]
        else {
            panic!()
        };
        assert_eq!(name, MmcTimeCodeName::SelectedTimeCode);
        assert_eq!(code.label.time_code().rate(), MtcRate::Fps2997Df);
        assert_eq!(
            (
                code.label.time_code().timecode().hours().get(),
                code.label.time_code().timecode().minutes().get(),
                code.label.time_code().timecode().seconds().get(),
                code.label.time_code().timecode().frames()
            ),
            (3, 2, 21, 1)
        );
        assert!(matches!(code.tail, MmcTimeTail::Status(_)));
    }

    /// RP-013 p.9: a short time code is frames and subframes or status, and its
    /// name is the five-byte name plus `20`.
    #[test]
    fn a_short_time_code_is_two_bytes() {
        let [MmcInfoField::ShortTimeCode(name, short)] = fields(1, &[0x21, 0x05, 0x32])[..] else {
            panic!()
        };
        assert_eq!(name, MmcTimeCodeName::SelectedTimeCode);
        assert_eq!(short.frames(), 5);
        assert_eq!(short.tail(), MmcTimeTail::Subframes(MmcSubframes::of(0x32)));
    }

    /// RP-013 pp.56 and 57, with p.79's tally: `48 03 05 44 01` is REWIND while
    /// LOCATE is active; `48 03 01 44 11` is STOP with the locate complete.
    #[test]
    fn a_motion_control_tally_is_state_process_and_success() {
        let [MmcInfoField::MotionControlTally(rewind)] =
            fields(1, &[0x48, 0x03, 0x05, 0x44, 0x01])[..]
        else {
            panic!()
        };
        assert_eq!(
            rewind,
            MotionControlTally {
                state: MotionControlState::Rewind,
                process: MotionProcess::Locate,
                state_success: McsSuccess::Achieved,
                process_success: McpSuccess::Active,
            }
        );
        let [MmcInfoField::MotionControlTally(done)] =
            fields(1, &[0x48, 0x03, 0x01, 0x44, 0x11])[..]
        else {
            panic!()
        };
        assert_eq!(done.process_success, McpSuccess::Complete);
        assert_eq!(
            parse(&framed(1, 0x07, &[0x48, 0x03, 0x01, 0x44, 0x07])),
            UniversalRead::Unread,
            "success code 7 is undefined"
        );
    }

    /// RP-013 p.78: the SIGNATURE `40 2E 01 00 00 00 14 ...` reads its two
    /// bitmaps; `61` in command byte 1 sets commands `07`, `0C` and `0D`.
    #[test]
    fn a_signature_is_two_bitmaps_of_names() {
        let mut data = vec![0x40, 0x2E, 0x01, 0x00, 0x00, 0x00, 0x14];
        data.extend_from_slice(&[
            0x7F, 0x61, 0, 0, 0, 0, 0, 0, 0, 0, 0x7F, 0x70, 0x7F, 0, 0, 0, 0, 0, 0, 0x09,
        ]);
        data.push(0x14);
        data.extend_from_slice(&[
            0x02, 0x1E, 0, 0, 0, 0x02, 0x13, 0, 0, 0, 0x3F, 0x62, 0x07, 0x01, 0x0C, 0x37, 0, 0, 0,
            0x09,
        ]);
        let [MmcInfoField::Signature(s)] = fields(1, &data)[..] else {
            panic!()
        };
        assert_eq!((s.major.get(), s.minor.get()), (1, 0));
        for supported in [0x01, 0x06, 0x07, 0x0C, 0x0D] {
            assert!(
                s.commands.supports(MmcName::of(supported)),
                "{supported:02X}"
            );
        }
        assert!(!s.commands.supports(MmcName::of(0x08)));
        assert!(
            s.commands.supports(MmcName::of(0x44)),
            "LOCATE: byte 10, bit 4"
        );
        assert!(s.fields.supports(MmcName::of(0x48)), "MOTION CONTROL TALLY");
        assert!(!s.fields.supports(MmcName::of(0x49)));
        // Written back, the same bytes.
        let mut list = MmcFields::new();
        list.push(&MmcInfoField::Signature(s)).unwrap();
        assert_eq!(list.as_bytes(), &data[..]);
    }

    /// RP-013 p.50: RESPONSE ERROR names the fields a READ asked for and the
    /// device lacks.
    #[test]
    fn a_response_error_names_the_unsupported_fields() {
        let [MmcInfoField::ResponseError(names)] = fields(1, &[0x42, 0x02, 0x10, 0x11])[..] else {
            panic!()
        };
        assert_eq!(names.iter().count(), 2);
    }

    /// RP-013 pp.51 to 53: COMMAND ERROR carries its flags, level, code and
    /// the command at fault; after power up it is `04 00 00 7F 00`.
    #[test]
    fn a_command_error_reads_its_parts() {
        let [MmcInfoField::CommandError(clear)] =
            fields(1, &[0x43, 0x04, 0x00, 0x00, 0x7F, 0x00])[..]
        else {
            panic!()
        };
        assert_eq!(clear.error, MmcErrorCode::Kind(MmcErrorKind::NoErrors));
        assert!(clear.command.is_none() && !clear.flags.halted);
        let [MmcInfoField::CommandError(error)] =
            fields(1, &[0x43, 0x06, 0x11, 0x3F, 0x40, 0x02, 0x00, 0x44])[..]
        else {
            panic!()
        };
        assert!(error.flags.halted && error.flags.unsolicited);
        assert_eq!(
            error.error,
            MmcErrorCode::Kind(MmcErrorKind::UnsupportedCommand)
        );
        assert_eq!(error.command.unwrap().offset.get(), 0);
    }

    /// RP-013 p.8: the status byte is `0 e v d n xxx` and its reserved bits are
    /// zero.
    #[test]
    fn a_status_byte_with_a_reserved_bit_is_not_read() {
        assert!(matches!(
            fields(1, &[0x01, 0x60, 0, 0, 0x20, 0b0100_1000])[..],
            [MmcInfoField::TimeCode(..)]
        ));
        assert_eq!(
            parse(&framed(1, 0x07, &[0x01, 0x60, 0, 0, 0x20, 0b0000_0001])),
            UniversalRead::Unread
        );
    }

    /// RP-013 p.11: the standard track bitmap starts with the video, a
    /// reserved bit, time code, aux A, aux B, tracks 1 and 2, then seven
    /// tracks to a byte.
    #[test]
    fn a_track_bitmap_places_each_track() {
        use tutti_midi_protocol::{MmcAudioTrack, MmcTrack, MmcTrackBitmap};
        let audio = |n| MmcTrack::Audio(MmcAudioTrack::of(n));
        let [MmcInfoField::TrackRecordReady(bitmap)] =
            fields(1, &[0x4F, 0x02, 0b0100000, 0b0000001])[..]
        else {
            panic!()
        };
        assert!(bitmap.contains(audio(1)));
        assert!(bitmap.contains(audio(3)));
        assert!(!bitmap.contains(audio(2)) && !bitmap.contains(MmcTrack::Video));
        assert!(!bitmap.contains(audio(300)), "past the bytes sent");
        let built = MmcTrackBitmap::NONE.with(audio(1)).with(audio(10));
        assert_eq!(built.as_bytes(), [0b0100000, 0, 1]);
        // The last audio track fills the last byte.
        assert_eq!(MmcTrackBitmap::NONE.with(audio(317)).as_bytes().len(), 46);
        assert!(
            MmcTrackBitmap::from_bytes(&[0b10]).is_none(),
            "the reserved bit"
        );
    }

    /// RP-013 p.71: PROCEDURE RESPONSE `7F` alone means no procedure is set.
    #[test]
    fn procedure_and_event_responses_can_be_invalid() {
        assert!(matches!(
            fields(1, &[0x60, 0x01, 0x7F])[..],
            [MmcInfoField::ProcedureResponse(
                tutti_midi_protocol::MmcProcedureResponse::Invalid
            )]
        ));
        assert!(matches!(
            fields(1, &[0x61, 0x01, 0x7F])[..],
            [MmcInfoField::EventResponse(
                tutti_midi_protocol::MmcEventResponse::Invalid
            )]
        ));
    }

    /// Every command written reads back as itself: the encoder and the grammar
    /// are one table.
    #[test]
    fn what_is_written_is_what_is_read() {
        let commands = [
            MmcCommand::Locate(MmcLocate::Field(MmcName::of(0x09))),
            MmcCommand::Update(MmcUpdate::Begin({
                let mut n = tutti_midi_protocol::MmcNames::new();
                n.push(&MmcName::of(0x48)).unwrap();
                n
            })),
            MmcCommand::Step(MmcStep::new(MmcDirection::Reverse, MmcStepCount::of(3))),
            MmcCommand::AssignSystemMaster(tutti_midi_protocol::MmcSystemMaster::Nobody),
            MmcCommand::GeneratorCommand(tutti_midi_protocol::GeneratorAction::CopyJam),
            MmcCommand::MtcCommand(tutti_midi_protocol::MtcAction::Follow),
            MmcCommand::Move(tutti_midi_protocol::MmcMove {
                destination: MmcName::of(0x08),
                source: MmcName::of(0x01),
            }),
            MmcCommand::Add(tutti_midi_protocol::MmcArithmetic {
                destination: MmcName::of(0x09),
                first: MmcName::of(0x08),
                second: MmcName::of(0x0A),
            }),
            MmcCommand::DropFrameAdjust(MmcName::of(0x09)),
            MmcCommand::Event(tutti_midi_protocol::MmcEvent::Test(MmcSlot::of(1))),
        ];
        for command in commands {
            let mut list = MmcCommands::new();
            list.push(&command).unwrap();
            let again = one_command(list.as_bytes());
            assert_eq!(again, command);
        }
    }

    /// RP-013 pp.11 and 30: the direction is bit 6 of the speed's first byte
    /// and of STEP's byte.
    #[test]
    fn a_direction_is_bit_six_of_a_speed_and_a_step() {
        assert_eq!(MmcDirection::Forward.wire(), 0);
        assert_eq!(MmcDirection::Reverse.wire(), 1);
        let mut list = MmcCommands::new();
        for direction in [MmcDirection::Forward, MmcDirection::Reverse] {
            let speed = MmcSpeed::new(direction, MmcShift::of(0), MmcMantissa::of(1 << 14));
            list.push(&MmcCommand::Shuttle(speed)).unwrap();
            list.push(&MmcCommand::Step(MmcStep::new(
                direction,
                MmcStepCount::of(3),
            )))
            .unwrap();
        }
        let MmcCommand::Shuttle(speed) = one_command(&[0x47, 0x03, 0x41, 0x00, 0x00]) else {
            panic!()
        };
        assert_eq!(speed.direction(), MmcDirection::Reverse);
        assert_eq!(
            list.as_bytes(),
            [
                0x47, 0x03, 0x01, 0, 0, 0x48, 0x01, 0x03, 0x47, 0x03, 0x41, 0, 0, 0x48, 0x01, 0x43
            ]
        );
    }

    /// RP-013 pp.8 and 9: the sign is bit 6 of the frames byte, in the time
    /// label and in the short time code.
    #[test]
    fn a_sign_is_bit_six_of_the_frames_byte() {
        assert_eq!(MmcSign::Positive.wire(), 0);
        assert_eq!(MmcSign::Negative.wire(), 1);
        assert_eq!(label(1, 2, 3, 4).sign(), MmcSign::Positive);
        let negative = label(1, 2, 3, 4).with_sign(MmcSign::Negative);
        let mut list = MmcCommands::new();
        list.push(&MmcCommand::Locate(MmcLocate::Target(MmcPosition {
            label: negative,
            subframes: MmcSubframes::of(0),
        })))
        .unwrap();
        assert_eq!(list.as_bytes()[6], 0x44);
        let [MmcInfoField::ShortTimeCode(_, short)] = fields(1, &[0x21, 0x45, 0x32])[..] else {
            panic!()
        };
        assert_eq!((short.frames(), short.sign()), (5, MmcSign::Negative));
        let short = MmcShortTimeCode::new(5, MmcSign::Positive, short.tail()).unwrap();
        assert_eq!(short.sign(), MmcSign::Positive);
    }

    /// The segment `place`, `remaining`, `bytes` as the command that carries it.
    fn segment(place: MmcSegmentPlace, remaining: u8, bytes: &[u8]) -> MmcSegment {
        MmcSegment::new(
            place,
            MmcSegmentCount::of(remaining),
            MmcSegmentBytes::new(bytes).unwrap(),
        )
    }

    /// The wire bytes of `segment` as a COMMAND SEGMENT message to all.
    fn segment_message(segment: MmcSegment) -> Vec<u8> {
        let mut commands = MmcCommands::new();
        commands.push(&MmcCommand::CommandSegment(segment)).unwrap();
        commands.payload(Recipient::AllCall).to_vec()
    }

    /// RP-013 p.5: the string `aa .. mm` goes as three segments whose counts
    /// are the segment byte count plus one (`05`, `05`, `04`) and whose
    /// identification bytes are `42`, `01`, `00`.
    #[test]
    fn the_specs_three_segment_example_has_its_counts_and_identifications() {
        use MmcSegmentPlace::{First, Later};
        let a = segment_message(segment(First, 2, &[1, 2, 3, 4]));
        let b = segment_message(segment(Later, 1, &[5, 6, 7, 8]));
        let c = segment_message(segment(Later, 0, &[9, 10, 11]));
        assert_eq!(a, [0x7F, 0x7F, 0x06, 0x53, 0x05, 0x42, 1, 2, 3, 4]);
        assert_eq!(b, [0x7F, 0x7F, 0x06, 0x53, 0x05, 0x01, 5, 6, 7, 8]);
        assert_eq!(c, [0x7F, 0x7F, 0x06, 0x53, 0x04, 0x00, 9, 10, 11]);
    }

    /// Takes in the segment messages of `bytes` and returns what the last
    /// completed, or the first refusal.
    fn reassemble(
        reassembler: &mut MmcCommandReassembler,
        segments: &[MmcSegment],
    ) -> Result<Option<Vec<MmcCommand>>, MmcSegmentError> {
        let mut done = None;
        for &segment in segments {
            done = reassembler.push(segment)?.map(|s| s.iter().collect());
        }
        Ok(done)
    }

    /// RP-013 p.40: the segments are executed as if they had arrived in one
    /// sysex, and a division need not fall on a command boundary: a LOCATE
    /// (`44 06 01 hr mn sc fr ff`) cut after its fourth byte is still one LOCATE.
    #[test]
    fn segments_make_the_commands_of_one_string_across_a_command() {
        use MmcSegmentPlace::{First, Later};
        let locate = [0x44, 0x06, 0x01, 0x61, 0x02, 0x08, 0x14, 0x00];
        let mut string = vec![0x02];
        string.extend(locate);
        string.push(0x01);
        let [a, b, c] = [&string[..4], &string[4..8], &string[8..]];
        let mut r = MmcCommandReassembler::new();
        let done = reassemble(
            &mut r,
            &[
                segment(First, 2, a),
                segment(Later, 1, b),
                segment(Later, 0, c),
            ],
        );
        let commands = done.unwrap().unwrap();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], MmcCommand::Play);
        assert!(matches!(
            commands[1],
            MmcCommand::Locate(MmcLocate::Target(_))
        ));
        assert_eq!(commands[2], MmcCommand::Stop);
        assert!(!r.in_progress());
    }

    /// A one-segment string is first and last, and the reassembler takes the
    /// next string after it.
    #[test]
    fn a_string_of_one_segment_completes_and_the_next_starts_clean() {
        use MmcSegmentPlace::First;
        let mut r = MmcCommandReassembler::new();
        for _ in 0..2 {
            let done = reassemble(&mut r, &[segment(First, 0, &[0x02, 0x01])]);
            assert_eq!(
                done.unwrap().unwrap(),
                [MmcCommand::Play, MmcCommand::Stop],
                "the first string's bytes do not stay"
            );
        }
    }

    /// RP-013 p.40 (d): segments out of order, and a segment missing, are
    /// Segmentation Errors (`09`), and cancel the string.
    #[test]
    fn a_missing_or_out_of_order_segment_is_refused_and_cancels() {
        use MmcSegmentPlace::{First, Later};
        let mut r = MmcCommandReassembler::new();
        let first = segment(First, 2, &[1, 2]);
        let second = segment(Later, 1, &[3, 4]);
        let third = segment(Later, 0, &[5]);
        // The middle one is missing.
        assert_eq!(
            reassemble(&mut r, &[first, third]),
            Err(MmcSegmentError::OutOfOrder {
                expected: MmcSegmentCount::of(1),
                got: MmcSegmentCount::of(0)
            })
        );
        assert!(!r.in_progress());
        // Out of order: the third before the second.
        r.push(first).unwrap();
        assert_eq!(
            r.push(third).unwrap_err(),
            MmcSegmentError::OutOfOrder {
                expected: MmcSegmentCount::of(1),
                got: MmcSegmentCount::of(0)
            }
        );
        // The string was cancelled, so the second now has no first.
        assert_eq!(r.push(second).unwrap_err(), MmcSegmentError::MissingFirst);
        assert_eq!(MmcErrorKind::Segmentation.wire(), 0x09);
    }

    /// RP-013 p.40 (c): a later segment with no first one is refused; and a first
    /// inside a string is out of order, which cancels the string and is not taken
    /// in, so pushing it again starts its string.
    #[test]
    fn a_later_segment_needs_a_first_and_a_first_restarts_after_its_refusal() {
        use MmcSegmentPlace::{First, Later};
        let mut r = MmcCommandReassembler::new();
        assert_eq!(
            r.push(segment(Later, 0, &[1])).unwrap_err(),
            MmcSegmentError::MissingFirst
        );
        r.push(segment(First, 1, &[0x02])).unwrap();
        assert!(r.in_progress());
        let restart = segment(First, 1, &[0x01]);
        assert_eq!(
            r.push(restart).unwrap_err(),
            MmcSegmentError::FirstInsideString
        );
        assert!(!r.in_progress());
        // The new string is whole from the refused first, with none of the old.
        assert!(r.push(restart).unwrap().is_none());
        let done = r.push(segment(Later, 0, &[0x02])).unwrap().unwrap();
        assert_eq!(
            done.iter().collect::<Vec<_>>(),
            [MmcCommand::Stop, MmcCommand::Play]
        );
    }

    /// A segment holds at most 45 string bytes: a message holds 48 and the
    /// segment command takes three, so 46 cannot be written.
    #[test]
    fn a_segment_holds_at_most_forty_five_bytes() {
        assert!(MmcSegmentBytes::new(&[0x02; 45]).is_some());
        assert!(MmcSegmentBytes::new(&[0x02; 46]).is_none());
        assert!(MmcSegmentBytes::new(&[0x80]).is_none());
        let full = segment(MmcSegmentPlace::First, 0, &[0x02; 45]);
        assert_eq!(segment_message(full).len(), 3 + MMC_MAX_DATA);
    }

    /// RP-013 p.40 (b): a COMMAND SEGMENT whose count is more than the bytes left
    /// in its message is a segment cut short, read as such (with the commands
    /// before it) and not as an unreadable message. No writer can name it: it is
    /// a `Universal`, not an `MmcCommand`.
    #[test]
    fn a_segment_whose_count_passes_the_message_is_truncated() {
        let truncated = |data: &[u8]| match heard(&framed(0x7F, 0x06, data)).message {
            Universal::MachineControlTruncated(before) => before.iter().collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert_eq!(truncated(&[0x53, 0x09, 0x01, 0x02]), []);
        assert_eq!(
            truncated(&[0x02, 0x01, 0x53, 0x09, 0x01]),
            [MmcCommand::Play, MmcCommand::Stop]
        );
        // A whole segment, and a cut-short command that is not a segment, are
        // not this.
        assert!(matches!(
            heard(&framed(0x7F, 0x06, &[0x53, 0x02, 0x00, 0x02])).message,
            Universal::MachineControl(_)
        ));
        assert!(!matches!(
            parse(&framed(0x7F, 0x06, &[0x44, 0x09, 0x01])),
            UniversalRead::Universal(Addressed {
                message: Universal::MachineControlTruncated(_),
                ..
            })
        ));
    }

    /// RP-013 p.40: a non-segmented message when a later segment was due
    /// cancels the string.
    #[test]
    fn cancel_drops_the_string_in_progress() {
        use MmcSegmentPlace::{First, Later};
        let mut r = MmcCommandReassembler::new();
        r.push(segment(First, 1, &[0x02])).unwrap();
        r.cancel();
        assert_eq!(
            r.push(segment(Later, 0, &[0x01])).unwrap_err(),
            MmcSegmentError::MissingFirst
        );
    }

    /// A string that ends inside a command is not a whole number of commands
    /// (RP-013 p.6): `44 06 01` is a LOCATE without its time.
    #[test]
    fn a_string_that_ends_inside_a_command_is_malformed() {
        use MmcSegmentPlace::{First, Later};
        let mut r = MmcCommandReassembler::new();
        assert_eq!(
            reassemble(
                &mut r,
                &[segment(First, 1, &[0x44, 0x06]), segment(Later, 0, &[0x01])]
            ),
            Err(MmcSegmentError::Malformed)
        );
        assert!(!r.in_progress());
    }

    /// RESPONSE SEGMENT (RP-013 p.73) strings are fields: a Motion Control
    /// Tally (`48 03 05 44 01`) cut across two segments reads as one field.
    #[test]
    fn response_segments_reassemble_fields() {
        use MmcSegmentPlace::{First, Later};
        let mut r = MmcResponseReassembler::new();
        assert!(
            r.push(segment(First, 1, &[0x48, 0x03, 0x05]))
                .unwrap()
                .is_none()
        );
        let done = r.push(segment(Later, 0, &[0x44, 0x01])).unwrap().unwrap();
        let [MmcInfoField::MotionControlTally(tally)] = done.iter().collect::<Vec<_>>()[..] else {
            panic!()
        };
        assert_eq!(tally.state, MotionControlState::Rewind);
    }

    /// A string splits into 45-byte segments (a message holds 48 bytes, and
    /// COMMAND SEGMENT takes its code, count and identification), counting down
    /// from the first, and the pieces join back to the string.
    #[test]
    fn a_long_string_splits_into_counted_segments_that_join_back() {
        let string: Vec<u8> = (0..100u8).map(|i| i & 0x7F).collect();
        let segments: Vec<MmcSegment> = MmcSegments::new(&string).unwrap().collect();
        let shape: Vec<_> = segments
            .iter()
            .map(|s| (s.place(), s.remaining().get(), s.bytes().as_bytes().len()))
            .collect();
        assert_eq!(
            shape,
            [
                (MmcSegmentPlace::First, 2, 45),
                (MmcSegmentPlace::Later, 1, 45),
                (MmcSegmentPlace::Later, 0, 10)
            ]
        );
        let joined: Vec<u8> = segments
            .iter()
            .flat_map(|s| s.bytes().as_bytes().to_vec())
            .collect();
        assert_eq!(joined, string);
        // Each is a message of at most 48 data bytes.
        for s in segments {
            assert!(segment_message(s).len() <= 3 + MMC_MAX_DATA);
        }
        assert_eq!(MmcSegments::new(&string).unwrap().len(), 3);
    }

    /// The identification byte counts down from 63, so 64 segments of 45 bytes
    /// are the most; one byte more, or a byte above 127, is refused.
    #[test]
    fn the_longest_segmented_string_is_sixty_four_segments() {
        let longest = vec![0x02; 2880];
        let segments: Vec<MmcSegment> = MmcSegments::new(&longest).unwrap().collect();
        assert_eq!(segments.len(), 64);
        assert_eq!(segments[0].remaining().get(), 63);
        assert_eq!(segments[63].remaining().get(), 0);
        let mut r = MmcCommandReassembler::new();
        let done = reassemble(&mut r, &segments).unwrap().unwrap();
        assert_eq!(done.len(), 2880);
        assert_eq!(
            MmcSegments::new(&[0x02; 2881]).unwrap_err(),
            MmcSegmentsRefused::TooLong
        );
        assert_eq!(
            MmcSegments::new(&[0x80]).unwrap_err(),
            MmcSegmentsRefused::NotSevenBit
        );
        assert_eq!(MmcSegments::new(&[]).unwrap().count(), 0);
    }

    /// RP-013 p.5: the first segment of a string carries bit 6.
    #[test]
    fn a_segment_place_is_bit_six_of_its_byte() {
        assert_eq!(MmcSegmentPlace::Later.wire(), 0);
        assert_eq!(MmcSegmentPlace::First.wire(), 1);
    }

    /// A position is the label of the frame it falls in and the hundredths of
    /// the way through it: 12.5 frames is frame 12 and 50 subframes, a whole
    /// frame is 0, and 24.999 frames is 99, never 100.
    #[test]
    fn a_position_splits_into_a_label_and_hundredths_of_a_frame() {
        let at = |seconds: f64| MmcPosition::from_seconds(Seconds::new(seconds), MtcRate::Fps25);
        let half = at(0.5).unwrap();
        assert_eq!(half.label.time_code().timecode().frames(), 12);
        assert_eq!(half.subframes.get(), 50);
        let whole = at(2.0).unwrap();
        assert_eq!(
            (
                whole.label.time_code().timecode().seconds().get(),
                whole.label.time_code().timecode().frames()
            ),
            (2, 0)
        );
        assert_eq!(whole.subframes.get(), 0);
        let nearly = at(24.999 / 25.0).unwrap();
        assert_eq!(nearly.label.time_code().timecode().frames(), 24);
        assert_eq!(nearly.subframes.get(), 99);
    }

    /// A position is a time from zero up to a day: before zero, NaN and a day
    /// or more name no label.
    #[test]
    fn a_position_is_within_one_day() {
        let at = |seconds: f64| MmcPosition::from_seconds(Seconds::new(seconds), MtcRate::Fps30);
        assert_eq!(at(-3.0), None);
        assert_eq!(at(f64::NAN), None);
        assert_eq!(at(0.0).unwrap().label.time_code().timecode().frames(), 0);
        assert_eq!(
            at(86_399.0)
                .unwrap()
                .label
                .time_code()
                .timecode()
                .hours()
                .get(),
            23
        );
        assert_eq!(at(86_400.0), None);
    }
}

/// RP-013 p.8: a subframe count is hundredths of a frame, 0 to 99.
#[test]
fn a_subframe_count_keeps_the_range_contract() {
    use tutti_midi_protocol::MmcSubframes;
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: MmcSubframes::new,
        get: MmcSubframes::get,
        of: Some(MmcSubframes::of),
        valid: &[0, 50, 99],
        invalid: &[100, 127, 255],
    });
}

/// The ranged ids keep the range-checked contract: `00` to `7E`, and serde
/// refuses `7F` and every byte above.
#[test]
fn a_device_id_and_a_slot_keep_the_range_contract() {
    use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
    use tutti_midi_protocol::MmcSlot;
    let ids = RangeContract {
        new: SysexDeviceId::new,
        get: SysexDeviceId::get,
        of: Some(SysexDeviceId::of),
        valid: &[0, 1, 0x7E],
        invalid: &[0x7F, 0x80, 0xFF],
    };
    check_range_contract(&ids);
    check_range_serde(&ids);
    let slots = RangeContract {
        new: MmcSlot::new,
        get: MmcSlot::get,
        of: Some(MmcSlot::of),
        valid: &[0, 1, 0x7E],
        invalid: &[0x7F, 0x80, 0xFF],
    };
    check_range_contract(&slots);
    check_range_serde(&slots);
}

/// RP-013 pp.11, 30: a speed's shift is three bits, its mantissa 17, a step's
/// count and a segment count six, and an audio track of the bitmap is 1 to 317.
#[test]
fn the_machine_control_fields_keep_the_range_contract() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    use tutti_midi_protocol::{
        MmcAudioTrack, MmcMantissa, MmcSegmentCount, MmcShift, MmcStepCount,
    };
    check_range_contract(&RangeContract {
        new: MmcShift::new,
        get: MmcShift::get,
        of: Some(MmcShift::of),
        valid: &[0, 7],
        invalid: &[8, 255],
    });
    check_range_contract(&RangeContract {
        new: MmcMantissa::new,
        get: MmcMantissa::get,
        of: Some(MmcMantissa::of),
        valid: &[0, 1 << 14, 0x1_FFFF],
        invalid: &[0x2_0000, u32::MAX],
    });
    check_range_contract(&RangeContract {
        new: MmcStepCount::new,
        get: MmcStepCount::get,
        of: Some(MmcStepCount::of),
        valid: &[0, 63],
        invalid: &[64, 255],
    });
    check_range_contract(&RangeContract {
        new: MmcSegmentCount::new,
        get: MmcSegmentCount::get,
        of: Some(MmcSegmentCount::of),
        valid: &[0, 63],
        invalid: &[64, 255],
    });
    check_range_contract(&RangeContract {
        new: MmcAudioTrack::new,
        get: MmcAudioTrack::get,
        of: Some(MmcAudioTrack::of),
        valid: &[1, 2, 317],
        invalid: &[0, 318, u16::MAX],
    });
}
