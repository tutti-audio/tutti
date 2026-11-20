//! [`CrossoverControls`]: the split frequencies of a Linkwitz-Riley crossover, and the checks
//! that make a set one a crossover can run.

use tutti_units::{Hz, SampleRate};

use super::bands::{CrossoverError, MAX_SPLITS, validate, validate_order};

/// The frequencies of a crossover's splits, lowest first; the entries past the
/// design's last split are 0 and ignored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrossoverControls {
    /// Split `i` divides band `i` from band `i + 1`.
    pub frequencies: [Hz; MAX_SPLITS],
}

/// The set `frequencies` for a rate not yet known: one split each, so `frequencies.len() + 1`
/// bands. The design holds each split under the rate it runs at
/// ([`CrossoverModel::design_into`](crate::CrossoverModel::design_into)), so a split above a
/// rate's Nyquist is clamped there, not refused.
///
/// The refusal depends on the frequencies together (their count, their order), so it is a
/// conversion and not a type.
///
/// # Errors
///
/// [`CrossoverError`]: a band count outside 2 to 6, or frequencies that are not finite,
/// above 0 and strictly increasing.
impl TryFrom<&[Hz]> for CrossoverControls {
    type Error = CrossoverError;

    fn try_from(frequencies: &[Hz]) -> Result<Self, CrossoverError> {
        validate_order(frequencies)?;
        Ok(Self::of(frequencies))
    }
}

/// The set `frequencies` at `rate`: the conversion from a slice, and every split below
/// Nyquist.
///
/// # Errors
///
/// [`CrossoverError`]: a band count outside 2 to 6, or frequencies that are not finite,
/// above 0, strictly increasing and below Nyquist.
impl TryFrom<(&[Hz], SampleRate)> for CrossoverControls {
    type Error = CrossoverError;

    fn try_from((frequencies, rate): (&[Hz], SampleRate)) -> Result<Self, CrossoverError> {
        validate(frequencies, rate)?;
        Ok(Self::of(frequencies))
    }
}

impl CrossoverControls {
    /// How many splits are set: the entries before the first 0, the value the unset ones
    /// hold. A set [`TryFrom`] makes has `frequencies.len()`; a literal with no split has 0.
    #[must_use]
    pub fn splits(&self) -> usize {
        self.frequencies
            .iter()
            .take_while(|f| **f > Hz::ZERO)
            .count()
    }

    fn of(frequencies: &[Hz]) -> Self {
        let mut set = [Hz::ZERO; MAX_SPLITS];
        set[..frequencies.len()].copy_from_slice(frequencies);
        Self { frequencies: set }
    }

    /// Splits an octave apart from 0.2% of the rate up: increasing and under Nyquist at
    /// any rate.
    pub(crate) fn octaves(rate: SampleRate) -> Self {
        Self {
            frequencies: core::array::from_fn(|i| Hz::new(rate.get() * 0.002 * (1 << i) as f64)),
        }
    }
}

#[cfg(test)]
mod tests;
