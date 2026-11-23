//! The high-pass and low-pass designs a cutoff control drives: the family and order of a
//! design, the cutoff range it is read within, and the ring-out over that range.

use tutti_units::{Db, Hz, SampleRate};

use super::{
    Butterworth, ButterworthOrder, ChebyshevI, ChebyshevII, CutResponse, Elliptic,
    EllipticTolerances, LinkwitzRiley, PassbandRipple, Sos, SosError, StopbandAttenuation,
};
use crate::{DecayTime, System};

/// The lowest cutoff a cut design takes: 10 Hz.
pub const MIN_CUT_CUTOFF: Hz = Hz::new(10.0);

/// The octaves from [`MIN_CUT_CUTOFF`] a cutoff below 192 kHz's Nyquist can sit in.
const LADDER: usize = 14;

/// The controls of a cut design.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutFilterControls {
    /// The cutoff: `-3 dB` for a Butterworth filter, `-6 dB` for a Linkwitz-Riley one, the
    /// passband edge of a Chebyshev I or elliptic one and the stopband edge of a Chebyshev II
    /// one. Read within [`MIN_CUT_CUTOFF`] and 0.99 of the Nyquist.
    pub cutoff: Hz,
}

impl CutFilterControls {
    /// 1 kHz.
    pub const DEFAULT: Self = Self {
        cutoff: Hz::new(1_000.0),
    };

    /// The cutoff held within [`MIN_CUT_CUTOFF`] and 0.99 of the Nyquist of `rate`.
    #[must_use]
    pub fn held(&self, rate: SampleRate) -> Hz {
        let top = rate.nyquist_scaled(0.99).get();
        Hz::new(self.cutoff.get().clamp(MIN_CUT_CUTOFF.get(), top))
    }
}

/// The family and order of a cut design.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum CutAlignment {
    /// A [`Butterworth`] filter of this order: `6 * order` dB per octave, `-3 dB` at the cutoff.
    Butterworth(ButterworthOrder),
    /// A [`LinkwitzRiley`] filter of twice this order (the Butterworth order it squares):
    /// `12 * order` dB per octave, `-6 dB` at the cutoff.
    LinkwitzRiley(ButterworthOrder),
    /// A [`ChebyshevI`] filter: equiripple in the passband, `-ripple` dB at the cutoff.
    ChebyshevI {
        /// The order.
        order: ButterworthOrder,
        /// The passband ripple.
        ripple: PassbandRipple,
    },
    /// A [`ChebyshevII`] filter: equiripple in the stopband, `-attenuation` dB at the cutoff,
    /// the stopband edge.
    ChebyshevII {
        /// The order.
        order: ButterworthOrder,
        /// The stopband attenuation.
        attenuation: StopbandAttenuation,
    },
    /// An [`Elliptic`] filter: equiripple in both bands, `-ripple` dB at the cutoff, the
    /// passband edge.
    Elliptic {
        /// The order.
        order: ButterworthOrder,
        /// The passband ripple and the stopband attenuation.
        tolerances: EllipticTolerances,
    },
}

/// A high-pass or low-pass of an [`alignment`](CutAlignment), the design a
/// [`CutFilterControls`] cutoff drives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutDesign {
    response: CutResponse,
    alignment: CutAlignment,
}

impl CutDesign {
    /// The `alignment` as a `response`.
    #[must_use]
    pub fn new(response: CutResponse, alignment: CutAlignment) -> Self {
        Self {
            response,
            alignment,
        }
    }

    /// The design at `controls`, held within [`CutFilterControls::held`], into `into`:
    /// no allocation when `into` holds room for the sections.
    ///
    /// # Errors
    ///
    /// The design's refusal of the cutoff; `into` is then as it was.
    pub fn design_into(
        &self,
        controls: &CutFilterControls,
        rate: SampleRate,
        into: &mut Sos,
    ) -> Result<(), SosError> {
        let cutoff = controls.held(rate);
        let response = self.response;
        match self.alignment {
            CutAlignment::Butterworth(order) => {
                Butterworth::new(response, order).design_into(cutoff, rate, into)
            }
            CutAlignment::LinkwitzRiley(half) => {
                LinkwitzRiley::new(response, half).design_into(cutoff, rate, into)
            }
            CutAlignment::ChebyshevI { order, ripple } => {
                ChebyshevI::new(response, order, ripple).design_into(cutoff, rate, into)
            }
            CutAlignment::ChebyshevII { order, attenuation } => {
                ChebyshevII::new(response, order, attenuation).design_into(cutoff, rate, into)
            }
            CutAlignment::Elliptic { order, tolerances } => {
                Elliptic::new(response, order, tolerances).design_into(cutoff, rate, into)
            }
        }
    }

    /// The ring-out at each octave from [`MIN_CUT_CUTOFF`] up, at `rate`, to `floor`.
    /// Allocates: it designs the filter at each octave.
    #[must_use]
    pub fn decay_ladder(&self, rate: SampleRate, floor: Db) -> CutDecayLadder {
        let top = rate.nyquist_scaled(0.99);
        let mut sos = Sos::new(Vec::new(), rate);
        let rungs = core::array::from_fn(|rung| {
            let controls = CutFilterControls {
                cutoff: rung_cutoff(rung).min(top),
            };
            match self.design_into(&controls, rate, &mut sos) {
                Ok(()) => sos.tail(floor),
                Err(_) => DecayTime::Unbounded,
            }
        });
        CutDecayLadder { rungs }
    }
}

/// The cutoff of ladder rung `rung`: `rung` octaves above [`MIN_CUT_CUTOFF`].
fn rung_cutoff(rung: usize) -> Hz {
    MIN_CUT_CUTOFF * f64::from(1u32 << rung)
}

/// The ring-out of a [`CutDesign`] at each octave from [`MIN_CUT_CUTOFF`], found without
/// simulating the filter at the cutoff in force: a lower cutoff rings longer, so the octave
/// below a cutoff errs long.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutDecayLadder {
    /// The ring-out at each octave from [`MIN_CUT_CUTOFF`] up (`LADDER` rungs at most); the
    /// rungs past the Nyquist's are the Nyquist's.
    rungs: [DecayTime; LADDER],
}

impl CutDecayLadder {
    /// The ring-out over every cutoff: the lowest octave's.
    #[must_use]
    pub fn ceiling(&self) -> DecayTime {
        self.rungs[0]
    }

    /// The ring-out at the highest octave from [`MIN_CUT_CUTOFF`] at or below `controls`'
    /// cutoff held within its range at `rate`.
    #[must_use]
    pub fn at(&self, controls: &CutFilterControls, rate: SampleRate) -> DecayTime {
        let cutoff = controls.held(rate);
        let octave = (1..LADDER)
            .take_while(|&rung| rung_cutoff(rung) <= cutoff)
            .count();
        self.rungs[octave]
    }
}

#[cfg(test)]
mod tests;
