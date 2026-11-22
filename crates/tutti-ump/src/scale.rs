//! The scalings of M2-115: Min-Center-Max (section 3), the zero-extension and rounding
//! that section 4 uses for fixed-point Registered Controller data.

/// Upscales `value` of `src` bits to `dst` bits. `1 < src < dst <= 32`.
///
/// Values up to the centre shift left, which keeps the steps even towards the
/// centre; values above it also repeat their low `src - 1` bits into the new
/// low bits, which lands the maximum on the maximum.
#[inline]
pub(crate) const fn upscale(value: u32, src: u32, dst: u32) -> u32 {
    let scale_bits = dst - src;
    let shifted = value << scale_bits;
    if value <= 1 << (src - 1) {
        return shifted;
    }
    let repeat_bits = src - 1;
    let mut repeat = value & ((1 << repeat_bits) - 1);
    repeat = if scale_bits > repeat_bits {
        repeat << (scale_bits - repeat_bits)
    } else {
        repeat >> (repeat_bits - scale_bits)
    };
    let mut out = shifted;
    while repeat != 0 {
        out |= repeat;
        repeat >>= repeat_bits;
    }
    out
}

/// Whether the Registered Controller `index` is zero-extended (M2-115 section
/// 4.1: "Translators shall use Zero-Extension Scaling on all Registered
/// Controller (RPN) Messages where the index (RPN LSB) is 0-31") rather than
/// scaled Min-Center-Max (section 3.1, index 32 to 127), whatever the bank.
#[inline]
pub(crate) const fn zero_extended(index: crate::U7) -> bool {
    index.get() < 32
}

/// A 32-bit value as 14 bits, rounded (M2-115 section 4.4.1,
/// `scaleDownRounding(value, 32, 14)`): fixed-point values are rounded when
/// downscaled, by adding half the scaled range, shifting, and clamping to the
/// 14-bit maximum.
#[inline]
pub(crate) const fn rounded_u14(value: u32) -> u16 {
    const SCALE_BITS: u32 = 32 - 14;
    let shifted = (value as u64 + (1 << (SCALE_BITS - 1))) >> SCALE_BITS;
    // The clamp keeps 14 bits.
    if shifted > 0x3FFF {
        0x3FFF
    } else {
        shifted as u16
    }
}
