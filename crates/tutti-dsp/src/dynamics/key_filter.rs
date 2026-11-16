//! [`KeyFilter`]: the filter on a detector's key, and the stage that runs it.

use core::ops::Range;

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::{ChannelLayout, Hz, MAX_CHANNELS, Q, SampleRate, Samples};

use super::shared::{CHUNK, level};
use crate::{COEFF_INTERVAL, Glide, SvfBank, SvfCoefficients, SvfControls, SvfModel, SvfType};

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
    pub const DEFAULT_CUTOFF: Hz = Hz(100.0);

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
/// coefficients it glides to, one grid period ([`COEFF_INTERVAL`]) a design.
#[derive(Clone, Debug)]
pub(super) struct KeyStage {
    ty: SvfType,
    bank: SvfBank<f32>,
    coefficients: Glide<SvfCoefficients<f32>>,
    rate: SampleRate,
}

impl KeyStage {
    /// A stage of response `ty` over `width` key channels, at the default cutoff and Q.
    /// Allocates: build it on the control side.
    pub(super) fn new(ty: SvfType, width: ChannelLayout, rate: SampleRate) -> Self {
        Self {
            ty,
            bank: SvfBank::new(width),
            coefficients: Glide::landed(Self::design(
                ty,
                KeyFilter::DEFAULT_CUTOFF,
                KeyFilter::DEFAULT_Q,
                rate,
            ))
            .by(COEFF_INTERVAL as u32),
            rate,
        }
    }

    fn design(ty: SvfType, cutoff: Hz, q: Q, rate: SampleRate) -> SvfCoefficients<f32> {
        let controls = SvfControls {
            ty,
            freq: cutoff,
            q,
            ..SvfControls::DEFAULT
        };
        SvfCoefficients::from(&SvfModel::new(controls, rate))
    }

    /// Silence in the filter's state, and the coefficients landed.
    pub(super) fn reset(&mut self) {
        self.bank.reset();
        self.coefficients.settle();
    }

    /// Lands the coefficients on their design now.
    pub(super) fn settle(&mut self) {
        self.coefficients.settle();
    }

    /// Aims at `cutoff` and `q`: the coefficients glide there over one grid period.
    pub(super) fn aim(&mut self, cutoff: Hz, q: Q) {
        self.coefficients
            .aim(Self::design(self.ty, cutoff, q, self.rate));
    }

    /// The key's level at each frame of `frames`, into `out` (as long as `frames`, at most
    /// [`CHUNK`]): the loudest channel of the filtered key. No allocation.
    pub(super) fn levels<S: Float>(
        &mut self,
        key: &Planes<'_, S>,
        frames: Range<usize>,
        out: &mut [f32],
    ) {
        out.fill(0.0);
        let n = frames.len().min(CHUNK);
        let heard = key.slice(Samples(frames.start)..Samples(frames.start + n));
        let mut scratch = [[S::ZERO; CHUNK]; MAX_CHANNELS];
        let mut views: [&mut [S]; MAX_CHANNELS] = scratch.each_mut().map(|p| &mut p[..n]);
        let width = heard.channels().min(MAX_CHANNELS);
        let Ok(mut filtered) = PlanesMut::new(&mut views[..width]) else {
            return;
        };
        self.bank
            .process(&mut self.coefficients, heard, filtered.reborrow());
        for plane in filtered.iter() {
            for (m, &x) in out.iter_mut().zip(plane.samples()) {
                *m = m.max(level(x.convert_to::<f32>()));
            }
        }
    }
}
