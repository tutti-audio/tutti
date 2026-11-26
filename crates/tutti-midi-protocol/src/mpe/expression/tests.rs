use super::*;

fn rc(expression: MpeExpression, data: u32) -> Voice2 {
    Voice2::registered(expression.bipolar_rpn(), ControlValue::new(data))
}

/// M2-120 Table 9: the bipolar controllers are the Registered Controllers
/// 0x20/0x20 and 0x20/0x21, and nothing else reads as one.
/// Spec: m2-120/4.9#4
#[test]
fn the_bipolar_controllers_are_registered_controllers_0x20_0x20_and_0x20_0x21() {
    let address = |e: MpeExpression| (e.bipolar_rpn().bank().get(), e.bipolar_rpn().index().get());
    assert_eq!(address(MpeExpression::Pressure), (0x20, 0x20));
    assert_eq!(address(MpeExpression::ThirdDimension), (0x20, 0x21));
    assert_eq!(
        MpeExpression::of_rpn(Rpn::new(U7::of(0x20), U7::of(0x20))),
        Some(MpeExpression::Pressure)
    );
    assert_eq!(
        MpeExpression::of_rpn(Rpn::new(U7::of(0x20), U7::of(0x21))),
        Some(MpeExpression::ThirdDimension)
    );
    assert_eq!(MpeExpression::of_rpn(Rpn::ORCHESTRAL_MUTE_TYPE), None);
    assert_eq!(MpeExpression::of_rpn(Rpn::PITCH_BEND_SENSITIVITY), None);
}

/// Section 4.9: a property that is not bipolar is sent as the upper half of
/// the range, centred at 0x8000_0000: Channel Pressure at rest is the centre
/// and at full is the top; Control Change 74 is centred already.
/// Spec: m2-120/4.9#4
#[test]
fn pressure_is_the_upper_half_and_the_third_dimension_is_as_it_is() {
    let pressure = MpeExpression::Pressure;
    assert_eq!(
        pressure.bipolar(ControlValue::MIN),
        rc(pressure, 0x8000_0000)
    );
    assert_eq!(
        pressure.bipolar(ControlValue::MAX),
        rc(pressure, 0xFFFF_FFFF)
    );
    assert_eq!(
        pressure.bipolar(ControlValue::CENTER),
        rc(pressure, 0xC000_0000)
    );
    let third = MpeExpression::ThirdDimension;
    for data in [0, 0x4000_0000, 0x8000_0000, 0xFFFF_FFFF] {
        assert_eq!(third.bipolar(ControlValue::new(data)), rc(third, data));
        assert_eq!(
            MpeExpression::read(rc(third, data)),
            Some((third, ControlValue::new(data)))
        );
    }
}

/// Section 4.9: a bipolar Pressure below its centre has no unipolar meaning and
/// is 0; above it, the upper half is the whole range.
#[test]
fn a_bipolar_pressure_below_the_centre_is_zero() {
    let read = |data| MpeExpression::read(rc(MpeExpression::Pressure, data));
    let at = |value| Some((MpeExpression::Pressure, ControlValue::new(value)));
    assert_eq!(read(0), at(0));
    assert_eq!(read(0x7FFF_FFFF), at(0));
    assert_eq!(read(0x8000_0000), at(0));
    assert_eq!(read(0xC000_0000), at(0x8000_0000));
    assert_eq!(read(0xFFFF_FFFF), at(0xFFFF_FFFE));
}

/// A bipolar controller is read as the control it replaces, and either form is
/// read as its control.
/// Spec: m2-120/4.4#5, m2-120/4.5#6
#[test]
fn both_forms_read_as_their_control_and_convert_to_the_control_change_form() {
    let value = ControlValue::new(0x1234_5678);
    for control in [MpeExpression::Pressure, MpeExpression::ThirdDimension] {
        let form = control.control_change(value);
        assert_eq!(MpeExpression::read(form), Some((control, value)));
        assert_eq!(MpeExpression::to_control_change(form), form);
        let bipolar = control.bipolar(value);
        let (_, read) = MpeExpression::read(bipolar).unwrap();
        assert_eq!(
            MpeExpression::to_control_change(bipolar),
            control.control_change(read)
        );
    }
    let other = Voice2::registered(Rpn::PITCH_BEND_SENSITIVITY, value);
    assert_eq!(MpeExpression::to_control_change(other), other);
    assert_eq!(MpeExpression::read(other), None);
}

/// Section 4.9: a receiver of the bipolar form is sent that form and not the
/// Control Change one; one of the Control Change form only is sent that; one
/// that does not use the control is sent none.
/// Spec: m2-120/4.9#2, m2-120/4.9#3
#[test]
fn a_form_is_the_one_the_receiver_supports_and_never_both() {
    let value = ControlValue::new(0x4000_0000);
    for control in [MpeExpression::Pressure, MpeExpression::ThirdDimension] {
        assert_eq!(control.form(ControllerSupport::Unsupported, value), None);
        assert_eq!(
            control.form(ControllerSupport::ControlChange, value),
            Some(control.control_change(value))
        );
        assert_eq!(
            control.form(ControllerSupport::Bipolar, value),
            Some(control.bipolar(value))
        );
        assert_ne!(control.control_change(value), control.bipolar(value));
    }
}
