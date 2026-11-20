//! Test-only support for this crate's unit tests: the signal helpers.

mod signals;

pub(crate) use signals::{
    amplitude_at, butterworth_all_pass, direct_form, lock_in, noise, sine_fit, steady_deviation,
    uniform,
};

/// The crossover model with a split at each of `frequencies` at `rate`, so
/// `frequencies.len() + 1` bands. The frequencies are the ones a crossover accepts: increasing
/// and below Nyquist.
pub(crate) fn crossover_model(
    frequencies: &[tutti_units::Hz],
    rate: tutti_units::SampleRate,
) -> crate::CrossoverModel {
    let controls = crate::CrossoverControls::try_from((frequencies, rate)).unwrap();
    let bands = crate::CrossoverBands::new(frequencies.len() + 1).unwrap();
    crate::CrossoverModel::designed(bands, &controls, rate)
}
