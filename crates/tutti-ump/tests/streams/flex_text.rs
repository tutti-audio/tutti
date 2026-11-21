//! The Flex text writers (M2-104 section 7.5.9: Complete for up to 12 bytes,
//! else Start, Continue, End, the last padded with zeros), and what each
//! entry point holds a message to.

use tutti_ump::{
    Entry, EntryError, FLEX_TEXT_MAX_LEN, Flex, FlexAddress, FlexTextAssembler, FlexTextError,
    FlexTextKind, FlexTextStep, Form, Iso8601Date, LyricKind, Message, MidiChannel, MidiGroup,
    PlainTextKind, Syllable, SyllableBody, SyllableEnd, Text, Ump, flex_clip_name_packets,
    flex_lyric_packets, flex_project_name_packets, flex_recording_date_packets, flex_text_packets,
};

fn text(s: &str) -> Text {
    Text::try_from(s).unwrap()
}

fn packets(s: &str) -> Vec<(Form, [u8; 12])> {
    flex_text_packets(
        MidiGroup::of(3),
        FlexAddress::Group,
        PlainTextKind::Composer,
        &text(s),
    )
    .map(|ump| {
        let Ok(Message::Flex(
            group,
            Flex::Text {
                address,
                kind,
                form,
                bytes,
            },
        )) = ump.decode()
        else {
            panic!("not Flex text");
        };
        assert_eq!(group, MidiGroup::of(3));
        assert_eq!(address, FlexAddress::Group);
        assert_eq!(kind, FlexTextKind::Composer);
        (form, bytes)
    })
    .collect()
}

/// Spec: m2-104/7.5.1#2
#[test]
fn twelve_bytes_are_one_complete_packet() {
    assert_eq!(
        packets("twelve bytes"),
        [(Form::Complete, *b"twelve bytes")]
    );
}

#[test]
fn an_empty_text_is_one_complete_packet_of_zeros() {
    assert_eq!(packets(""), [(Form::Complete, [0; 12])]);
}

/// Spec: m2-104/7.5.1#2, m2-104/7.5.1#3
#[test]
fn a_long_text_is_start_continue_end() {
    let got = packets("0123456789ab0123456789abXYZ");
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], (Form::Start, *b"0123456789ab"));
    assert_eq!(got[1], (Form::Continue, *b"0123456789ab"));
    assert_eq!(got[2].0, Form::End);
}

/// Spec: m2-104/7.5.9#2
#[test]
fn the_last_packet_is_padded_with_zeros() {
    let got = packets("0123456789abXYZ");
    assert_eq!(got[1], (Form::End, *b"XYZ\0\0\0\0\0\0\0\0\0"));
}

#[test]
fn the_length_is_exact() {
    for (s, n) in [
        ("", 1),
        ("a", 1),
        ("0123456789ab", 1),
        ("0123456789abc", 2),
        ("0123456789ab0123456789ab", 2),
    ] {
        let name = text(s);
        let it = flex_project_name_packets(MidiGroup::of(0), &name);
        assert_eq!(it.len(), n, "{s}");
        assert_eq!(it.count(), n, "{s}");
    }
}

/// Zero is the padding, so a text holding one is refused.
#[test]
fn a_nul_is_refused() {
    assert_eq!(Text::try_from("a\0b").err(), Some(FlexTextError::Nul));
}

/// A Flex Data message has at most 32 packets: a Start, 30 Continue and an
/// End carry the longest text, and one byte more is refused.
/// Spec: m2-104/7.5.1#4
#[test]
fn a_message_has_at_most_32_packets() {
    let longest = "x".repeat(FLEX_TEXT_MAX_LEN);
    let name = text(&longest);
    let umps = flex_project_name_packets(MidiGroup::of(0), &name);
    assert_eq!(umps.len(), 32);
    assert_eq!(
        Text::try_from(&*format!("{longest}x")).err(),
        Some(FlexTextError::TooLong)
    );
}

type Seen = Vec<(MidiGroup, FlexAddress, FlexTextKind, Form)>;

fn seen(packets: impl Iterator<Item = Ump>) -> Seen {
    packets
        .map(|ump| match ump.decode() {
            Ok(Message::Flex(
                group,
                Flex::Text {
                    address,
                    kind,
                    form,
                    ..
                },
            )) => (group, address, kind, form),
            other => panic!("not Flex text: {other:?}"),
        })
        .collect()
}

/// Section 7.5 and 7.5.1: a Flex Data message spans several packets and is
/// addressed to a Channel or to the Group, and the Start, Continue and End
/// framing is the one every message of type 0xD uses: each text kind of both
/// status banks, at either address, is a Start, a Continue and an End, and its
/// packets say the kind and the address they were written with.
/// Spec: m2-104/7.5#1
#[test]
fn every_text_kind_is_framed_alike_at_a_channel_or_the_group() {
    use PlainTextKind as P;
    const PLAIN: [PlainTextKind; 13] = [
        P::UnknownMetadata,
        P::CompositionName,
        P::Copyright,
        P::Composer,
        P::Lyricist,
        P::Arranger,
        P::Publisher,
        P::PrimaryPerformer,
        P::AccompanyingPerformer,
        P::RecordingLocation,
        P::UnknownPerformance,
        P::LyricsLanguage,
        P::RubyLanguage,
    ];
    // 25 bytes (twelve, twelve and one): a date, a syllable and a name at once.
    let s = "2026-11-15T10:30:00+09:00";
    let group = MidiGroup::of(5);
    let syllable = Syllable {
        body: SyllableBody::try_from(s).unwrap(),
        end: SyllableEnd::None,
    };
    let date = s.parse::<Iso8601Date>().unwrap();
    for address in [FlexAddress::Group, FlexAddress::Channel(MidiChannel::of(9))] {
        let mut all: Vec<(FlexTextKind, Vec<Ump>)> = PLAIN
            .iter()
            .map(|&k| {
                (
                    k.into(),
                    flex_text_packets(group, address, k, &text(s)).collect(),
                )
            })
            .collect();
        all.push((
            FlexTextKind::RecordingDate,
            flex_recording_date_packets(group, address, &date).collect(),
        ));
        for k in [LyricKind::Lyrics, LyricKind::Ruby] {
            all.push((
                k.into(),
                flex_lyric_packets(group, address, k, &syllable).collect(),
            ));
        }
        if address == FlexAddress::Group {
            all.push((
                FlexTextKind::ProjectName,
                flex_project_name_packets(group, &text(s)).collect(),
            ));
            all.push((
                FlexTextKind::ClipName,
                flex_clip_name_packets(group, &text(s)).collect(),
            ));
        }
        for (kind, packets) in all {
            let want: Seen = [Form::Start, Form::Continue, Form::End]
                .into_iter()
                .map(|form| (group, address, kind, form))
                .collect();
            assert_eq!(seen(packets.into_iter()), want, "{kind:?} at {address:?}");
        }
    }
}

/// Table 16: the Project Name and the MIDI Clip Name have their Address field
/// set to 1 (Group). Their entry points take no address, so a Channel is not
/// written.
/// Spec: m2-104/7.5.9.1#1, m2-104/7.5.9.1#2
#[test]
fn project_and_clip_names_are_addressed_to_the_group() {
    let group = MidiGroup::of(2);
    let name = text("Demo");
    assert_eq!(
        seen(flex_project_name_packets(group, &name)),
        [(
            group,
            FlexAddress::Group,
            FlexTextKind::ProjectName,
            Form::Complete
        )]
    );
    assert_eq!(
        seen(flex_clip_name_packets(group, &name)),
        [(
            group,
            FlexAddress::Group,
            FlexTextKind::ClipName,
            Form::Complete
        )]
    );
}

/// M2-104 section 7.5.9.1, Multiple Entities/Names: entries in one message
/// are separated by a comma, as in the specification's own example "John
/// Lennon, Paul McCartney"; an entry holding a comma would read as two.
/// Spec: m2-104/7.5.9.1#5
#[test]
fn several_entities_in_one_text_are_separated_by_commas() {
    let list = |entries: &[&str]| {
        let entries: Vec<Entry> = entries
            .iter()
            .map(|e| Entry::try_from(*e).unwrap())
            .collect();
        Text::try_from(&entries[..])
    };
    assert_eq!(
        list(&["John Lennon", "Paul McCartney"]).unwrap().as_str(),
        "John Lennon, Paul McCartney"
    );
    assert_eq!(list(&["Solo"]).unwrap().as_str(), "Solo");
    assert_eq!(list(&[]).unwrap().as_str(), "");
    assert_eq!(
        Entry::try_from("Lennon, John").err(),
        Some(EntryError::Comma)
    );
    // An empty entry would leave two separators in a row.
    assert_eq!(Entry::try_from("").err(), Some(EntryError::Empty));
    // A list past the longest text is refused.
    let long = "x".repeat(200);
    assert_eq!(list(&[&long, &long]).err(), Some(FlexTextError::TooLong));
}

/// M2-104 section 7.5.9.2: the date text is ISO 8601, a date with an optional
/// time: a year, a month, a calendar, ordinal or week date, the last three
/// with a time, a fraction, `24:00` for the end of the day and an offset, in
/// the basic form or the extended one and never both.
/// Spec: m2-104/7.5.9.2#2
#[test]
fn a_recording_date_is_an_iso_8601_date_with_an_optional_time() {
    for good in [
        "2026",
        "2026-11",
        "2026-11-15",
        "20261115",
        "2024-02-29",
        "2026-319",
        "2026319",
        "2024-366",
        "2026-W46",
        "2026W46",
        "2026-W46-7",
        "2026W467",
        "2026-W53",
        "2020-W53",
        "2000-02-29",
        "2026-11-15T10",
        "2026-11-15T10:30",
        "2026-11-15T10:30:00",
        "2026-11-15T10:30:00.25Z",
        "2026-11-15T10,5Z",
        "2026-11-15T10:30:00+09:00",
        "2026-11-15T10:30:00-05:30",
        "2026-11-15T10:30:00+09",
        "20261115T103000Z",
        "20261115T103000+0900",
        "2026-11-15T23:59:60Z",
        "2026-11-15T24:00",
        "2026-11-15T24:00:00.000Z",
        "2026-319T10:30:00+09:00",
        "2026-W46-7T10:30Z",
        "2026W467T1030Z",
    ] {
        assert!(good.parse::<Iso8601Date>().is_ok(), "{good}");
        assert_eq!(good.parse::<Iso8601Date>().unwrap().as_str(), good);
    }
    for bad in [
        "",
        "202",
        "20260",
        "11/15/2026",
        "15 Nov 2026",
        "2026-13",
        "2026-00",
        "2026-00-10",
        "2026-13-01",
        "2026-11-31",
        "2025-02-29",
        "2100-02-29",
        "2026-1115",
        "2026-11-5",
        "2026-000",
        "2026-366",
        "2026-W00",
        "2026-W54",
        "2025-W53",
        "2026-W46-0",
        "2026-W46-8",
        "2026W46-7",
        "2026-W467",
        "2026-11T10",
        "2026T10",
        "2026-W46T10",
        "2026-11-15 10:30",
        "2026-11-15T",
        "2026-11-15T25",
        "2026-11-15T24:01",
        "2026-11-15T24:00:00.5",
        "2026-11-15T10:60",
        "2026-11-15T10:30:61",
        "2026-11-15T10:30:00+9",
        "2026-11-15T10:30:00Zulu",
        "2026-11-15T10:30:00.",
        "2026-11-15T1030",
        "20261115T10:30",
        "2026-11-15T10:30:00+0900",
        "20261115T103000+09:00",
        "2026-11-15+09:00",
    ] {
        assert!(bad.parse::<Iso8601Date>().is_err(), "{bad:?}");
    }
}

/// A parsed date reads back, over the wire, as the text it was parsed from.
#[test]
fn a_recording_date_keeps_its_text() {
    let written = "2026-11-15T10:30:00+09:00";
    let date = written.parse::<Iso8601Date>().unwrap();
    let mut assembler = FlexTextAssembler::new(vec![0; 64], 64, 1);
    let mut read = None;
    for ump in flex_recording_date_packets(MidiGroup::of(0), FlexAddress::Group, &date) {
        if let FlexTextStep::Done(t) = assembler.push(ump) {
            read = Some(t.text.to_owned());
        }
    }
    assert_eq!(read.as_deref(), Some(written));
}

/// A Recording/Concert Date is text in the common format (M2-104 section
/// 7.5.9.2): status bank 1, status 0x0B, Complete for up to 12 bytes, else
/// Start, Continue and End with the bytes after the end zero.
/// Spec: m2-104/7.5.9.2#1
#[test]
fn a_recording_date_is_common_format_text() {
    let words = |date: &str| -> Vec<[u32; 4]> {
        let date = date.parse::<Iso8601Date>().unwrap();
        flex_recording_date_packets(MidiGroup::of(0), FlexAddress::Group, &date)
            .map(|u| u.0)
            .collect()
    };
    assert_eq!(
        words("2026-11-15"),
        [[0xD010_010B, 0x3230_3236, 0x2D31_312D, 0x3135_0000]]
    );
    assert_eq!(
        words("2026-11-15T10:30:00+09:00"),
        [
            [0xD050_010B, 0x3230_3236, 0x2D31_312D, 0x3135_5431],
            [0xD090_010B, 0x303A_3330, 0x3A30_302B, 0x3039_3A30],
            [0xD0D0_010B, 0x3000_0000, 0, 0],
        ]
    );
}
