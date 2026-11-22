//! EBU R128 loudness — integrated LUFS, loudness range, and true peak, and
//! the momentary and short-term readings of a running meter.
//!
//! The measurement half of normalization. Applying a gain is not analysis: it
//! is `target - measured` and a multiply, which the caller composes (see
//! [`Loudness::gain_to`]). Keeping the two apart is what lets an exporter
//! stream — measure while rendering, apply on a second pass — instead of
//! holding the whole signal to do both at once.
//!
//! `ebur128` is an online meter, so this follows the crate's streaming shape:
//! a [`LoudnessConfig`] describes the channels, [`Measure::measure`]
//! binds the rate and makes a [`LoudnessMeter`] that takes the frames in
//! chunks, and [`measure_loudness`] folds the same meter over a whole buffer.
//!
//! # A running meter
//!
//! [`WindowedMeter`]'s push answers a different question from
//! [`LoudnessMeter`]'s finish: not "how loud was the whole signal" but "how loud
//! is it now", as a [`WindowedReading`] after every chunk. Momentary loudness
//! is the last 400 ms and short-term the last 3 s, both K-weighted and ungated
//! (EBU Tech 3341). The true peak is the largest seen since the meter was built
//! or [reset](WindowedMeter::reset).
//!
//! # Interleaved in, unit-typed out
//!
//! `push` takes an [`Interleaved`] — the shape a render loop already
//! has, carrying its own width. Results come back as [`Db`]: LUFS and dBTP
//! are both decibel readings.
//!
//! # Weighted by placement
//!
//! A config takes a [`ChannelTopology`], and each speaker's weight ([`channel_weights`]) is read from
//! where it stands (ITU-R BS.1770-5, Annex 3, Table 4): 1.41 when its elevation
//! is under 30 degrees and its azimuth is within 60 to 120 degrees either side,
//! else 1.0; the LFE adds no loudness (its peak still counts). The angles are the bed's own
//! ([`Bed::direction`](tutti_buffers::Bed::direction)), so a 5.1's side speakers (M+110 and M-110) weigh 1.41 and a
//! 7.1's rear speakers (M+135 and M-135) and every upper speaker weigh 1.0, as Table 5
//! lists them. A quad's side speakers (VST3's FL FR SL SR) stand for the bed's
//! M+-135 backs and weigh 1.0. `ebur128` has no free weights, so each speaker is given the
//! channel label of its weight. A topology without positions (an unplaced set of
//! speakers) and an ambisonic one are refused: neither has an angle to weigh.

use core::marker::PhantomData;

use ebur128::{Channel, EbuR128, Mode};
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{Interleaved, Sample};
use tutti_dsp::{ChannelWeight, channel_weights};
use tutti_units::{Amplitude, Db, SampleRate, Samples};

use crate::convert::{BLOCK, all_finite, for_blocks, run_len};
use crate::measure::measure_buffer;
use crate::{Measure, MeterError, StreamMeter};

/// What a loudness meter measures: the channel topology of the frames fed to
/// it. Unlike a width, a topology is part of the analysis: it is the channel
/// model each speaker's weight is read from. [`measure_loudness`] checks the
/// buffer's width against it ([`MeterError::ChunkLayoutMismatch`]).
///
/// The rate is **not** part of the config and **not** defaulted: the call that
/// takes the frames binds it ([`measure`](Self::measure),
/// [`measure_loudness`]). Hardcode it to 48 kHz and a 44.1 kHz render measures
/// its peaks through a filter built for the wrong rate and normalizes to a
/// biased target.
///
/// The topology says where each channel stands, and with it how much the
/// channel counts: see the module docs. The call that takes the frames refuses
/// a topology that has no placement to weigh.
#[derive(Debug, Clone, PartialEq)]
pub struct LoudnessConfig {
    topology: ChannelTopology,
}

impl LoudnessConfig {
    /// A config for frames in `topology`.
    #[must_use]
    pub fn new(topology: ChannelTopology) -> Self {
        Self { topology }
    }

    /// The topology of the fed frames. Fixed at meter construction: a chunk of
    /// a different width is refused ([`MeterError::ChunkLayoutMismatch`]).
    #[inline]
    #[must_use]
    pub fn topology(&self) -> &ChannelTopology {
        &self.topology
    }
}

impl<S: Sample> Measure<S> for LoudnessConfig {
    type Context = SampleRate;
    type Meter = LoudnessMeter<S>;

    /// A meter for frames at `rate`.
    ///
    /// # Errors
    ///
    /// [`MeterError::AmbisonicLoudness`] for an ambisonic topology;
    /// [`MeterError::UnplacedLoudnessChannels`] for speakers that are not a
    /// [`Bed`](tutti_buffers::Bed)'s, whose angles are unknown;
    /// [`MeterError::MeterUnavailable`] when `ebur128` cannot meter the rate.
    fn measure(&self, rate: SampleRate) -> Result<LoudnessMeter<S>, MeterError> {
        let labels = channel_labels(&self.topology)?;
        let layout = self.topology.width();
        let mut meter = EbuR128::new(
            u32::from(layout.count()),
            rate.get().round() as u32,
            Mode::I | Mode::LRA | Mode::TRUE_PEAK,
        )
        .map_err(|_| MeterError::MeterUnavailable)?;
        meter
            .set_channel_map(&labels)
            .map_err(|_| MeterError::MeterUnavailable)?;
        Ok(LoudnessMeter {
            layout,
            meter,
            sample: PhantomData,
        })
    }
}

/// The label of every channel of `topology`, in channel order: the one `ebur128` weighs as
/// [`channel_weights`] does (the surround labels carry 1.41, the rest 1.0, the LFE is unused).
fn channel_labels(topology: &ChannelTopology) -> Result<Box<[Channel]>, MeterError> {
    let weights = channel_weights(topology)?;
    Ok(weights
        .as_slice()
        .iter()
        .map(|w| match w {
            ChannelWeight::Silent => Channel::Unused,
            ChannelWeight::Unity => Channel::Left,
            ChannelWeight::Surround => Channel::LeftSurround,
        })
        .collect())
}

/// An EBU R128 reading.
///
/// All three are decibel quantities, so all three are [`Db`] — LUFS and LU are
/// not given their own types. That is a deliberate call: an affine `Lufs`/`Lu`
/// pair would be two new types for three fields, and the one operation anyone
/// performs on them (`target - measured`, giving a gain) is exactly what
/// [`gain_to`](Self::gain_to) already names.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    /// Integrated (gated) loudness, in LUFS.
    pub lufs: Db,
    /// Maximum true peak, in dBTP, over every channel including the LFE: the
    /// LFE adds no loudness, but its peak still counts. The meter oversamples 4× below 96 kHz and
    /// 2× from 96 kHz to below 192 kHz, so it catches inter-sample peaks a
    /// plain sample peak misses; at 192 kHz and above it is the sample peak.
    pub true_peak: Db,
    /// Loudness range (LRA), in LU.
    pub range: Db,
}

impl Loudness {
    /// The gain that moves this reading to `target`, limited so the result's
    /// true peak does not exceed `ceiling`.
    ///
    /// This is the whole of "normalize", and it is deliberately a *value*
    /// rather than an in-place mutation: the caller multiplies, so the same
    /// reading can gate a decision, be logged, or be applied on a second pass
    /// over a signal that was never held in memory.
    pub fn gain_to(&self, target: Db, ceiling: Db) -> Db {
        let gain = target.get() - self.lufs.get();
        let projected = self.true_peak.get() + gain;
        // Pull back by exactly the overshoot, never push up: a signal already
        // under the ceiling keeps the loudness-derived gain.
        Db::new(gain - (projected - ceiling.get()).max(0.0))
    }
}

/// A running EBU R128 meter at one rate and topology.
///
/// Wraps `EbuR128`, which accumulates internally. Not for the audio thread: the
/// meter keeps the gating blocks of the whole run, so it grows with the audio
/// and allocates.
pub struct LoudnessMeter<S: Sample> {
    layout: ChannelLayout,
    meter: EbuR128,
    sample: PhantomData<fn(S)>,
}

// `EbuR128` is not `Debug`; report what the meter was built for instead.
impl<S: Sample> std::fmt::Debug for LoudnessMeter<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoudnessMeter")
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

impl<S: Sample> LoudnessMeter<S> {
    /// Forgets everything fed: the readings start over.
    pub fn reset(&mut self) {
        self.meter.reset();
    }
}

impl<S: Sample> StreamMeter for LoudnessMeter<S> {
    type Input<'a> = Interleaved<'a, S>;
    type Item = ();
    type Output = Loudness;

    /// Feeds one chunk of frames.
    ///
    /// The chunk arrives as an [`Interleaved`], so its width travels with it
    /// rather than beside it. An [`Interleaved`] is whole frames, so every
    /// chunk keeps each sample on its own channel, and chunks need not align to
    /// any block size. A push yields no item: the reading is of the whole run.
    ///
    /// # Errors
    ///
    /// [`MeterError::ChunkLayoutMismatch`] when `chunk` is not the topology's width:
    /// the meter's channel count is fixed, so a chunk of another width would be
    /// split into the wrong number of frames and read as a different signal.
    fn push(
        &mut self,
        chunk: Interleaved<'_, S>,
        _: &mut impl Extend<()>,
    ) -> Result<(), MeterError> {
        if chunk.layout() != self.layout {
            return Err(MeterError::ChunkLayoutMismatch {
                chunk: chunk.layout(),
                config: self.layout,
            });
        }
        let Self { meter, .. } = self;
        for_blocks(chunk.samples(), chunk.channels(), |block| {
            // The width was checked above, and the block is whole frames, so
            // the meter takes it; a metering miss must not fail a render.
            let _ = meter.add_frames_f64(block);
        });
        Ok(())
    }

    /// Reads the meter.
    ///
    /// Consumes it: R128's integrated loudness is a whole-signal answer, and
    /// letting a caller read it mid-stream and keep feeding invites treating a
    /// partial reading as final.
    fn finish(self) -> Loudness {
        let meter = self.meter;
        // `-70` LUFS is R128's absolute gate. The meter reports `-inf` (not an
        // error) when nothing passed it — silence, or simply an input shorter than
        // the 400 ms gating block, which a short render legitimately is. Clamping to
        // the gate keeps the reading finite, because an infinite loudness poisons
        // every gain derived from it into `NaN`.
        let lufs = meter.loudness_global().unwrap_or(f64::NEG_INFINITY);
        let lufs = if lufs.is_finite() { lufs } else { -70.0 };
        let range = meter.loudness_range().unwrap_or(0.0);
        let range = if range.is_finite() { range } else { 0.0 };

        // True peak is per-channel; the file's peak is the loudest of them.
        let channels = meter.channels();
        let peak_linear = (0..channels)
            .filter_map(|c| meter.true_peak(c).ok())
            .fold(0.0f64, f64::max);

        Loudness {
            lufs: Db::new(lufs),
            true_peak: true_peak_db(peak_linear),
            range: Db::new(range),
        }
    }
}

/// What a [`TruePeakMeter`] measures: the largest inter-sample peak over every
/// channel, for channels with no placement to weigh a loudness by.
///
/// It reads nothing but the peak, so it takes no [`ChannelTopology`], pays for
/// no integrated or range gating, and any channel layout will do: the width is
/// the audio's, so the config describes nothing and the meter is made from the
/// rate and the width it measures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TruePeakConfig;

impl<S: Sample> Measure<S> for TruePeakConfig {
    type Context = (SampleRate, ChannelLayout);
    type Meter = TruePeakMeter<S>;

    /// A meter for frames `layout` wide at `rate`.
    ///
    /// # Errors
    ///
    /// [`MeterError::MeterUnavailable`] when `ebur128` cannot meter the width or the
    /// rate.
    fn measure(
        &self,
        (rate, layout): (SampleRate, ChannelLayout),
    ) -> Result<TruePeakMeter<S>, MeterError> {
        EbuR128::new(
            u32::from(layout.count()),
            rate.get().round() as u32,
            Mode::TRUE_PEAK,
        )
        .map_err(|_| MeterError::MeterUnavailable)
        .map(|meter| TruePeakMeter {
            layout,
            meter,
            scratch: vec![0.0; BLOCK.max(usize::from(layout.count()))],
            sample: PhantomData,
        })
    }
}

/// A running true-peak meter: the largest inter-sample peak over every
/// channel, made by a [`TruePeakConfig`].
///
/// Not for the audio thread: the meter's own buffers allocate as it grows. A
/// push widens the chunk to the meter's `f64` in blocks of a scratch built with
/// the meter, and allocates nothing itself.
pub struct TruePeakMeter<S: Sample> {
    layout: ChannelLayout,
    meter: EbuR128,
    /// One block of a chunk widened to the meter's `f64`.
    scratch: Vec<f64>,
    sample: PhantomData<fn(S)>,
}

// `EbuR128` is not `Debug`.
impl<S: Sample> std::fmt::Debug for TruePeakMeter<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TruePeakMeter")
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

impl<S: Sample> TruePeakMeter<S> {
    /// Forgets everything fed: the peak reads the floor again.
    pub fn reset(&mut self) {
        self.meter.reset();
    }
}

impl<S: Sample> StreamMeter for TruePeakMeter<S> {
    type Input<'a> = Interleaved<'a, S>;
    type Item = ();
    type Output = Db;

    /// Feeds one chunk of frames. A push yields no item: the peak is of the
    /// whole run.
    ///
    /// # Errors
    ///
    /// [`MeterError::ChunkLayoutMismatch`] when `chunk` is not the meter's width.
    fn push(
        &mut self,
        chunk: Interleaved<'_, S>,
        _: &mut impl Extend<()>,
    ) -> Result<(), MeterError> {
        if chunk.layout() != self.layout {
            return Err(MeterError::ChunkLayoutMismatch {
                chunk: chunk.layout(),
                config: self.layout,
            });
        }
        let run = run_len(self.scratch.len(), chunk.channels());
        for part in chunk.samples().chunks(run) {
            let widened = &mut self.scratch[..part.len()];
            for (slot, s) in widened.iter_mut().zip(part) {
                *slot = s.convert_to::<f64>();
            }
            let _ = self.meter.add_frames_f64(widened);
        }
        Ok(())
    }

    /// The largest true peak of every channel fed, in dBTP; [`Db::FLOOR`] for
    /// silence. Consumes the meter, as [`LoudnessMeter`]'s finish does.
    fn finish(self) -> Db {
        let peak = (0..u32::from(self.layout.count()))
            .filter_map(|c| self.meter.true_peak(c).ok())
            .fold(0.0f64, f64::max);
        true_peak_db(peak)
    }
}

/// The true peak of a whole buffer at `rate`, in dBTP: a [`TruePeakMeter`] fed
/// the buffer, then finished. The width is the buffer's.
///
/// # Errors
///
/// [`MeterError::MeterUnavailable`] when `ebur128` cannot meter the width or the
/// rate.
pub fn measure_true_peak<S: Sample>(
    buffer: Interleaved<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Db, MeterError> {
    measure_buffer::<S, _>(&TruePeakConfig, (rate.into(), buffer.layout()), buffer)
}

/// A true-peak amplitude as dBTP, pinning silence at [`Db::FLOOR`].
///
/// [`Db::from_amplitude`] leaves silence at `-inf` because the floor belongs to
/// the consumer; this is that consumer. An infinite peak would reach
/// [`Loudness::gain_to`], which subtracts it into a `NaN` — the same hazard the
/// `-70` LUFS gate guards against.
#[inline]
fn true_peak_db(peak_linear: f64) -> Db {
    if peak_linear <= 0.0 {
        Db::FLOOR
    } else {
        Db::from_amplitude(Amplitude::new(peak_linear))
    }
}

/// The rates a windowed meter is built for. The window it keeps is three
/// seconds of every channel, so the upper bound caps its allocation (a topology
/// is at most 16 channels).
const WINDOWED_MIN_RATE: SampleRate = SampleRate::of(8_000.0);
const WINDOWED_MAX_RATE: SampleRate = SampleRate::of(192_000.0);

/// What a [`WindowedMeter`] measures: the channel topology of the frames fed to
/// it, weighted by placement as [`LoudnessConfig`] is.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowedLoudnessConfig {
    config: LoudnessConfig,
}

impl WindowedLoudnessConfig {
    /// A config for frames in `topology`.
    #[must_use]
    pub fn new(topology: ChannelTopology) -> Self {
        Self {
            config: LoudnessConfig::new(topology),
        }
    }

    /// The topology of the fed frames.
    #[inline]
    #[must_use]
    pub fn topology(&self) -> &ChannelTopology {
        self.config.topology()
    }
}

impl<S: Sample> Measure<S> for WindowedLoudnessConfig {
    type Context = SampleRate;
    type Meter = WindowedMeter<S>;

    /// A meter for frames at `rate`. Allocates its three-second window here: at
    /// most 3 s of `f64` samples of every channel, 192 kHz x 16 channels x 3 s x
    /// 8 bytes, about 74 MB, and far less at ordinary rates and widths.
    ///
    /// # Errors
    ///
    /// [`MeterError::SampleRateOutOfRange`] for a rate outside 8 kHz..=192 kHz;
    /// [`MeterError::AmbisonicLoudness`] or [`MeterError::UnplacedLoudnessChannels`] for
    /// a topology with no placement to weigh; [`MeterError::MeterUnavailable`] if the
    /// meter cannot be built.
    fn measure(&self, rate: SampleRate) -> Result<WindowedMeter<S>, MeterError> {
        if !(WINDOWED_MIN_RATE..=WINDOWED_MAX_RATE).contains(&rate) {
            return Err(MeterError::SampleRateOutOfRange {
                rate,
                min: WINDOWED_MIN_RATE,
                max: WINDOWED_MAX_RATE,
            });
        }
        let labels = channel_labels(self.config.topology())?;
        let layout = self.config.topology().width();
        let mut meter = EbuR128::new(
            u32::from(layout.count()),
            rate.get().round() as u32,
            Mode::M | Mode::S | Mode::TRUE_PEAK,
        )
        .map_err(|_| MeterError::MeterUnavailable)?;
        meter
            .set_channel_map(&labels)
            .map_err(|_| MeterError::MeterUnavailable)?;
        Ok(WindowedMeter {
            layout,
            rate,
            meter,
            frames: 0,
            sample: PhantomData,
        })
    }
}

/// A running meter's reading.
///
/// All three are decibel readings, so all three are [`Db`] (see
/// [`Loudness`] for why LUFS has no type of its own).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowedReading {
    /// Loudness of the last 400 ms, in LUFS. [`LOUDNESS_FLOOR`](Self::LOUDNESS_FLOOR)
    /// until 400 ms have been fed.
    pub momentary: Db,
    /// Loudness of the last 3 s, in LUFS. [`LOUDNESS_FLOOR`](Self::LOUDNESS_FLOOR)
    /// until 3 s have been fed.
    pub short_term: Db,
    /// The largest true peak since the meter was built or reset, in dBTP.
    /// [`Db::FLOOR`] for silence.
    pub true_peak: Db,
    /// The frame the reading ends at: the frames fed since the meter was
    /// built or reset, in [`Samples`] of the stream.
    pub frame: Samples,
}

impl WindowedReading {
    /// The lowest loudness a reading reports: a clamp at BS.1770's `-70` LUFS
    /// absolute gate level, the same one [`LoudnessMeter::finish`] applies.
    ///
    /// Silence, a signal quieter than this and a window that is not yet full
    /// all read it, and only
    /// [`frames_seen`](WindowedMeter::frames_seen) tells them apart.
    /// A finite floor keeps every gain derived from a reading finite.
    pub const LOUDNESS_FLOOR: Db = Db::new(-70.0);
}

/// A running windowed meter at one rate and topology.
///
/// Each push yields the [`WindowedReading`] after it, and
/// [`finish`](StreamMeter::finish) reads the last one. Not for the audio thread (see
/// [`push`](StreamMeter::push)).
pub struct WindowedMeter<S: Sample> {
    layout: ChannelLayout,
    rate: SampleRate,
    meter: EbuR128,
    /// Whole frames fed since construction or the last reset.
    frames: u64,
    sample: PhantomData<fn(S)>,
}

// `EbuR128` is not `Debug`.
impl<S: Sample> std::fmt::Debug for WindowedMeter<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowedMeter")
            .field("layout", &self.layout)
            .field("rate", &self.rate)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl<S: Sample> WindowedMeter<S> {
    /// Forgets everything fed: the windows read the floor again and the peak
    /// is cleared. Keeps the buffers.
    pub fn reset(&mut self) {
        self.meter.reset();
        self.frames = 0;
    }

    /// Whole frames fed since construction or the last reset.
    #[inline]
    #[must_use]
    pub fn frames_seen(&self) -> u64 {
        self.frames
    }

    /// Frames in `tenths` hundred-millisecond units, as the meter counts them.
    fn window_frames(&self, tenths: u64) -> u64 {
        ((self.rate.get().round() as u64 + 5) / 10) * tenths
    }

    /// The reading of everything fed.
    fn reading(&self) -> WindowedReading {
        let peak = (0..u32::from(self.layout.count()))
            .filter_map(|c| self.meter.true_peak(c).ok())
            .fold(0.0f64, f64::max);
        WindowedReading {
            momentary: windowed_db(
                self.frames,
                self.window_frames(4),
                self.meter.loudness_momentary().ok(),
            ),
            short_term: windowed_db(
                self.frames,
                self.window_frames(30),
                self.meter.loudness_shortterm().ok(),
            ),
            true_peak: true_peak_db(peak),
            frame: Samples::new(self.frames as usize),
        }
    }
}

impl<S: Sample> StreamMeter for WindowedMeter<S> {
    type Input<'a> = Interleaved<'a, S>;
    type Item = WindowedReading;
    type Output = WindowedReading;

    /// Feeds one chunk and extends `out` with the reading after it: exactly one
    /// item per accepted push.
    ///
    /// A chunk is whole frames, so the reading does not depend on how the
    /// stream is cut. A chunk may be empty.
    ///
    /// A push allocates nothing once the meter is built (the meter's
    /// `add_frames_f64` and its momentary, short-term and true-peak getters,
    /// counted in `tests/alloc/windowed_loudness.rs` for the locked `ebur128`),
    /// but it is not a realtime-safe call: each reads both windows over their
    /// whole length, three seconds of every channel for the short-term one, so
    /// its cost does not shrink with the chunk. Run it on a consumer thread
    /// that drains a graph tap's `TapReader`, once per display tick, not in an
    /// audio callback.
    ///
    /// # Errors
    ///
    /// [`MeterError::ChunkLayoutMismatch`] when `chunk` is not the topology's width;
    /// [`MeterError::NonFiniteInput`] for a NaN or infinite sample;
    /// [`MeterError::MeterRejectedFrames`] if the meter refuses the frames. The
    /// meter is untouched on the first two.
    fn push(
        &mut self,
        chunk: Interleaved<'_, S>,
        out: &mut impl Extend<WindowedReading>,
    ) -> Result<(), MeterError> {
        if chunk.layout() != self.layout {
            return Err(MeterError::ChunkLayoutMismatch {
                chunk: chunk.layout(),
                config: self.layout,
            });
        }
        if !all_finite(chunk.samples()) {
            return Err(MeterError::NonFiniteInput);
        }
        let channels = chunk.channels();
        let mut rejected = false;
        for_blocks(chunk.samples(), channels, |block| {
            if self.meter.add_frames_f64(block).is_err() {
                rejected = true;
            } else {
                self.frames += (block.len() / channels) as u64;
            }
        });
        if rejected {
            return Err(MeterError::MeterRejectedFrames);
        }
        out.extend([self.reading()]);
        Ok(())
    }

    /// The reading of everything fed.
    fn finish(self) -> WindowedReading {
        self.reading()
    }
}

/// A window's loudness: the floor until `needed` frames have been fed, and for
/// a reading the meter could not give or that is not finite (silence is `-inf`);
/// otherwise the reading, not under the floor.
fn windowed_db(frames: u64, needed: u64, lufs: Option<f64>) -> Db {
    match lufs {
        Some(lufs) if frames >= needed && lufs.is_finite() => {
            Db::new(lufs.max(WindowedReading::LOUDNESS_FLOOR.get()))
        }
        _ => WindowedReading::LOUDNESS_FLOOR,
    }
}

/// Measures a whole buffer at `rate`.
///
/// Feeds one [`LoudnessMeter`] the buffer, so the whole-buffer and streaming
/// paths can never disagree.
///
/// # Errors
///
/// As [`LoudnessConfig::measure`], and [`MeterError::ChunkLayoutMismatch`] when
/// `buffer` is not the topology's width.
pub fn measure_loudness<S: Sample>(
    cfg: &LoudnessConfig,
    buffer: Interleaved<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Loudness, MeterError> {
    measure_buffer::<S, _>(cfg, rate.into(), buffer)
}

#[cfg(test)]
mod tests;
