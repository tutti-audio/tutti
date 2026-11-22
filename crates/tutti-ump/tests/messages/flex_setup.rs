//! The Flex Data setup messages (M2-104 sections 7.5.3 to 7.5.5): each is a
//! Complete message addressed to the Group, pinned by the spec's bit
//! positions (Figures 70 to 72), and nothing else decodes as one.

use core::num::NonZeroU8;
use tutti_ump::{
    BarAccents, Flex, FlexTempo, FlexTimeSignature, Message, Metronome, MidiGroup, Ump,
};

/// The first word of a Complete, Group-addressed Flex Data message of Status
/// Bank 0: message type 0xD, Format 0 in bits 23-22, Address 1 in bits 21-20.
fn head(group: u32, status: u32) -> u32 {
    0xD010_0000 | group << 24 | status
}

fn setup(flex: Flex) -> Ump {
    Ump::from(Message::Flex(MidiGroup::of(2), flex))
}

/// The three setup messages as written carry Format 0 and Address 1, and are
/// addressed to a Group, so they ignore the Channel field.
/// Spec: m2-104/7.5.3#5, m2-104/7.5.3#6, m2-104/7.5.4#4, m2-104/7.5.4#5, m2-104/7.5.5#3, m2-104/7.5.5#4
#[test]
fn setup_messages_are_complete_and_group_addressed() {
    let tempo = setup(Flex::SetTempo(FlexTempo::new(0x0262_5A00)));
    assert_eq!(tempo.0, [head(2, 0x0000), 0x0262_5A00, 0, 0]);
    let signature = setup(Flex::SetTimeSignature(FlexTimeSignature {
        numerator: 7,
        denominator: 3,
        thirty_seconds: 8,
    }));
    assert_eq!(signature.0, [head(2, 0x0001), 0x0703_0800, 0, 0]);
    let metronome = setup(Flex::SetMetronome(Metronome {
        clocks_per_click: 24,
        accents: BarAccents::Two(NonZeroU8::new(3).unwrap(), NonZeroU8::new(2).unwrap()),
        subdivision_1: 2,
        subdivision_2: 0,
    }));
    assert_eq!(metronome.0, [head(2, 0x0002), 0x1803_0200, 0x0200_0000, 0]);
}

/// A Start, Continue or End form, or a Channel address, is not a setup
/// message: it does not decode as one.
/// Spec: m2-104/7.5.3#5, m2-104/7.5.3#6, m2-104/7.5.4#4, m2-104/7.5.4#5, m2-104/7.5.5#3, m2-104/7.5.5#4
#[test]
fn other_formats_and_addresses_are_not_setup_messages() {
    for status in [0x0000, 0x0001, 0x0002] {
        let valid = Ump([head(0, status), 0x0102_0304, 0, 0]);
        assert!(matches!(
            valid.decode(),
            Ok(Message::Flex(
                _,
                Flex::SetTempo(_) | Flex::SetTimeSignature(_) | Flex::SetMetronome(_)
            ))
        ));
        for form in 1..=3u32 {
            let bad = Ump([head(0, status) | form << 22, 0x0102_0304, 0, 0]);
            assert!(bad.decode().is_err(), "{status:#x} form {form}");
        }
        // Address 0 is a Channel; 2 and 3 are reserved.
        for address in [0u32, 2, 3] {
            let bad = Ump([
                head(0, status) & !(3 << 20) | address << 20,
                0x0102_0304,
                0,
                0,
            ]);
            assert!(
                !matches!(
                    bad.decode(),
                    Ok(Message::Flex(
                        _,
                        Flex::SetTempo(_) | Flex::SetTimeSignature(_) | Flex::SetMetronome(_)
                    ))
                ),
                "{status:#x} address {address}"
            );
        }
    }
}

fn part(n: u8) -> NonZeroU8 {
    NonZeroU8::new(n).unwrap()
}

/// The Bar Accent fields of Set Metronome (M2-104 §7.5.5): an undivided bar has
/// its beats in Part 1 and zero in Parts 2 and 3; a bar divides in up to three
/// parts, as 5/4 in 3 + 2 and 9/4 in 3 + 3 + 3 or 4 + 4 + 1; and the parts add
/// up to the beats of the bar. A shape with Part 1 zero, or a part after a zero
/// part, is read as `BarAccents::Reserved`, which keeps the rest of the message
/// and the bytes, and which no sender builds.
/// Spec: m2-104/7.5.5#5, m2-104/7.5.5#6, m2-104/7.5.5#7, m2-104/7.5.5#8
#[test]
fn the_bar_accents_are_the_parts_of_the_bar() {
    let word = |a: u8, b: u8, c: u8| [head(2, 0x0002), u32::from_be_bytes([24, a, b, c]), 0, 0];
    let accents = |a: u8, b: u8, c: u8| match Ump(word(a, b, c)).decode() {
        Ok(Message::Flex(_, Flex::SetMetronome(m))) => {
            assert_eq!(m.clocks_per_click, 24);
            m.accents
        }
        other => panic!("{other:?}"),
    };
    // 6/8 undivided: Part 1 is the six beats, Parts 2 and 3 zero.
    let undivided = BarAccents::Undivided(part(6));
    assert_eq!(accents(6, 0, 0), undivided);
    assert_eq!((undivided.parts(), undivided.beats()), ([6, 0, 0], 6));
    // 5/4 as 3 + 2; 9/4 as 3 + 3 + 3 and as 4 + 4 + 1.
    for (parts, accent, beats) in [
        ([3, 2, 0], BarAccents::Two(part(3), part(2)), 5),
        ([3, 3, 3], BarAccents::Three(part(3), part(3), part(3)), 9),
        ([4, 4, 1], BarAccents::Three(part(4), part(4), part(1)), 9),
    ] {
        let [a, b, c] = parts;
        assert_eq!(accents(a, b, c), accent);
        assert_eq!((accent.parts(), accent.beats()), (parts, beats));
        let metronome = Metronome {
            clocks_per_click: 24,
            accents: accent,
            subdivision_1: 0,
            subdivision_2: 0,
        };
        assert_eq!(
            setup(Flex::SetMetronome(metronome)).0[1],
            u32::from_be_bytes([24, a, b, c])
        );
    }
    // No Part 1, or a part after a zero part.
    for (a, b, c) in [(0, 0, 0), (0, 2, 0), (0, 0, 2), (0, 2, 3), (3, 0, 2)] {
        let read = accents(a, b, c);
        assert!(matches!(read, BarAccents::Reserved(r) if r.parts() == [a, b, c]));
        assert_eq!(BarAccents::from_parts([a, b, c]), None);
        // The clocks and the subdivisions are still there, and the packet
        // re-encodes to the words it came from.
        let packet = Ump([word(a, b, c)[0], word(a, b, c)[1], 0x0200_0000, 0]);
        let Ok(message @ Message::Flex(_, Flex::SetMetronome(m))) = packet.decode() else {
            panic!("{a} {b} {c}")
        };
        assert_eq!((m.clocks_per_click, m.subdivision_1), (24, 2));
        assert_eq!(Ump::from(message), packet);
    }
}
