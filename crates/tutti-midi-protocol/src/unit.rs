//! A native UMP pitch bend as the signed unit float a DSP node wants, and back.
//!
//! A bend in `[-1.0, 1.0]` is a position on a range the receiver owns
//! (`PitchBendSensitivity` is that range), so these return a bare `f32` and the
//! caller wraps it in the unit it means at its own seam. The inverse takes the
//! same bare `f32`, as the plugin formats carry it, and clamps. They are a
//! convenience for the audio edge, not part of the MIDI 1.0 to 2.0 resolution
//! path (`widen` and the `narrow_*` family). The velocity and controller
//! scalings are `NoteVelocity::normalized` and `ControlValue::normalized`.

use tutti_ump::BendValue;

/// A MIDI 2.0 pitch bend (centre [`BendValue::CENTER`]) → `[-1.0, 1.0]`.
#[inline]
pub fn bend_to_signed_f32(v: BendValue) -> f32 {
    ((v.get() as f64 - 0x8000_0000_u32 as f64) / 0x8000_0000_u32 as f64) as f32
}

/// `[-1.0, 1.0]` → a MIDI 2.0 pitch bend. Inverse of [`bend_to_signed_f32`];
/// clamps and saturates at the 32-bit max.
#[inline]
pub fn signed_f32_to_bend(v: f32) -> BendValue {
    let centered = 0x8000_0000_u32 as f64 + v.clamp(-1.0, 1.0) as f64 * 0x8000_0000_u32 as f64;
    BendValue::new(libm::round(centered).clamp(0.0, u32::MAX as f64) as u32)
}
