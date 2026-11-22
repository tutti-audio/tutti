//! The graphic equalizer's design: a bank of [`Cookbook`] peaking bells at the ISO 266
//! preferred centers, one gain per band.

use core::f64::consts::LN_2;

use tutti_units::{Db, Hz, Q, SampleRate};

use super::{Cookbook, Sos, SosError};
use crate::filter::decay_time;
use crate::{Biquad, DecayTime, Lti};

/// The ISO 266 preferred third-octave centers, 20 Hz to 20 kHz; the octave bands are every
/// third from 31.5 Hz.
const THIRD_OCTAVE: [Hz; 31] = [
    Hz::new(20.0),
    Hz::new(25.0),
    Hz::new(31.5),
    Hz::new(40.0),
    Hz::new(50.0),
    Hz::new(63.0),
    Hz::new(80.0),
    Hz::new(100.0),
    Hz::new(125.0),
    Hz::new(160.0),
    Hz::new(200.0),
    Hz::new(250.0),
    Hz::new(315.0),
    Hz::new(400.0),
    Hz::new(500.0),
    Hz::new(630.0),
    Hz::new(800.0),
    Hz::new(1_000.0),
    Hz::new(1_250.0),
    Hz::new(1_600.0),
    Hz::new(2_000.0),
    Hz::new(2_500.0),
    Hz::new(3_150.0),
    Hz::new(4_000.0),
    Hz::new(5_000.0),
    Hz::new(6_300.0),
    Hz::new(8_000.0),
    Hz::new(10_000.0),
    Hz::new(12_500.0),
    Hz::new(16_000.0),
    Hz::new(20_000.0),
];

/// The ISO 266 preferred octave centers, 31.5 Hz to 16 kHz.
const OCTAVE: [Hz; 10] = [
    Hz::new(31.5),
    Hz::new(63.0),
    Hz::new(125.0),
    Hz::new(250.0),
    Hz::new(500.0),
    Hz::new(1_000.0),
    Hz::new(2_000.0),
    Hz::new(4_000.0),
    Hz::new(8_000.0),
    Hz::new(16_000.0),
];

/// The section of a band above Nyquist: the input as it is.
const WIRE: Biquad = Biquad {
    b0: 1.0,
    b1: 0.0,
    b2: 0.0,
    a1: 0.0,
    a2: 0.0,
};

/// The band spacing of a graphic equalizer: the ISO 266 octave or third-octave centers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GraphicBands {
    /// Ten bands, 31.5 Hz to 16 kHz, an octave apart.
    Octave,
    /// Thirty-one bands, 20 Hz to 20 kHz, a third of an octave apart.
    ThirdOctave,
}

impl GraphicBands {
    /// The most bands of any spacing: the third-octave ones.
    pub const MAX_COUNT: usize = THIRD_OCTAVE.len();

    /// The nominal center of each band, lowest first.
    #[must_use]
    pub fn centers(self) -> &'static [Hz] {
        match self {
            Self::Octave => &OCTAVE,
            Self::ThirdOctave => &THIRD_OCTAVE,
        }
    }

    /// The number of bands.
    #[must_use]
    pub fn count(self) -> usize {
        self.centers().len()
    }

    /// The quality factor of every band: the analog `Q = 1 / (2 sinh(ln(2) / 2 * BW))` of a
    /// bandwidth `BW` of one band's spacing in octaves (1 or 1/3), the cookbook's
    /// bandwidth-to-`Q` relation.
    #[must_use]
    pub fn q(self) -> Q {
        let octaves = match self {
            Self::Octave => 1.0,
            Self::ThirdOctave => 1.0 / 3.0,
        };
        Q::new(1.0 / (2.0 * libm::sinh(LN_2 / 2.0 * octaves)))
    }

    /// Designs the bank at `rate`: one [`Cookbook::Peaking`] section per band, band `i` with
    /// `gains[i]` (the gains past [`count`](Self::count) are not read). A band whose center is
    /// not below Nyquist is a wire, so the section count is the band count at every rate.
    ///
    /// # Errors
    ///
    /// [`SosError::GainOutOfRange`] for a read gain beyond
    /// [`MAX_GAIN_MAGNITUDE`](super::MAX_GAIN_MAGNITUDE).
    pub fn design(self, gains: &[Db; Self::MAX_COUNT], rate: SampleRate) -> Result<Sos, SosError> {
        let mut sos = Sos::new(Vec::with_capacity(self.count()), rate);
        self.design_into(gains, rate, &mut sos)?;
        Ok(sos)
    }

    /// [`design`](Self::design) into `into`, reusing its sections' storage: no allocation
    /// when it holds room for [`count`](Self::count) sections.
    ///
    /// # Errors
    ///
    /// As [`design`](Self::design); `into` is then as it was.
    pub fn design_into(
        self,
        gains: &[Db; Self::MAX_COUNT],
        rate: SampleRate,
        into: &mut Sos,
    ) -> Result<(), SosError> {
        let q = self.q();
        let mut sections = [WIRE; Self::MAX_COUNT];
        for ((section, &center), &gain) in sections.iter_mut().zip(self.centers()).zip(gains) {
            if center < rate.nyquist() {
                *section = Cookbook::Peaking { q, gain }.section(center, rate)?;
            }
        }
        into.rate = rate;
        into.sections.clear();
        into.sections.extend_from_slice(&sections[..self.count()]);
        Ok(())
    }

    /// A ceiling on the ring-out of the bank over every gain within `+-largest`: the frames
    /// until `r^n` falls to `floor` plus a time constant (the decay of a single pole), `r`
    /// the greatest modulus among the sections' poles with every band at `largest` and at
    /// `-largest`. A bell's poles depend on its own gain alone and their modulus is monotone in
    /// it, so no gain set within the range has a slower one.
    ///
    /// The figure is the slowest pole's, not the cascade's own decay, which falls sooner (the
    /// modes of a bell have residues of order `alpha`): it errs long.
    /// [`DecayTime::Unbounded`] for a pole on or past the unit circle.
    ///
    /// # Errors
    ///
    /// [`SosError::GainOutOfRange`] for `largest` beyond
    /// [`MAX_GAIN_MAGNITUDE`](super::MAX_GAIN_MAGNITUDE).
    pub fn decay_ceiling(
        self,
        largest: Db,
        rate: SampleRate,
        floor: Db,
    ) -> Result<DecayTime, SosError> {
        let largest = Db::new(largest.get().abs());
        let radius = [largest, -largest]
            .into_iter()
            .map(|gain| {
                Ok(self
                    .design(&[gain; Self::MAX_COUNT], rate)?
                    .spectral_radius())
            })
            .try_fold(0.0f64, |r, radius| radius.map(|x: f64| r.max(x)))?;
        Ok(decay_time(radius, 1, floor, rate))
    }
}
