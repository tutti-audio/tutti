//! `rem_euclid`, the one `std` float method that `libm` has no function for:
//! `%` is `fmod`, and the remainder is shifted into `[0, |rhs|)`.

/// `a` modulo `b`, in `[0, |b|)` (`f32::rem_euclid`).
pub(crate) fn rem_euclid_f32(a: f32, b: f32) -> f32 {
    let r = a % b;
    if r < 0.0 { r + libm::fabsf(b) } else { r }
}

/// `a` modulo `b`, in `[0, |b|)` (`f64::rem_euclid`).
pub(crate) fn rem_euclid_f64(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r < 0.0 { r + libm::fabs(b) } else { r }
}
