//! `to_u7`, `to_u14` and `to_note_on_u7` against figures that do not come from their own code: Table 8 of
//! M2-115 section 3.4, the bit-level statement of the rule at the other widths,
//! and the round trip through `Voice2::from` for every 7-bit and 14-bit value.

use tutti_ump::{BendValue, ControlValue, Voice2};
use tutti_ump::{CCNumber, NoteNumber, NoteVelocity, U7, U14, Voice1};

/// Table 8, downscale 16 to 7 bits. `0x15FF` is not in the table: it is the
/// value Table 10's rounding method sends to 11, which the plain shift of
/// section 3.4 sends to 10.
/// Spec: m2-115/2.2#2, m2-115/2.3#1
#[test]
fn m2_115_table_8_velocity() {
    for (v, want) in [
        (5120, 10),
        (32768, 64),
        (44730, 87),
        (65535, 127),
        (0x15FF, 10),
    ] {
        assert_eq!(NoteVelocity::new(v).to_u7().get(), want, "{v:#x}");
    }
}

/// The centre and the extremes land on the centre and the extremes (section
/// 3.2), at every width.
#[test]
fn the_extremes_and_the_centre_keep_their_places() {
    assert_eq!(
        [0, 0x8000, 0xFFFF].map(|v| NoteVelocity::new(v).to_u7().get()),
        [0, 64, 127]
    );
    assert_eq!(
        [0, 0x8000_0000, u32::MAX].map(|v| ControlValue::new(v).to_u7().get()),
        [0, 64, 127]
    );
    assert_eq!(
        [0, 0x8000_0000, u32::MAX].map(|v| BendValue::new(v).to_u14().get()),
        [0, 8192, 16383]
    );
}

/// Section 3.4 stated bit by bit: the result is the source's top bits, so the
/// value is exact on the multiple of the step and constant over the step.
/// Spec: m2-115/2.2#2
#[test]
fn the_top_bits_are_kept() {
    for v in 0..=127u32 {
        let step = 1u32 << 9;
        assert_eq!(NoteVelocity::new((v * step) as u16).to_u7().get(), v as u8);
        assert_eq!(
            NoteVelocity::new((v * step + step - 1) as u16)
                .to_u7()
                .get(),
            v as u8
        );
        let step = 1u32 << 25;
        assert_eq!(ControlValue::new(v * step).to_u7().get(), v as u8);
        assert_eq!(
            ControlValue::new(v * step + (step - 1)).to_u7().get(),
            v as u8
        );
    }
    for v in 0..=16383u32 {
        let step = 1u32 << 18;
        assert_eq!(BendValue::new(v * step).to_u14().get(), v as u16);
        assert_eq!(
            BendValue::new(v * step + (step - 1)).to_u14().get(),
            v as u16
        );
    }
}

#[test]
fn to_midi2_then_to_midi1_returns_every_seven_bit_value() {
    for v in 0..=127u8 {
        let off = Voice2::from(Voice1::NoteOff {
            note: NoteNumber::of(60),
            velocity: U7::of(v),
        });
        let Voice2::NoteOff { velocity, .. } = off else {
            panic!("{off:?}")
        };
        assert_eq!(velocity.to_u7(), U7::of(v), "velocity {v}");

        let cc = Voice2::from(Voice1::ControlChange {
            controller: CCNumber::of(7),
            value: U7::of(v),
        });
        let Voice2::ControlChange { value, .. } = cc else {
            panic!("{cc:?}")
        };
        assert_eq!(value.to_u7(), U7::of(v), "value {v}");
    }
}

#[test]
fn to_midi2_then_to_midi1_returns_every_fourteen_bit_bend() {
    for v in 0..=16383u16 {
        let bend = U14::of(v);
        let Voice2::PitchBend(wide) = Voice2::from(Voice1::PitchBend(bend)) else {
            panic!("{v}")
        };
        assert_eq!(wide.to_u14(), bend, "bend {v}");
    }
}

/// M2-104 section 7.4.2: a MIDI 1.0 Note On at velocity 0 is a Note Off, so a
/// nonzero 16-bit Note On velocity never downscales to 0. Velocity 0 itself and the
/// range at and above `0x0200` are the plain shift's.
#[test]
fn a_note_on_velocity_never_translates_to_a_note_off() {
    for v in 1..0x0200 {
        assert_eq!(NoteVelocity::new(v).to_u7().get(), 0, "{v:#x}");
        assert_eq!(NoteVelocity::new(v).to_note_on_u7().get(), 1, "{v:#x}");
    }
    for v in [0x0200, 0x8000, 0xFFFF] {
        assert_eq!(
            NoteVelocity::new(v).to_note_on_u7(),
            NoteVelocity::new(v).to_u7(),
            "{v:#x}"
        );
    }
}

/// **`Voice1::from_voice2` applies D.1.5 to every value it translates**: with the extremes
/// avoided a value above 0 never downscales to 0, and one below its maximum never to
/// the maximum, at each width; the minimum and maximum downscale to themselves.
/// Scaled is `Voice1::try_from`'s plain shift. A message with no MIDI 1.0 form is refused
/// as `Voice1::try_from` refuses it.
/// Spec: m2-104/d.1.5#1
#[test]
fn from_voice2_keeps_values_off_the_extremes_only_when_avoided() {
    use tutti_ump::Voice2;
    use tutti_ump::{
        BendValue, CCNumber, ControlValue, ExtremeValues, NoMidi1Form, NoteNumber, NoteVelocity,
        U7, U14, Voice1,
    };

    let cc = |value: u32| Voice2::ControlChange {
        controller: CCNumber::MOD_WHEEL,
        value: ControlValue::new(value),
    };
    let cc_of = |voice| match voice {
        Ok(Voice1::ControlChange { value, .. }) => value.get(),
        other => panic!("{other:?}"),
    };
    // Just above the minimum, just below the maximum, and the extremes.
    for (value, scaled, avoided) in [
        (0x0000_0001, 0, 1),
        (0xFFFF_FFFE, 127, 126),
        (0, 0, 0),
        (u32::MAX, 127, 127),
    ] {
        assert_eq!(
            cc_of(Voice1::from_voice2(cc(value), ExtremeValues::Scaled)),
            scaled
        );
        assert_eq!(
            cc_of(Voice1::from_voice2(cc(value), ExtremeValues::Avoided)),
            avoided
        );
        assert_eq!(cc_of(Voice1::try_from(cc(value))), scaled);
    }

    let note = NoteNumber::of(60);
    let on = |velocity: u16| Voice2::NoteOn {
        note,
        velocity: NoteVelocity::new(velocity),
        attribute: tutti_ump::Attribute::None,
    };
    let velocity_of = |voice| match voice {
        Ok(Voice1::NoteOn { velocity, .. }) => velocity,
        other => panic!("{other:?}"),
    };
    // A Note On velocity never downscales to 0 (D.2.1), and one below the maximum
    // is off the maximum when avoided.
    assert_eq!(
        velocity_of(Voice1::from_voice2(on(1), ExtremeValues::Avoided)),
        U7::of(1)
    );
    assert_eq!(
        velocity_of(Voice1::from_voice2(on(0xFFFE), ExtremeValues::Avoided)),
        U7::of(126)
    );
    assert_eq!(
        velocity_of(Voice1::from_voice2(on(0xFFFE), ExtremeValues::Scaled)),
        U7::of(127)
    );

    let bend = |value: u32| Voice2::PitchBend(BendValue::new(value));
    let bend_of = |voice| match voice {
        Ok(Voice1::PitchBend(value)) => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        bend_of(Voice1::from_voice2(bend(1), ExtremeValues::Avoided)),
        U14::of(1)
    );
    assert_eq!(
        bend_of(Voice1::from_voice2(
            bend(u32::MAX - 1),
            ExtremeValues::Avoided
        )),
        U14::of(0x3FFE)
    );
    assert_eq!(
        bend_of(Voice1::from_voice2(bend(1), ExtremeValues::Scaled)),
        U14::of(0)
    );

    let parameter = Voice2::Registered {
        bank: U7::of(0),
        index: U7::of(0),
        value: ControlValue::new(0),
    };
    assert_eq!(
        Voice1::from_voice2(parameter, ExtremeValues::Avoided),
        Err(NoMidi1Form::Parameter)
    );
}
