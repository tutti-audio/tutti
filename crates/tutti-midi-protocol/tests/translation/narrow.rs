//! `narrow_*` against figures that do not come from their own code: Table 8 of
//! M2-115 section 3.4, the bit-level statement of the rule at the other widths,
//! and the round trip through `widen` for every 7-bit and 14-bit value.

use tutti_midi_protocol::{
    narrow_bend, narrow_note_on_velocity, narrow_value, narrow_velocity, widen,
};
use tutti_ump::{BendValue, ControlValue};
use tutti_ump::{CCNumber, NoteNumber, NoteVelocity, U7, U14, Voice1, Voice2};

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
        assert_eq!(narrow_velocity(NoteVelocity::new(v)).get(), want, "{v:#x}");
    }
}

/// The centre and the extremes land on the centre and the extremes (section
/// 3.2), at every width.
#[test]
fn the_extremes_and_the_centre_keep_their_places() {
    assert_eq!(
        [0, 0x8000, 0xFFFF].map(|v| narrow_velocity(NoteVelocity::new(v)).get()),
        [0, 64, 127]
    );
    assert_eq!(
        [0, 0x8000_0000, u32::MAX].map(|v| narrow_value(ControlValue::new(v)).get()),
        [0, 64, 127]
    );
    assert_eq!(
        [0, 0x8000_0000, u32::MAX].map(|v| narrow_bend(BendValue::new(v)).get()),
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
        assert_eq!(
            narrow_velocity(NoteVelocity::new((v * step) as u16)).get(),
            v as u8
        );
        assert_eq!(
            narrow_velocity(NoteVelocity::new((v * step + step - 1) as u16)).get(),
            v as u8
        );
        let step = 1u32 << 25;
        assert_eq!(narrow_value(ControlValue::new(v * step)).get(), v as u8);
        assert_eq!(
            narrow_value(ControlValue::new(v * step + (step - 1))).get(),
            v as u8
        );
    }
    for v in 0..=16383u32 {
        let step = 1u32 << 18;
        assert_eq!(narrow_bend(BendValue::new(v * step)).get(), v as u16);
        assert_eq!(
            narrow_bend(BendValue::new(v * step + (step - 1))).get(),
            v as u16
        );
    }
}

#[test]
fn widen_then_narrow_returns_every_seven_bit_value() {
    for v in 0..=127u8 {
        let off = widen(Voice1::NoteOff {
            note: NoteNumber::of(60),
            velocity: U7::of(v),
        });
        let Voice2::NoteOff { velocity, .. } = off else {
            panic!("{off:?}")
        };
        assert_eq!(narrow_velocity(velocity), U7::of(v), "velocity {v}");

        let cc = widen(Voice1::ControlChange {
            controller: CCNumber::of(7),
            value: U7::of(v),
        });
        let Voice2::ControlChange { value, .. } = cc else {
            panic!("{cc:?}")
        };
        assert_eq!(narrow_value(value), U7::of(v), "value {v}");
    }
}

#[test]
fn widen_then_narrow_returns_every_fourteen_bit_bend() {
    for v in 0..=16383u16 {
        let bend = U14::of(v);
        let Voice2::PitchBend(wide) = widen(Voice1::PitchBend(bend)) else {
            panic!("{v}")
        };
        assert_eq!(narrow_bend(wide), bend, "bend {v}");
    }
}

/// M2-104 section 7.4.2: a MIDI 1.0 Note On at velocity 0 is a Note Off, so a
/// nonzero 16-bit Note On velocity never narrows to 0. Velocity 0 itself and the
/// range at and above `0x0200` are the plain shift's.
#[test]
fn a_note_on_velocity_never_narrows_to_a_note_off() {
    for v in 1..0x0200 {
        assert_eq!(narrow_velocity(NoteVelocity::new(v)).get(), 0, "{v:#x}");
        assert_eq!(
            narrow_note_on_velocity(NoteVelocity::new(v)).get(),
            1,
            "{v:#x}"
        );
    }
    for v in [0x0200, 0x8000, 0xFFFF] {
        assert_eq!(
            narrow_note_on_velocity(NoteVelocity::new(v)),
            narrow_velocity(NoteVelocity::new(v)),
            "{v:#x}"
        );
    }
}

/// **`narrow_with` applies D.1.5 to every value it narrows**: with the extremes
/// avoided a value above 0 never narrows to 0, and one below its maximum never to
/// the maximum, at each width; the minimum and maximum narrow to themselves.
/// Scaled is [`narrow`]'s plain shift. A message with no MIDI 1.0 form is refused
/// as `narrow` refuses it.
/// Spec: m2-104/d.1.5#1
#[test]
fn narrow_with_keeps_values_off_the_extremes_only_when_avoided() {
    use tutti_midi_protocol::{ExtremeValues, NoMidi1Form, narrow, narrow_with};
    use tutti_ump::{BendValue, CCNumber, ControlValue, NoteNumber, NoteVelocity, U7, U14};
    use tutti_ump::{Voice1, Voice2};

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
        assert_eq!(cc_of(narrow_with(ExtremeValues::Scaled, cc(value))), scaled);
        assert_eq!(
            cc_of(narrow_with(ExtremeValues::Avoided, cc(value))),
            avoided
        );
        assert_eq!(cc_of(narrow(cc(value))), scaled);
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
    // A Note On velocity never narrows to 0 (D.2.1), and one below the maximum
    // is off the maximum when avoided.
    assert_eq!(
        velocity_of(narrow_with(ExtremeValues::Avoided, on(1))),
        U7::of(1)
    );
    assert_eq!(
        velocity_of(narrow_with(ExtremeValues::Avoided, on(0xFFFE))),
        U7::of(126)
    );
    assert_eq!(
        velocity_of(narrow_with(ExtremeValues::Scaled, on(0xFFFE))),
        U7::of(127)
    );

    let bend = |value: u32| Voice2::PitchBend(BendValue::new(value));
    let bend_of = |voice| match voice {
        Ok(Voice1::PitchBend(value)) => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        bend_of(narrow_with(ExtremeValues::Avoided, bend(1))),
        U14::of(1)
    );
    assert_eq!(
        bend_of(narrow_with(ExtremeValues::Avoided, bend(u32::MAX - 1))),
        U14::of(0x3FFE)
    );
    assert_eq!(
        bend_of(narrow_with(ExtremeValues::Scaled, bend(1))),
        U14::of(0)
    );

    let parameter = Voice2::Registered {
        bank: U7::of(0),
        index: U7::of(0),
        value: ControlValue::new(0),
    };
    assert_eq!(
        narrow_with(ExtremeValues::Avoided, parameter),
        Err(NoMidi1Form::Parameter)
    );
}
