//! Lyric Data and Ruby Data (M2-104 sections 7.5.10 and 7.5.12): one syllable
//! is a Complete packet or a run ending in End, the bytes after its end are
//! zero, a trailing space, CR or CR LF marks the end of a word, line or
//! paragraph, and an all-zero Complete packet is a melisma. Both texts share
//! the Flex text format, so each property is checked for both.

use tutti_ump::{
    Flex, FlexAddress, FlexTextAssembler, FlexTextKind, FlexTextStep, Form, Message, MidiGroup,
    Ump, flex_text_packets,
};

const G0: MidiGroup = MidiGroup::of(0);
const KINDS: [FlexTextKind; 2] = [FlexTextKind::Lyrics, FlexTextKind::Ruby];

fn parts(kind: FlexTextKind, text: &str) -> Vec<(Form, [u8; 12])> {
    flex_text_packets(G0, FlexAddress::Group, kind, text)
        .unwrap()
        .map(|ump| match ump.decode() {
            Ok(Message::Flex(_, Flex::Text { form, bytes, .. })) => (form, bytes),
            other => panic!("not Flex text: {other:?}"),
        })
        .collect()
}

fn packet(kind: FlexTextKind, form: Form, bytes: [u8; 12]) -> Ump {
    Ump::from(Message::Flex(
        G0,
        Flex::Text {
            address: FlexAddress::Group,
            kind,
            form,
            bytes,
        },
    ))
}

fn read(kind: FlexTextKind, text: &str) -> String {
    let mut a = FlexTextAssembler::new(vec![0; 512], 512, 1);
    let mut got = None;
    for ump in flex_text_packets(G0, FlexAddress::Group, kind, text).unwrap() {
        if let FlexTextStep::Done(t) = a.push(ump) {
            assert_eq!(t.kind, kind);
            got = Some(t.text.to_owned());
        }
    }
    got.unwrap()
}

/// A syllable is one Complete packet, or a Start, Continue* and End: the last
/// packet of every text is a Complete or an End, never a Start or Continue.
/// Spec: m2-104/7.5.10#3, m2-104/7.5.12#3, m2-104/7.5.2#1, m2-104/7.5.10#4, m2-104/7.5.12#4
#[test]
fn a_syllable_is_a_complete_packet_or_a_run_ending_in_end() {
    for kind in KINDS {
        for len in 1..=60 {
            let text = "a".repeat(len);
            let forms: Vec<Form> = parts(kind, &text).iter().map(|p| p.0).collect();
            if len <= 12 {
                assert_eq!(forms, [Form::Complete], "{kind:?} {len}");
            } else {
                assert_eq!(forms[0], Form::Start, "{kind:?} {len}");
                assert_eq!(*forms.last().unwrap(), Form::End, "{kind:?} {len}");
                assert!(
                    forms[1..forms.len() - 1]
                        .iter()
                        .all(|f| *f == Form::Continue),
                    "{kind:?} {len}"
                );
            }
        }
    }
}

/// When the text ends inside the packet, the bytes after it are zero, and
/// the zero bytes are only in a Complete or End packet.
/// Spec: m2-104/7.5.10#6, m2-104/7.5.10#10, m2-104/7.5.12#6, m2-104/7.5.12#10
#[test]
fn bytes_after_the_end_of_a_syllable_are_zero() {
    for kind in KINDS {
        for text in ["la", "word ", "line\r", "paragraph\r\n", "thirteen byte"] {
            let packets = parts(kind, text);
            let (form, last) = packets.last().unwrap();
            assert!(matches!(form, Form::Complete | Form::End));
            let used = text.len() - 12 * (packets.len() - 1);
            assert_eq!(
                last[..used],
                text.as_bytes()[text.len() - used..],
                "{text:?}"
            );
            assert!(last[used..].iter().all(|b| *b == 0), "{text:?}");
            for (form, bytes) in &packets[..packets.len() - 1] {
                assert!(matches!(form, Form::Start | Form::Continue));
                assert!(bytes.iter().all(|b| *b != 0), "{text:?}");
            }
        }
    }
}

/// A Start packet never holds a zero byte: the writer produces none, and the
/// assembler refuses a message whose Start holds one.
/// Spec: m2-104/7.5.10#11, m2-104/7.5.12#11
#[test]
fn a_start_packet_never_holds_a_zero() {
    for kind in KINDS {
        for len in 13..40 {
            let packets = parts(kind, &"x".repeat(len));
            assert!(packets[0].1.iter().all(|b| *b != 0), "{kind:?} {len}");
        }
        let mut a = FlexTextAssembler::new(vec![0; 64], 64, 1);
        let mut start = [b'a'; 12];
        start[7] = 0;
        a.push(packet(kind, Form::Start, start));
        let done = a.push(packet(kind, Form::End, [b'b'; 12]));
        assert!(!matches!(done, FlexTextStep::Done(_)), "{kind:?}");
        assert_eq!(a.invalid(), 1, "{kind:?}");
    }
}

/// A space, CR or CR LF at the end of the text is part of it: the reader
/// returns it, so the end of a word, line or paragraph is not lost to the
/// zero padding that follows.
/// Spec: m2-104/7.5.10#7, m2-104/7.5.10#8, m2-104/7.5.10#9, m2-104/7.5.12#7, m2-104/7.5.12#8, m2-104/7.5.12#9
#[test]
fn a_trailing_space_cr_or_cr_lf_is_kept() {
    for kind in KINDS {
        for text in [
            "la ",
            "la\r",
            "la\r\n",
            "a long syllable ",
            "a long line\r",
            " ",
            "\r\n",
        ] {
            assert_eq!(read(kind, text), text, "{kind:?} {text:?}");
        }
        // A syllable with no terminator reads back as it is: the next one
        // continues the same word.
        assert_eq!(read(kind, "la"), "la");
    }
}

/// An all-zero Complete packet is a melisma: an empty text that ends no word
/// and starts no syllable, and a word-end space after it is a text of its own,
/// with CR and LF permitted in it.
/// Spec: m2-104/7.5.10.1#1, m2-104/7.5.10.1#3, m2-104/7.5.12.1#1, m2-104/7.5.12.1#3
#[test]
fn an_all_zero_complete_packet_is_a_melisma() {
    for kind in KINDS {
        let mut a = FlexTextAssembler::new(vec![0; 64], 64, 1);
        let FlexTextStep::Done(melisma) = a.push(packet(kind, Form::Complete, [0; 12])) else {
            panic!("{kind:?}");
        };
        assert_eq!((melisma.kind, melisma.text), (kind, ""));
        assert_eq!(read(kind, ""), "");
        assert_eq!(read(kind, " \r\n"), " \r\n");
    }
}

/// A Recording/Concert Date is text in the common format (M2-104 section
/// 7.5.9.2): status bank 1, status 0x0B, Complete for up to 12 bytes, else
/// Start, Continue and End with the bytes after the end zero.
/// Spec: m2-104/7.5.9.2#1
#[test]
fn a_recording_date_is_common_format_text() {
    let words = |date: &str| -> Vec<[u32; 4]> {
        flex_text_packets(G0, FlexAddress::Group, FlexTextKind::RecordingDate, date)
            .unwrap()
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
