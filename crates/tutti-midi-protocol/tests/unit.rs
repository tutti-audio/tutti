//! The pitch bend's float scaling.

use tutti_midi_protocol::signed_f32_to_bend;
use tutti_ump::BendValue;

#[test]
fn signed_bend_inverse_centers() {
    assert_eq!(signed_f32_to_bend(0.0), BendValue::new(0x8000_0000));
    assert_eq!(signed_f32_to_bend(-1.0), BendValue::new(0));
    assert_eq!(signed_f32_to_bend(1.0), BendValue::new(u32::MAX));
    // Clamps out-of-range input.
    assert_eq!(signed_f32_to_bend(-2.0), BendValue::new(0));
    assert_eq!(signed_f32_to_bend(2.0), BendValue::new(u32::MAX));
}

/// Half way up the wheel is three quarters of the 32-bit range.
#[test]
fn signed_bend_span_is_half_the_range() {
    assert_eq!(signed_f32_to_bend(0.5), BendValue::new(0xC000_0000));
    assert_eq!(signed_f32_to_bend(-0.5), BendValue::new(0x4000_0000));
}
