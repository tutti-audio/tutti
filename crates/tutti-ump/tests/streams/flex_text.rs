//! `flex_text_packets`: a text as Flex Data packets (M2-104 section 7.5.9:
//! Complete for up to 12 bytes, else Start, Continue, End, the last padded
//! with zeros).

use tutti_ump::{
    FLEX_TEXT_MAX_LEN, Flex, FlexAddress, FlexTextKind, Form, Message, MidiChannel, MidiGroup,
    flex_text_packets,
};

fn packets(text: &str) -> Vec<(Form, [u8; 12])> {
    flex_text_packets(
        MidiGroup::of(3),
        FlexAddress::Group,
        FlexTextKind::Lyrics,
        text,
    )
    .unwrap()
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
        assert_eq!(kind, FlexTextKind::Lyrics);
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
    for (text, n) in [
        ("", 1),
        ("a", 1),
        ("0123456789ab", 1),
        ("0123456789abc", 2),
        ("0123456789ab0123456789ab", 2),
    ] {
        let it = flex_text_packets(
            MidiGroup::of(0),
            FlexAddress::Group,
            FlexTextKind::ClipName,
            text,
        )
        .unwrap();
        assert_eq!(it.len(), n, "{text}");
        assert_eq!(it.count(), n, "{text}");
    }
}

/// Zero is the padding, so a text holding one is refused.
#[test]
fn a_nul_is_refused() {
    assert!(
        flex_text_packets(
            MidiGroup::of(0),
            FlexAddress::Group,
            FlexTextKind::Lyrics,
            "a\0b"
        )
        .is_none()
    );
}

/// A Flex Data message has at most 32 packets: a Start, 30 Continue and an
/// End carry the longest text, and one byte more is refused.
/// Spec: m2-104/7.5.1#4
#[test]
fn a_message_has_at_most_32_packets() {
    let longest = "x".repeat(FLEX_TEXT_MAX_LEN);
    let umps = flex_text_packets(
        MidiGroup::of(0),
        FlexAddress::Group,
        FlexTextKind::Lyrics,
        &longest,
    )
    .unwrap();
    assert_eq!(umps.len(), 32);
    assert!(
        flex_text_packets(
            MidiGroup::of(0),
            FlexAddress::Group,
            FlexTextKind::Lyrics,
            &format!("{longest}x")
        )
        .is_none()
    );
}

/// Section 7.5 and 7.5.1: a Flex Data message spans several packets and is
/// addressed to a Channel or to the Group, and the Start, Continue and End
/// framing is the one every message of type 0xD uses: each text kind of both
/// status banks, at either address, is a Start, a Continue and an End, and its
/// packets say the kind and the address they were written with.
/// Spec: m2-104/7.5#1, m2-104/7.5.1#1
#[test]
fn every_text_kind_is_framed_alike_at_a_channel_or_the_group() {
    const KINDS: [FlexTextKind; 18] = [
        FlexTextKind::UnknownMetadata,
        FlexTextKind::ProjectName,
        FlexTextKind::CompositionName,
        FlexTextKind::ClipName,
        FlexTextKind::Copyright,
        FlexTextKind::Composer,
        FlexTextKind::Lyricist,
        FlexTextKind::Arranger,
        FlexTextKind::Publisher,
        FlexTextKind::PrimaryPerformer,
        FlexTextKind::AccompanyingPerformer,
        FlexTextKind::RecordingLocation,
        FlexTextKind::UnknownPerformance,
        FlexTextKind::Lyrics,
        FlexTextKind::LyricsLanguage,
        FlexTextKind::Ruby,
        FlexTextKind::RubyLanguage,
        FlexTextKind::RecordingDate,
    ];
    // 25 bytes: twelve, twelve and one.
    let text = "0123456789ab0123456789abX";
    for kind in KINDS {
        for address in [FlexAddress::Group, FlexAddress::Channel(MidiChannel::of(9))] {
            let packets = flex_text_packets(MidiGroup::of(5), address, kind, text).unwrap();
            let seen: Vec<_> = packets
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
                .collect();
            let want: Vec<_> = [Form::Start, Form::Continue, Form::End]
                .into_iter()
                .map(|form| (MidiGroup::of(5), address, kind, form))
                .collect();
            assert_eq!(seen, want, "{kind:?} at {address:?}");
        }
    }
}
