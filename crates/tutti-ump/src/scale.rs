//! Min-Center-Max upscaling (M2-115 section 3).

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
