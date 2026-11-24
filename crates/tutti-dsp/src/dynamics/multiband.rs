//! [`MultibandBank`]: the multiband compressor over buffers views.
//!
//! A Linkwitz-Riley crossover ([`CrossoverBank`], one band tree per channel) splits the
//! signal into 2 to 4 bands; each band is compressed by its own [`CompressorBank`] with a
//! gain **linked** across channels, from the loudest channel of the band; and the bands are
//! summed. The detector is the band itself: there is no side-chain.
//!
//! A band may be **bypassed** (its gain goes to unity, the makeup with it, while its
//! compressor keeps running so a release finds it warm) and **soloed** (while any band is
//! soloed the output is the sum of the soloed bands, the others muted but still metering).
//! Both are weights of 0 or 1 that ramp over [`GAIN_RAMP`], like a makeup: a step in
//! either multiplies the output directly, so it would click.
//!
//! The crossover ramps to each design over a grid period of [`COEFF_INTERVAL`] frames, as
//! [`CrossoverBank`]'s does; each compressor's detector lands whole, and the makeups and the
//! two weights ramp over [`GAIN_RAMP`]. Each band's threshold is a per-frame control signal
//! ([`MultibandSignals`]).
//!
//! The controls are `MultibandSettings` and `BandSettings`, not `*Controls`: the node's
//! `MultibandControls` (its handle on the params and meters) is a different type.
//!
//! The crossover is IIR, so the bands ring on after the input stops (a tail the bank leaves
//! to its caller to declare), and the compressors only scale. There is no latency.

use core::ops::Range;

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{Float, InputOutput, Mono, Planes, PlanesMut};
use tutti_units::{Db, Hz, SampleRate, Samples};

use super::compressor::{CompressorBank, CompressorControls, CompressorSignals};
use super::gain_pass::{CHUNK, GAIN_RAMP};
use crate::{
    COEFF_INTERVAL, CrossoverBands, CrossoverBank, CrossoverBlend, CrossoverControls,
    CrossoverModel, Lerp, Ramp, SimdLanes, Steps,
};

/// The most bands.
const MAX_BANDS: usize = BankShape::MAX_BANDS;

/// The controls of one band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandSettings {
    compressor: CompressorControls,
    bypass: bool,
    solo: bool,
}

impl BandSettings {
    /// A band that does nothing: [`CompressorControls::DEFAULT`], neither bypassed nor
    /// soloed.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            compressor: CompressorControls::DEFAULT,
            bypass: false,
            solo: false,
        }
    }

    /// The band's compressor.
    #[must_use]
    pub const fn compressor(mut self, compressor: CompressorControls) -> Self {
        self.compressor = compressor;
        self
    }

    /// Passes the band unreduced and without makeup.
    #[must_use]
    pub const fn bypass(mut self, bypass: bool) -> Self {
        self.bypass = bypass;
        self
    }

    /// Solos the band: while any band is soloed, only the soloed ones sound.
    #[must_use]
    pub const fn solo(mut self, solo: bool) -> Self {
        self.solo = solo;
        self
    }
}

impl Default for BandSettings {
    fn default() -> Self {
        Self::new()
    }
}

/// The controls of a [`MultibandBank`]: the split frequencies and a [`BandSettings`] each
/// band. Entries past the bank's bands (and past its splits) are ignored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MultibandSettings {
    crossover: CrossoverControls,
    bands: [BandSettings; MAX_BANDS],
}

impl MultibandSettings {
    /// A multiband that does nothing but split and sum: every band at
    /// [`BandSettings::new`], the splits at 200 Hz, 1 kHz and 5 kHz.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            crossover: CrossoverControls {
                frequencies: [
                    Hz::new(200.0),
                    Hz::new(1_000.0),
                    Hz::new(5_000.0),
                    Hz::ZERO,
                    Hz::ZERO,
                ],
            },
            bands: [BandSettings::new(); MAX_BANDS],
        }
    }

    /// The crossover's split frequencies, lowest first.
    #[must_use]
    pub const fn crossover(mut self, crossover: CrossoverControls) -> Self {
        self.crossover = crossover;
        self
    }

    /// The bands, lowest first.
    #[must_use]
    pub const fn bands(mut self, bands: [BandSettings; MAX_BANDS]) -> Self {
        self.bands = bands;
        self
    }
}

impl Default for MultibandSettings {
    fn default() -> Self {
        Self::new()
    }
}

/// The per-frame control signals a multiband reads beside its audio, in the bank's float, `None` where
/// nothing drives them. A control signal is at least as long as the frames a call renders; a frame past
/// its end is not driven.
#[derive(Clone, Copy, Debug, Default)]
pub struct MultibandSignals<'a, F: Float> {
    /// Band `b`'s threshold in dB. A frame that is not finite is the set threshold.
    pub thresholds: [Option<Mono<'a, F>>; MAX_BANDS],
}

/// Each band's largest reduction since the last read, in dB and positive: a
/// **measurement**, for a reduction meter. A band is left out of it only for a whole chunk
/// of 64 frames of a call on which its bypass weight is 1 (a bypassed band passes none of
/// its reduction); any chunk with a frame not bypassed counts. It carries the bank's band
/// count, so a reader cannot ask about a band that is not there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandReductions {
    deepest: [Db; MAX_BANDS],
    bands: usize,
}

impl BandReductions {
    /// Band `band`'s deepest reduction ([`Db::UNITY`], 0 dB, is none), or `None` past the
    /// last band.
    #[must_use]
    pub fn band(&self, band: usize) -> Option<Db> {
        self.deepest[..self.bands].get(band).copied()
    }

    /// Every band's deepest reduction, in band order: exactly the bank's bands.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Db> + '_ {
        self.deepest[..self.bands].iter().copied()
    }
}

tutti_units::ranged! {
    /// How many bands a [`MultibandBank`] splits into, `2..=4`: a band count the crossover
    /// design holds.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    MultibandBands(u8), MultibandBandsError, "a band count within 2..=4",
    |raw| raw as usize >= BankShape::MIN_BANDS && raw as usize <= BankShape::MAX_BANDS
}

/// The width and band count of a [`MultibandBank`]: each band has the channels of the input
/// bus. A bus holds at most [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) channels, so
/// `width * bands` fits the planes of a crossover and nothing here can be refused. The check
/// does not depend on the bank's float.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BankShape {
    width: ChannelLayout,
    bands: MultibandBands,
}

impl BankShape {
    /// The fewest bands.
    pub const MIN_BANDS: usize = 2;
    /// The most bands.
    pub const MAX_BANDS: usize = 4;

    /// A shape of `bands` bands, each as wide as the bus `input`.
    #[must_use]
    pub fn new(input: ChannelTopology, bands: MultibandBands) -> Self {
        Self {
            width: input.width(),
            bands,
        }
    }

    /// The channels each band has.
    #[must_use]
    pub fn width(&self) -> ChannelLayout {
        self.width
    }

    /// The bands, 2 to 4.
    #[must_use]
    pub fn bands(&self) -> usize {
        usize::from(self.bands.get())
    }
}

/// The planes a chunk of [`CHUNK`] frames passes through, sized when the bank is built.
#[derive(Clone, Debug)]
struct Scratch<F> {
    /// The input, in `F`, a plane per channel.
    input: Vec<Vec<F>>,
    /// The crossover's bands, band-major: plane `band * width + channel`.
    bands: Vec<Vec<F>>,
    /// One band, compressed.
    squeezed: Vec<Vec<F>>,
    /// The bands summed.
    sum: Vec<Vec<F>>,
}

impl<F: Float> Scratch<F> {
    fn new(width: usize, bands: usize) -> Self {
        let planes = |n: usize| vec![vec![F::ZERO; CHUNK]; n];
        Self {
            input: planes(width),
            bands: planes(width * bands),
            squeezed: planes(width),
            sum: planes(width),
        }
    }
}

/// A multiband compressor of `bands` bands over a fixed width: the crossover, a
/// [`CompressorBank`] a band, and the weights that bypass and solo.
///
/// Built on the control side for one rate; the state, the views and the control signals are `F`, the
/// graph's float. Nothing it holds grows.
#[derive(Clone, Debug)]
pub struct MultibandBank<F: Float + SimdLanes + Lerp> {
    width: usize,
    bands: usize,
    rate: SampleRate,
    crossover: CrossoverBank<F>,
    /// The model the controls design, in place.
    model: CrossoverModel,
    /// The blend in flight over one grid period.
    blend: Ramp<CrossoverBlend>,
    compressors: [CompressorBank<F>; MAX_BANDS],
    /// 1 while the band is bypassed, else 0, ramping over [`GAIN_RAMP`]: a per-sample weight
    /// in `F`.
    bypass: [Ramp<F>; MAX_BANDS],
    /// 1 while the band is heard, else 0, ramping over [`GAIN_RAMP`]: a per-sample weight in
    /// `F`.
    audible: [Ramp<F>; MAX_BANDS],
    /// The steps a weight ramp takes: [`GAIN_RAMP`] at the rate.
    steps: u32,
    /// Each band's deepest reduction since the last reading, in dB.
    deepest: [Db; MAX_BANDS],
    scratch: Scratch<F>,
}

impl<F: Float + SimdLanes + Lerp> MultibandBank<F> {
    /// A multiband of a checked `shape` at `rate`, splitting and summing until it is set.
    /// Allocates: build it on the control side.
    ///
    /// # Panics
    ///
    /// Never: the one `expect` is on the crossover design's band count, which a shape keeps
    /// within 2 to 4.
    #[must_use]
    pub fn new(shape: BankShape, rate: SampleRate) -> Self {
        let BankShape { width, bands } = shape;
        let bands = usize::from(bands.get());
        let channels = usize::from(width.count());
        let split = CrossoverBands::new(bands)
            .expect("a shape's bands are 2 to 4, inside a crossover's 2 to 6");
        let model = CrossoverModel::designed(split, &CrossoverControls::octaves(rate), rate);
        // 5 ms is a few hundred frames at any real rate; an absurd rate saturates rather
        // than panic, since a prepare must not.
        let steps = u32::try_from(GAIN_RAMP.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        Self {
            width: channels,
            bands,
            rate,
            crossover: CrossoverBank::new(width),
            blend: Ramp::new(model.blend()).steps(COEFF_INTERVAL as u32),
            model,
            compressors: core::array::from_fn(|_| CompressorBank::new(width, rate)),
            bypass: [Ramp::new(F::ZERO); MAX_BANDS],
            audible: [Ramp::new(F::ONE); MAX_BANDS],
            steps,
            deepest: [Db::UNITY; MAX_BANDS],
            scratch: Scratch::new(channels, bands),
        }
    }

    /// How many bands it makes.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.bands
    }

    /// Back to silence: the state cleared, the ramps at rest on their targets, the meters at
    /// none. The design and the bands' controls are kept. No allocation.
    pub fn reset(&mut self) {
        self.crossover.reset();
        for compressor in &mut self.compressors {
            compressor.reset();
        }
        self.jump_ramps();
        self.deepest = [Db::UNITY; MAX_BANDS];
    }

    /// Lands every ramp (the crossover's blend, each band's makeup and key filter, bypass and
    /// audible weights) on its target now.
    pub fn jump_to_target(&mut self) {
        for compressor in &mut self.compressors {
            compressor.jump_to_target();
        }
        self.jump_ramps();
    }

    /// Lands the blend and the weights on their targets.
    fn jump_ramps(&mut self) {
        self.blend.jump_to_target();
        for (bypass, audible) in self.bypass.iter_mut().zip(&mut self.audible) {
            bypass.jump_to_target();
            audible.jump_to_target();
        }
    }

    /// Aims at `controls`: the crossover is designed in place (total: a set out of order is
    /// solved with its splits no lower than the one before) and ramps to the design over a
    /// grid period, each band's compressor is set as [`CompressorBank::retarget`] does, and the
    /// bypass and solo weights ramp over [`GAIN_RAMP`]. No allocation.
    pub fn retarget(&mut self, controls: &MultibandSettings) {
        self.model.design_into(&controls.crossover, self.rate);
        self.blend.retarget(self.model.blend());
        let bands = &controls.bands[..self.bands];
        let any_solo = bands.iter().any(|b| b.solo);
        let steps = Steps::Fixed(self.steps);
        for (b, band) in bands.iter().enumerate() {
            self.compressors[b].retarget(&band.compressor);
            self.bypass[b].retarget_by(flag(band.bypass), steps);
            self.audible[b].retarget_by(flag(!any_solo || band.solo), steps);
        }
    }

    /// Each band's deepest reduction since the last call, and the meters cleared. No
    /// allocation.
    pub fn take_reading(&mut self) -> BandReductions {
        BandReductions {
            deepest: core::mem::replace(&mut self.deepest, [Db::UNITY; MAX_BANDS]),
            bands: self.bands,
        }
    }

    /// Runs `input` into `output`: the bands of each channel, compressed and summed.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`. Frame `i` reads the weights at
    /// the ramp's step `taken + 1 + i`, then the block's steps are taken. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process(
        &mut self,
        signals: MultibandSignals<'_, F>,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        expect_widths(block, self.width, self.width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let mut start = 0;
        while start < frames {
            let end = (start + CHUNK).min(frames);
            self.chunk(&signals, &input, &mut output, start..end);
            start = end;
        }
        Ok(())
    }

    /// Runs frames `at` of the views, at most [`CHUNK`] of them.
    fn chunk(
        &mut self,
        signals: &MultibandSignals<'_, F>,
        input: &Planes<'_, F>,
        output: &mut PlanesMut<'_, '_, F>,
        at: Range<usize>,
    ) {
        let n = at.len();
        let width = self.width;
        let scratch = &mut self.scratch;
        for (c, plane) in scratch.input.iter_mut().enumerate() {
            let dst = &mut plane[..n];
            match input.get_plane(c) {
                Some(src) => dst.copy_from_slice(&src.samples()[at.clone()]),
                None => dst.fill(F::ZERO),
            }
        }
        for plane in &mut scratch.sum {
            plane[..n].fill(F::ZERO);
        }
        let frames = Samples::new(0)..Samples::new(n);
        {
            let (Ok(heard), Ok(mut split)) = (
                Planes::from_vecs(&scratch.input),
                PlanesMut::from_vecs(&mut scratch.bands[..self.bands * width]),
            ) else {
                return;
            };
            let Ok(mut block) = InputOutput::new(
                Samples::new(n),
                heard.slice(frames.clone()),
                split.slice_mut(frames.clone()),
            ) else {
                debug_assert!(false, "the scratch planes are `n` frames long");
                return;
            };
            let crossed = self.crossover.process(&mut self.blend, &mut block);
            debug_assert!(crossed.is_ok(), "the scratch is the crossover's width");
        }
        let mut by = [F::ZERO; CHUNK];
        let mut au = [F::ZERO; CHUNK];
        let mut signal = [F::ZERO; CHUNK];
        // The bands are `width` planes each, band-major.
        for (b, band_planes) in scratch.bands.chunks(width).take(self.bands).enumerate() {
            {
                let (Ok(band), Ok(mut squeezed)) = (
                    Planes::from_vecs(band_planes),
                    PlanesMut::from_vecs(&mut scratch.squeezed),
                ) else {
                    return;
                };
                let band = band.slice(frames.clone());
                // A frame past the control signal's end is not finite, so not driven.
                let threshold = signals.thresholds[b].map(|l| {
                    let all = l.samples();
                    let driven = &all[at.start.min(all.len())..at.end.min(all.len())];
                    for (i, t) in signal[..n].iter_mut().enumerate() {
                        *t = driven.get(i).copied().unwrap_or(F::NAN);
                    }
                    Mono::new(&signal[..n])
                });
                let Ok(mut block) =
                    InputOutput::new(Samples::new(n), band, squeezed.slice_mut(frames.clone()))
                else {
                    debug_assert!(false, "the scratch planes are `n` frames long");
                    return;
                };
                let squeezed =
                    self.compressors[b].process(CompressorSignals { threshold }, band, &mut block);
                debug_assert!(squeezed.is_ok(), "the scratch is the compressor's width");
            }
            // The meter counts a band unless the whole chunk is bypassed.
            let deepest = self.compressors[b].take_deepest();
            let bypass = &self.bypass[b];
            if bypass.blended_frames(n) != 0 || bypass.target() < F::ONE {
                self.deepest[b] = self.deepest[b].max(deepest);
            }
            weights(bypass, &mut by[..n]);
            weights(&self.audible[b], &mut au[..n]);
            let planes = band_planes
                .iter()
                .zip(&scratch.squeezed)
                .zip(&mut scratch.sum);
            for ((x, y), sum) in planes {
                let (x, y) = (&x[..n], &y[..n]);
                for (i, s) in sum[..n].iter_mut().enumerate() {
                    let (w, a) = (by[i], au[i]);
                    if a == F::ZERO {
                        continue;
                    }
                    // The blend toward the unreduced band, makeup included, as the bypass
                    // takes hold; at either end it is exactly one of the two.
                    let v = if w == F::ZERO {
                        y[i]
                    } else if w == F::ONE {
                        x[i]
                    } else {
                        crate::lerp(y[i], x[i], w)
                    };
                    *s += if a == F::ONE { v } else { v * a };
                }
            }
        }
        let steps = u32::try_from(n).unwrap_or(u32::MAX);
        for b in 0..self.bands {
            self.bypass[b].advance(steps);
            self.audible[b].advance(steps);
        }
        for (c, sum) in scratch.sum[..width].iter().enumerate() {
            let mut plane = output.plane_mut(c);
            plane.samples_mut()[at.clone()].copy_from_slice(&sum[..n]);
        }
    }
}

/// A flag as a weight: 1 if set, else 0.
fn flag<F: Float>(on: bool) -> F {
    if on { F::ONE } else { F::ZERO }
}

/// The weight of `ramp` at each of the next frames: its blend while it moves, then its
/// target.
#[inline]
fn weights<F: Float + Lerp>(ramp: &Ramp<F>, out: &mut [F]) {
    let blended = ramp.blended_frames(out.len());
    let taken = ramp.steps_taken();
    for (i, w) in out.iter_mut().enumerate() {
        *w = if i < blended {
            ramp.at(taken.saturating_add(1).saturating_add(i as u32))
        } else {
            ramp.target()
        };
    }
}

#[cfg(test)]
mod tests;
