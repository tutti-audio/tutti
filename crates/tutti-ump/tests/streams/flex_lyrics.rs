//! Lyric Data and Ruby Data (M2-104 sections 7.5.10 and 7.5.12): one syllable
//! is a Complete packet or a run ending in End, the bytes after its end are
//! zero, a trailing space, CR or CR LF marks the end of a word, line or
//! paragraph, and an all-zero Complete packet is a melisma. Both texts share
//! the Flex text format, so each property is checked for both.

use tutti_ump::{
    Flex, FlexAddress, FlexTextAssembler, FlexTextError, FlexTextKind, FlexTextStep, Form,
    LyricKind, Message, MidiGroup, Syllable, SyllableBody, SyllableEnd, SyllableError, Ump,
    WordEnd, flex_lyric_packets, flex_melisma_end_packets,
};

const G0: MidiGroup = MidiGroup::of(0);
const KINDS: [LyricKind; 2] = [LyricKind::Lyrics, LyricKind::Ruby];

/// The syllable of `text`: its body, and the space, CR or CR LF it ends with.
fn syllable(text: &str) -> Syllable {
    let body = text.trim_end_matches([' ', '\r', '\n']);
    let end = match &text[body.len()..] {
        "" => SyllableEnd::None,
        " " => SyllableEnd::Word,
        "\r" => SyllableEnd::Line,
        "\r\n" => SyllableEnd::Paragraph,
        other => panic!("{other:?} is no syllable end"),
    };
    Syllable {
        body: SyllableBody::try_from(body).unwrap(),
        end,
    }
}

fn parts(kind: LyricKind, text: &str) -> Vec<(Form, [u8; 12])> {
    flex_lyric_packets(G0, FlexAddress::Group, kind, &syllable(text))
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

fn read_all(packets: impl Iterator<Item = Ump>) -> Vec<String> {
    let mut a = FlexTextAssembler::new(vec![0; 512], 512, 1);
    let mut got = vec![];
    for ump in packets {
        if let FlexTextStep::Done(t) = a.push(ump) {
            got.push(t.text.to_owned());
        }
    }
    got
}

fn read(kind: LyricKind, text: &str) -> String {
    let got = read_all(flex_lyric_packets(
        G0,
        FlexAddress::Group,
        kind,
        &syllable(text),
    ));
    assert_eq!(got.len(), 1);
    got.into_iter().next().unwrap()
}

/// A syllable is one Complete packet, or a Start, Continue* and End: the last
/// packet of every text is a Complete or an End, never a Start or Continue.
/// Spec: m2-104/7.5.10#3, m2-104/7.5.12#3, m2-104/7.5.10#4, m2-104/7.5.12#4
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
        for text in ["la", "word ", "line\r", "paragraph\r\n", "thirteen-byte"] {
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
        a.push(packet(kind.into(), Form::Start, start));
        let done = a.push(packet(kind.into(), Form::End, [b'b'; 12]));
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
            "extraordinary ",
            "supercalifragilistic\r",
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
        let FlexTextStep::Done(melisma) = a.push(packet(kind.into(), Form::Complete, [0; 12]))
        else {
            panic!("{kind:?}");
        };
        assert_eq!((melisma.kind, melisma.text), (FlexTextKind::from(kind), ""));
        assert_eq!(read(kind, ""), "");
        let word_end = read_all(flex_melisma_end_packets(
            G0,
            FlexAddress::Group,
            kind,
            WordEnd::Paragraph,
        ));
        assert_eq!(word_end, ["", " \r\n"]);
    }
}

/// Each message is one syllable (or a one-syllable word): a space, a carriage
/// return or a line feed inside the body would make it more than one word or
/// line, so a body cannot hold one; the end of a word, line or paragraph is
/// the syllable's end, at the end of the text only.
/// Spec: m2-104/7.5.10#1, m2-104/7.5.12#1
#[test]
fn a_message_holds_one_syllable() {
    for good in ["hel", "sixty-four", "hel-", "-", ""] {
        assert!(SyllableBody::try_from(good).is_ok(), "{good:?}");
    }
    for bad in ["hello world", " la", "la ", "a\rb", "a\nb", "\n"] {
        assert_eq!(
            SyllableBody::try_from(bad).err(),
            Some(SyllableError::Separator),
            "{bad:?}"
        );
    }
    assert_eq!(
        SyllableBody::try_from("a\0").err(),
        Some(SyllableError::Text(FlexTextError::Nul))
    );
    assert_eq!(
        SyllableBody::try_from(&*"x".repeat(383)).err(),
        Some(SyllableError::Text(FlexTextError::TooLong))
    );
    // The longest syllable with the longest end still fits 32 packets.
    let longest = Syllable {
        body: SyllableBody::try_from(&*"x".repeat(382)).unwrap(),
        end: SyllableEnd::Paragraph,
    };
    for kind in KINDS {
        let packets = flex_lyric_packets(G0, FlexAddress::Group, kind, &longest);
        assert_eq!(packets.len(), 32);
    }
    for (end, text) in [
        (SyllableEnd::None, "la"),
        (SyllableEnd::Word, "la "),
        (SyllableEnd::Line, "la\r"),
        (SyllableEnd::Paragraph, "la\r\n"),
    ] {
        let la = Syllable {
            body: SyllableBody::try_from("la").unwrap(),
            end,
        };
        for kind in KINDS {
            assert_eq!(
                read_all(flex_lyric_packets(G0, FlexAddress::Group, kind, &la)),
                [text]
            );
        }
    }
}

/// A melisma that is the end of a word is followed immediately by a message
/// of a space, with a carriage return and a line feed when the line or the
/// paragraph ends: the melisma is the all-zero Complete packet and the next
/// packet is the space message.
/// Spec: m2-104/7.5.10.1#2, m2-104/7.5.12.1#2
#[test]
fn a_melisma_that_ends_a_word_is_followed_by_a_space() {
    for kind in KINDS {
        for (end, text) in [
            (WordEnd::Word, " "),
            (WordEnd::Line, " \r"),
            (WordEnd::Paragraph, " \r\n"),
        ] {
            let got: Vec<Ump> =
                flex_melisma_end_packets(G0, FlexAddress::Group, kind, end).collect();
            assert_eq!(got.len(), 2);
            assert_eq!(got[0], packet(kind.into(), Form::Complete, [0; 12]));
            let mut padded = [0; 12];
            padded[..text.len()].copy_from_slice(text.as_bytes());
            assert_eq!(got[1], packet(kind.into(), Form::Complete, padded));
        }
    }
}
