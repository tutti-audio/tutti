//! [`KeyFilter`]: the filter on a detector's key, and the stage that runs it.

use core::ops::Range;

use tutti_buffers::{ChannelLayout, MAX_CHANNELS};
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_units::{Hz, Q, SampleRate, Samples};

use super::gain_pass::{CHUNK, level};
use crate::{
    COEFF_INTERVAL, Lerp, Ramp, SimdLanes, SvfBank, SvfCoefficients, SvfControls, SvfModel, SvfType,
};

/// The filter a compressor's or a gate's detector hears its key through: a type of the
/// node, so a graph's controls and its sound say the same thing.
///
/// The filter is on the **key only**. The detector measures the filtered key; the signal
/// the gain is applied to is never filtered. A high-pass keeps bass out of the detector (a
/// kick that would pump a mix compressor, a rumble that holds a gate open); a band-pass
/// makes a de-esser or a frequency-conscious ducker. Its cutoff and Q are controls of the
/// node ([`CompressorControls::key_cutoff`](super::CompressorControls::key_cutoff)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum KeyFilter {
    /// The detector hears the key as it is. The node has no key-filter controls.
    #[default]
    Off,
    /// A 12 dB per octave high-pass: the key below the cutoff is attenuated.
    HighPass,
    /// A band-pass around the cutoff, the SVF's constant-skirt-gain form
    /// ([`SvfType::BandPass`]): its peak gain at the cutoff is `Q`.
    BandPass,
}

impl KeyFilter {
    /// The default cutoff of a key filter: 100 Hz.
    pub const DEFAULT_CUTOFF: Hz = Hz::new(100.0);

    /// The default Q of a key filter: Butterworth, the flattest high-pass.
    pub const DEFAULT_Q: Q = Q::BUTTERWORTH;

    /// The SVF response this mode is, or `None` for [`Off`](Self::Off).
    #[must_use]
    pub const fn response(self) -> Option<SvfType> {
        match self {
            Self::Off => None,
            Self::HighPass => Some(SvfType::HighPass),
            Self::BandPass => Some(SvfType::BandPass),
        }
    }
}

/// A detector's key filter at a rate: an [`SvfBank`] over the key's channels and the
/// coefficients it ramps to, one grid period ([`COEFF_INTERVAL`]) a design.
#[derive(Clone, Debug)]
pub(super) struct KeyStage<F: Float + SimdLanes> {
    ty: SvfType,
    bank: SvfBank<F>,
    coefficients: Ramp<SvfCoefficients<F>>,
    rate: SampleRate,
}

impl<F: Float + SimdLanes + Lerp> KeyStage<F> {
    /// A stage of response `ty` over `width` key channels, at the default cutoff and Q.
    /// Allocates: build it on the control side.
    pub(super) fn new(ty: SvfType, width: ChannelLayout, rate: SampleRate) -> Self {
        Self {
            ty,
            bank: SvfBank::new(width),
            coefficients: Ramp::new(Self::design(
                ty,
                KeyFilter::DEFAULT_CUTOFF,
                KeyFilter::DEFAULT_Q,
                rate,
            ))
            .steps(COEFF_INTERVAL as u32),
            rate,
        }
    }

    fn design(ty: SvfType, cutoff: Hz, q: Q, rate: SampleRate) -> SvfCoefficients<F> {
        let controls = SvfControls {
            ty,
            freq: cutoff,
            q,
            ..SvfControls::DEFAULT
        };
        SvfCoefficients::from(&SvfModel::new(controls, rate))
    }

    /// Silence in the filter's state, and the coefficients at rest.
    pub(super) fn reset(&mut self) {
        self.bank.reset();
        self.coefficients.jump_to_target();
    }

    /// Lands the coefficients on their design now.
    pub(super) fn jump_to_target(&mut self) {
        self.coefficients.jump_to_target();
    }

    /// Aims at `cutoff` and `q`: the coefficients ramp there over one grid period.
    pub(super) fn retarget(&mut self, cutoff: Hz, q: Q) {
        self.coefficients
            .retarget(Self::design(self.ty, cutoff, q, self.rate));
    }

    /// The key's level at each frame of `frames`, into `out` (as long as `frames`, at most
    /// [`CHUNK`]): the loudest channel of the filtered key. Key channels past the stage's
    /// width are not heard. A frame where the filter's output is not finite reads the
    /// unfiltered key's level there (a NaN as quiet, an infinity as the largest finite level),
    /// and the filter's state resets after the chunk, so a non-finite key is heard for at most
    /// one chunk. No allocation.
    pub(super) fn levels(&mut self, key: &Planes<'_, F>, frames: Range<usize>, out: &mut [F]) {
        out.fill(F::ZERO);
        let n = frames.len().min(CHUNK);
        let heard = key.slice(Samples::new(frames.start)..Samples::new(frames.start + n));
        let width = heard.channels().min(MAX_CHANNELS);
        let mut scratch = [[F::ZERO; CHUNK]; MAX_CHANNELS];
        let mut outs: [&mut [F]; MAX_CHANNELS] = scratch.each_mut().map(|p| &mut p[..n]);
        // Every plane is `n` frames long and `width` is within `MAX_CHANNELS`.
        let Ok(mut filtered) = PlanesMut::new(&mut outs[..width]) else {
            debug_assert!(false, "the scratch planes are one length");
            return;
        };
        self.bank.run(&mut self.coefficients, &heard, &mut filtered);
        let mut non_finite = false;
        for (plane, raw) in filtered.iter().zip(heard.iter()) {
            for ((m, &y), &x) in out.iter_mut().zip(plane.samples()).zip(raw.samples()) {
                let l = if y.is_finite() {
                    level(y)
                } else {
                    non_finite = true;
                    level(x)
                };
                *m = m.max(l);
            }
        }
        if non_finite {
            self.bank.reset();
        }
    }
}
