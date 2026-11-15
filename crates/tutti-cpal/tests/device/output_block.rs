//! The sample-format conversion the device driver hands the output block, tested
//! without a sound card.
//!
//! The block renders `f32`; a driver converts each sample to its device's type
//! with `T::from_sample` (what `CpalDriver`'s callback passes to
//! `OutputBlock::render_delayed_with`). The clamp, the zero-fill and the width
//! are the block's, tested in `tutti-engine`.
//!
//! Expected values are stated from **each format's own arithmetic**, never by
//! calling `T::from_sample`: asking the function under test what it should
//! have produced is not a second opinion.

use crate::support::dc_state;
use tutti_engine::OutputBlock;
use tutti_types::ChannelLayout;

/// Full scale for an integer format, from the format's definition rather than
/// from the conversion under test.
fn dc_and_check<T: cpal::SizedSample + cpal::FromSample<f32> + PartialEq + std::fmt::Debug>(
    level: f32,
    expected: T,
    label: &str,
) {
    let mut block = OutputBlock::new(dc_state(level, 2), ChannelLayout::STEREO);
    let mut out = [T::from_sample(0.0f32); 64];
    block.render_with(&mut out, T::from_sample);
    for (i, s) in out.iter().enumerate() {
        assert_eq!(
            *s, expected,
            "{label}: sample {i} of a constant {level} block should be \
             {expected:?}, got {s:?}"
        );
    }
}

/// **The eight-way sample-format matrix, one constant level through each.**
///
/// A constant-DC graph makes every sample in the block the same known value,
/// so the expected result is a single number per format, derived from that
/// format's range. `dasp`'s conversions are the thing being checked, so none
/// of these numbers comes from asking it.
#[test]
fn every_sample_format_carries_the_dc_level_it_was_given() {
    // Half of full scale. The integer expectations are `0.5 * 2^(bits-1)`,
    // derived from each format's width rather than from the conversion: the
    // scale factor is the *magnitude* 2^(bits-1), with the positive extreme
    // clamped, which is why i16 gives 16384 and not 16383. Stating it this way
    // is what makes a wrong scale factor visible.
    dc_and_check::<f32>(0.5, 0.5, "f32");
    dc_and_check::<f64>(0.5, 0.5, "f64");
    dc_and_check::<i16>(0.5, 1 << 14, "i16"); // 0.5 * 2^15
    dc_and_check::<i8>(0.5, 1 << 6, "i8"); //    0.5 * 2^7
    dc_and_check::<i32>(0.5, 1 << 30, "i32"); // 0.5 * 2^31
    // Unsigned formats are offset-binary: silence is mid-scale.
    dc_and_check::<u8>(0.0, 128, "u8 silence");
    dc_and_check::<u16>(0.0, 32768, "u16 silence");
    dc_and_check::<u32>(0.0, 2_147_483_648, "u32 silence");
}

/// **Silence is silence in every format**, including the offset-binary ones
/// where "zero" is not zero. A conversion that emitted a literal 0 for `u16`
/// would produce full-negative DC — a loud thump, not silence.
#[test]
fn silence_is_the_formats_own_zero_not_a_literal_zero() {
    dc_and_check::<i16>(0.0, 0, "i16");
    dc_and_check::<u8>(0.0, 128, "u8");
    dc_and_check::<u16>(0.0, 32768, "u16");
}
