//! [`KeyFilter`]: the filter on a detector's key, and the stage that runs it.

use core::ops::Range;

use tutti_buffers::{ChannelLayout, MAX_CHANNELS};
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_rt::atomic_word;
use tutti_units::{Hz, Q, SampleRate};

use super::gain_pass::{CHUNK, key_levels, level};
use crate::{
    COEFF_INTERVAL, Lerp, Ramp, SimdLanes, SvfBank, SvfCoefficients, SvfControls, SvfModel, SvfType,
};

/// The filter a compressor's or a gate's detector hears its key through: a setting a
/// running bank reads ([`CompressorControls::key_filter`](super::CompressorControls::key_filter)),
/// switched in and out like a front-panel button.
///
/// The filter is on the **key only**. The detector measures the filtered key; the signal
/// the gain is applied to is never filtered. A high-pass keeps bass out of the detector (a
/// kick that would pump a mix compressor, a rumble that holds a gate open); a band-pass
/// makes a de-esser or a frequency-conscious ducker. Its cutoff and Q are controls of the
/// controls ([`CompressorControls::key_cutoff`](super::CompressorControls::key_cutoff)), which
/// are kept while the filter is off.
///
/// A switch from [`Off`](Self::Off) to a response starts the filter from silence, at the
/// cutoff and Q the controls hold; a switch between the two responses keeps the filter's
/// state, which both read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum KeyFilter {
    /// The detector hears the key as it is, and the filter is not run.
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

atomic_word!(KeyFilter {
    Off,
    HighPass,
    BandPass
});

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

    /// Takes the response `ty`: the next [`retarget`](Self::retarget) designs it. The
    /// integrators are untouched, both responses of a key filter reading the same two.
    fn respond(&mut self, ty: SvfType) {
        self.ty = ty;
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
        let width = key.channels().min(MAX_CHANNELS).min(self.bank.channels());
        let span = frames.start..frames.start + n;
        let planes: [&[F]; MAX_CHANNELS] = core::array::from_fn(|c| {
            if c < width {
                &key.plane(c).samples()[span.clone()]
            } else {
                &[]
            }
        });
        // Every plane is `n` frames long and `width` is within `MAX_CHANNELS`.
        let Ok(heard) = Planes::new(&planes[..width]) else {
            debug_assert!(false, "the key's planes are one length");
            return;
        };
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

/// What a compressor's or a gate's detector hears the key through: a [`KeyStage`] that a
/// [`KeyFilter`] switches in and out.
///
/// Built for `width` key channels on the control side. While the filter is
/// [`Off`](KeyFilter::Off) the stage is not run and the detector reads the key as it is.
#[derive(Clone, Debug)]
pub(super) struct KeyPath<F: Float + SimdLanes> {
    stage: KeyStage<F>,
    /// The response the stage runs, or `None` while it is off.
    on: Option<SvfType>,
}

impl<F: Float + SimdLanes + Lerp> KeyPath<F> {
    /// A path that is off, over `width` key channels at `rate`. Allocates: build it on the
    /// control side.
    pub(super) fn new(width: ChannelLayout, rate: SampleRate) -> Self {
        Self {
            stage: KeyStage::new(SvfType::HighPass, width, rate),
            on: None,
        }
    }

    /// Silence in the stage, and its coefficients at rest.
    pub(super) fn reset(&mut self) {
        self.stage.reset();
    }

    /// Lands the stage's coefficients on their design now.
    pub(super) fn jump_to_target(&mut self) {
        self.stage.jump_to_target();
    }

    /// Takes `filter` at `cutoff` and `q`. Off, the stage is left as it is and the cutoff
    /// and Q are not read. A switch in from off starts the stage from silence at once on the
    /// cutoff and Q, so no state of an earlier on-period is heard; a switch between the two
    /// responses ramps the coefficients over one grid period and keeps the state, which both
    /// read. No allocation.
    pub(super) fn retarget(&mut self, filter: KeyFilter, cutoff: Hz, q: Q) {
        let Some(ty) = filter.response() else {
            self.on = None;
            return;
        };
        self.stage.respond(ty);
        self.stage.retarget(cutoff, q);
        if self.on.replace(ty).is_none() {
            self.stage.reset();
        }
    }

    /// The key's level at each frame of `frames`, into `out` (as long as `frames`, at most
    /// [`CHUNK`]): the loudest channel of the key, filtered while the filter is on. No
    /// allocation.
    pub(super) fn levels(&mut self, key: &Planes<'_, F>, frames: Range<usize>, out: &mut [F]) {
        match self.on {
            Some(_) => self.stage.levels(key, frames, out),
            None => key_levels(key, frames, out),
        }
    }
}

#[cfg(test)]
mod tests;
