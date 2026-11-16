//! Decibel maths in `f64`, for design and analysis that runs in double precision.
//!
//! [`Db`](tutti_types::Db) is `f32`: a parameter is widened once at entry
//! (`f64::from(db.get())`), and a level leaves narrowed once at the return.
//! These two are the one `f64` form of the amplitude-decibel pair, through
//! `libm` so a design does not depend on the platform's C math library.

/// The amplitude ratio of `db` decibels, `10^(db / 20)`.
#[inline]
pub(crate) fn db_to_ratio_f64(db: f64) -> f64 {
    libm::pow(10.0, db / 20.0)
}

/// Decibels of an amplitude ratio, `20 log10 ratio`: `-inf` for 0, NaN for a
/// negative ratio.
#[inline]
pub(crate) fn ratio_to_db_f64(ratio: f64) -> f64 {
    20.0 * libm::log10(ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 20 dB is a ratio of 10, -6.020599913279624 dB (`20 log10 0.5`) is a half,
    /// and the pair round-trips to a few ulps in `f64`.
    #[test]
    fn the_pair_is_the_amplitude_decibel_definition() {
        assert_eq!(db_to_ratio_f64(20.0), 10.0);
        assert_eq!(db_to_ratio_f64(0.0), 1.0);
        assert_eq!(ratio_to_db_f64(100.0), 40.0);
        assert!((ratio_to_db_f64(0.5) + 6.020_599_913_279_624).abs() < 1e-12);
        assert_eq!(ratio_to_db_f64(0.0), f64::NEG_INFINITY);
        assert!(ratio_to_db_f64(-1.0).is_nan());
        for db in [-120.0, -60.0, -3.0, 0.0, 12.0] {
            assert!((ratio_to_db_f64(db_to_ratio_f64(db)) - db).abs() < 1e-12);
        }
    }
}
