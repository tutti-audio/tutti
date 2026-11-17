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
//! are both decibel readings, and the f32 that `Db` carries is ~3× finer than a
//! 24-bit LSB at any level these reach, so nothing measurable is lost by not
//! keeping them `f64`.
//!
//! # Weighted by placement
//!
//! A config takes a [`ChannelTopology`], and each speaker's weight is read from
//! where it stands (ITU-R BS.1770-5, Annex 3, Table 4): 1.41 when its elevation
//! is under 30 degrees and its azimuth is within 60 to 120 degrees either side,
//! else 1.0; the LFE adds no loudness (its peak still counts). The angles are the bed's own
//! ([`Bed::direction`](tutti_types::Bed::direction)), so a 5.1's side speakers (M+110 and M-110) weigh 1.41 and a
//! 7.1's rear speakers (M+135 and M-135) and every upper speaker weigh 1.0, as Table 5
//! lists them. A quad's side speakers (VST3's FL FR SL SR) stand for the bed's
//! M+-135 backs and weigh 1.0. `ebur128` has no free weights, so each speaker is given the
//! channel label of its weight. A topology without positions (an unplaced set of
//! speakers) and an ambisonic one are refused: neither has an angle to weigh.

use core::marker::PhantomData;

use ebur128::{Channel, EbuR128, Mode};
use tutti_buffers::{Interleaved, Sample};
use tutti_types::{ChannelLayout, ChannelTopology, Db, Direction, SampleRate, Samples};

use crate::convert::{BLOCK, Discard, all_finite, for_blocks, run_len};
use crate::{Error, Measure, Meter, Result};

/// What a loudness meter measures: the channel topology of the frames fed to
/// it. Unlike a width, a topology is part of the analysis: it is the channel
/// model each speaker's weight is read from. [`measure_loudness`] checks the
/// buffer's width against it ([`Error::ChunkLayoutMismatch`]).
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
    /// a different width is refused ([`Error::ChunkLayoutMismatch`]).
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
    /// [`Error::AmbisonicLoudness`] for an ambisonic topology;
    /// [`Error::UnplacedLoudnessChannels`] for speakers that are not a
    /// [`Bed`](tutti_types::Bed)'s, whose angles are unknown;
    /// [`Error::MeterUnavailable`] when `ebur128` cannot meter the rate.
    fn measure(&self, rate: SampleRate) -> Result<LoudnessMeter<S>> {
        let labels = channel_labels(&self.topology)?;
        let layout = self.topology.width();
        let mut meter = EbuR128::new(
            u32::from(layout.count()),
            rate.get().round() as u32,
            Mode::I | Mode::LRA | Mode::TRUE_PEAK,
        )
        .map_err(|_| Error::MeterUnavailable)?;
        meter
            .set_channel_map(&labels)
            .map_err(|_| Error::MeterUnavailable)?;
        Ok(LoudnessMeter {
            layout,
            meter,
            sample: PhantomData,
        })
    }
}

/// BS.1770-5 Table 4's weight is 1.41 for a speaker in the horizontal band
/// (elevation within 30 degrees, exclusive) and 60 to 120 degrees to either
/// side, and 1.0 elsewhere. The label is one `ebur128` weighs the same: the
/// surround labels carry 1.41 and the rest 1.0.
fn label_at(direction: Direction) -> Channel {
    let azimuth = direction.azimuth.get();
    let beside = direction.elevation.get().abs() < 30.0 && (60.0..=120.0).contains(&azimuth.abs());
    match (beside, azimuth) {
        (true, a) if a > 0.0 => Channel::LeftSurround,
        (true, _) => Channel::RightSurround,
        (false, a) if a > 0.0 => Channel::Left,
        (false, a) if a < 0.0 => Channel::Right,
        (false, _) => Channel::Center,
    }
}

/// The label of every channel of `topology`, in channel order: the LFE unused,
/// every other speaker by [`label_at`] its bed's direction.
fn channel_labels(topology: &ChannelTopology) -> Result<Box<[Channel]>> {
    let speakers = topology.positions().ok_or(Error::AmbisonicLoudness)?;
    let bed = topology.bed().ok_or(Error::UnplacedLoudnessChannels {
        width: topology.width(),
    })?;
    speakers
        .iter()
        .map(|&speaker| {
            if speaker.is_lfe() {
                return Ok(Channel::Unused);
            }
            // A side/back counterpart stands for the bed's own spelling of it
            // (`Bed::from_topology`).
            let direction = bed
                .direction(speaker)
                .or_else(|| speaker.counterpart().and_then(|c| bed.direction(c)))
                .ok_or(Error::UnplacedLoudnessChannels {
                    width: topology.width(),
                })?;
            Ok(label_at(direction))
        })
        .collect()
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
        Db(gain - (projected - ceiling.get()).max(0.0))
    }
}

/// A running EBU R128 meter at one rate and topology.
///
/// Wraps `EbuR128`, which accumulates internally. Not for the audio thread: the
/// meter keeps the gating blocks of the whole run, so it grows with the audio
/// and allocates.
pub struct LoudnessMeter<S: Sample = f32> {
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

impl<S: Sample> Meter for LoudnessMeter<S> {
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
    /// [`Error::ChunkLayoutMismatch`] when `chunk` is not the topology's width:
    /// the meter's channel count is fixed, so a chunk of another width would be
    /// split into the wrong number of frames and read as a different signal.
    fn push(&mut self, chunk: Interleaved<'_, S>, _: &mut impl Extend<()>) -> Result<()> {
        if chunk.layout() != self.layout {
            return Err(Error::ChunkLayoutMismatch {
                chunk: chunk.layout(),
                config: self.layout,
            });
        }
        let Self { meter, .. } = self;
        for_blocks(chunk.samples(), chunk.channels(), |block| {
            // The width was checked above, and the block is whole frames, so
            // the meter takes it; a metering miss must not fail a render.
            let _ = meter.add_frames_f32(block);
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
            lufs: Db(lufs as f32),
            true_peak: true_peak_db(peak_linear),
            range: Db(range as f32),
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
    /// [`Error::MeterUnavailable`] when `ebur128` cannot meter the width or the
    /// rate.
    fn measure(&self, (rate, layout): (SampleRate, ChannelLayout)) -> Result<TruePeakMeter<S>> {
        EbuR128::new(
            u32::from(layout.count()),
            rate.get().round() as u32,
            Mode::TRUE_PEAK,
        )
        .map_err(|_| Error::MeterUnavailable)
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
pub struct TruePeakMeter<S: Sample = f32> {
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

impl<S: Sample> Meter for TruePeakMeter<S> {
    type Input<'a> = Interleaved<'a, S>;
    type Item = ();
    type Output = Db;

    /// Feeds one chunk of frames. A push yields no item: the peak is of the
    /// whole run.
    ///
    /// # Errors
    ///
    /// [`Error::ChunkLayoutMismatch`] when `chunk` is not the meter's width.
    fn push(&mut self, chunk: Interleaved<'_, S>, _: &mut impl Extend<()>) -> Result<()> {
        if chunk.layout() != self.layout {
            return Err(Error::ChunkLayoutMismatch {
                chunk: chunk.layout(),
                config: self.layout,
            });
        }
        let run = run_len(self.scratch.len(), chunk.channels());
        for part in chunk.samples().chunks(run) {
            let widened = &mut self.scratch[..part.len()];
            for (slot, s) in widened.iter_mut().zip(part) {
                *slot = s.to_f64();
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
/// [`Error::MeterUnavailable`] when `ebur128` cannot meter the width or the
/// rate.
pub fn measure_true_peak<S: Sample>(
    buffer: Interleaved<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Db> {
    let mut meter = TruePeakConfig.measure((rate.into(), buffer.layout()))?;
    meter.push(buffer, &mut Discard)?;
    Ok(meter.finish())
}

/// An `f64` true-peak amplitude as dBTP, pinning silence at [`Db::FLOOR`].
///
/// [`Db::from_amplitude`] leaves silence at `-inf` because the floor belongs to
/// the consumer; this is that consumer. An infinite peak would reach
/// [`Loudness::gain_to`], which subtracts it into a `NaN` — the same hazard the
/// `-70` LUFS gate guards against.
///
/// Not routed through [`tutti_types::Amplitude`], which is `f32`: converting first narrows
/// the input before the `log10`, defeating the reason this path is `f64`. The
/// arithmetic is [`Db::from_amplitude`]'s, at the width this boundary holds.
#[inline]
fn true_peak_db(peak_linear: f64) -> Db {
    if peak_linear <= 0.0 {
        Db::FLOOR
    } else {
        Db((20.0 * libm::log10(peak_linear)) as f32)
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
    /// [`Error::SampleRateOutOfRange`] for a rate outside 8 kHz..=192 kHz;
    /// [`Error::AmbisonicLoudness`] or [`Error::UnplacedLoudnessChannels`] for
    /// a topology with no placement to weigh; [`Error::MeterUnavailable`] if the
    /// meter cannot be built.
    fn measure(&self, rate: SampleRate) -> Result<WindowedMeter<S>> {
        if !(WINDOWED_MIN_RATE..=WINDOWED_MAX_RATE).contains(&rate) {
            return Err(Error::SampleRateOutOfRange {
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
        .map_err(|_| Error::MeterUnavailable)?;
        meter
            .set_channel_map(&labels)
            .map_err(|_| Error::MeterUnavailable)?;
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
    pub const LOUDNESS_FLOOR: Db = Db(-70.0);
}

/// A running windowed meter at one rate and topology.
///
/// Each push yields the [`WindowedReading`] after it, and
/// [`finish`](Meter::finish) reads the last one. Not for the audio thread (see
/// [`push`](Meter::push)).
pub struct WindowedMeter<S: Sample = f32> {
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
            frame: Samples(self.frames as usize),
        }
    }
}

impl<S: Sample> Meter for WindowedMeter<S> {
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
    /// `add_frames_f32` and its momentary, short-term and true-peak getters,
    /// counted in `tests/alloc/windowed_loudness.rs` for the locked `ebur128`),
    /// but it is not a realtime-safe call: each reads both windows over their
    /// whole length, three seconds of every channel for the short-term one, so
    /// its cost does not shrink with the chunk. Run it on a consumer thread
    /// that drains a graph tap's `TapReader`, once per display tick, not in an
    /// audio callback.
    ///
    /// # Errors
    ///
    /// [`Error::ChunkLayoutMismatch`] when `chunk` is not the topology's width;
    /// [`Error::NonFiniteInput`] for a NaN or infinite sample;
    /// [`Error::MeterRejectedFrames`] if the meter refuses the frames. The
    /// meter is untouched on the first two.
    fn push(
        &mut self,
        chunk: Interleaved<'_, S>,
        out: &mut impl Extend<WindowedReading>,
    ) -> Result<()> {
        if chunk.layout() != self.layout {
            return Err(Error::ChunkLayoutMismatch {
                chunk: chunk.layout(),
                config: self.layout,
            });
        }
        if !all_finite(chunk.samples()) {
            return Err(Error::NonFiniteInput);
        }
        let channels = chunk.channels();
        let mut rejected = false;
        for_blocks(chunk.samples(), channels, |block| {
            if self.meter.add_frames_f32(block).is_err() {
                rejected = true;
            } else {
                self.frames += (block.len() / channels) as u64;
            }
        });
        if rejected {
            return Err(Error::MeterRejectedFrames);
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
            Db((lufs as f32).max(WindowedReading::LOUDNESS_FLOOR.get()))
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
/// As [`LoudnessConfig::measure`], and [`Error::ChunkLayoutMismatch`] when
/// `buffer` is not the topology's width.
pub fn measure_loudness<S: Sample>(
    cfg: &LoudnessConfig,
    buffer: Interleaved<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Loudness> {
    let mut meter = cfg.measure(rate.into())?;
    meter.push(buffer, &mut Discard)?;
    Ok(meter.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold_uneven;
    use std::f32::consts::TAU;
    use tutti_types::{
        AmbisonicFormat, AmbisonicOrder, Amplitude, Azimuth, Bed, Elevation, Speaker,
    };

    /// Pushes one chunk to a windowed meter and returns the reading it yields.
    fn read(state: &mut WindowedMeter, chunk: Interleaved<'_>) -> Result<WindowedReading> {
        let mut readings = Vec::new();
        state.push(chunk, &mut readings)?;
        assert_eq!(readings.len(), 1, "one reading per accepted push");
        Ok(readings[0])
    }

    /// A config with the rate its tests measure at.
    struct Rated<C>(C, SampleRate);

    /// `measure_loudness` at the rate the config was made for.
    fn measure_loudness<S: Sample>(
        c: &Rated<LoudnessConfig>,
        buffer: Interleaved<'_, S>,
    ) -> Result<Loudness> {
        super::measure_loudness(&c.0, buffer, c.1)
    }

    /// The true-peak floor lives at this boundary, not in `Db::from_amplitude`.
    #[test]
    fn true_peak_pins_silence_and_stays_wide() {
        // The shared conversion does not floor; this boundary does.
        assert!(Db::from_amplitude(Amplitude::SILENT).get().is_infinite());
        assert_eq!(true_peak_db(0.0), Db::FLOOR);
        assert!(true_peak_db(0.0).get().is_finite());

        // Agrees with the shared conversion everywhere above silence.
        let want = Db::from_amplitude(Amplitude(0.5));
        assert!((true_peak_db(0.5).get() - want.get()).abs() < 1e-5);
        assert!((true_peak_db(1.0).get() - 0.0).abs() < 1e-6);

        // Computed at f64 width: narrowing to `Amplitude` (f32) before the
        // log10 is the one thing this path exists to avoid.
        let tiny = 1.0e-30_f64;
        assert!(
            (true_peak_db(tiny).get() - -600.0).abs() < 1.0,
            "f64 input must survive the log10, got {:?}",
            true_peak_db(tiny)
        );
    }

    /// An infinite true peak poisons every gain derived from it — the same
    /// reason `lufs` is clamped to the `-70` gate.
    #[test]
    fn silent_render_yields_a_finite_normalization_gain() {
        let m = Loudness {
            lufs: Db(-70.0),
            true_peak: true_peak_db(0.0),
            range: Db(0.0),
        };
        assert!(m.gain_to(Db(-14.0), Db(-1.0)).get().is_finite());
    }

    /// Interleaved stereo sine at `amp`, `secs` long.
    fn sine(rate: f64, secs: f64, freq: f32, amp: f32) -> Vec<f32> {
        let n = (rate * secs) as usize;
        tutti_fixtures::signal::sine(
            tutti_types::Hz(freq),
            tutti_types::Amplitude(amp),
            SampleRate::of(rate),
            tutti_types::Samples(n),
        )
        .into_iter()
        .flat_map(|s| [s, s])
        .collect()
    }

    fn cfg(rate: f64) -> Rated<LoudnessConfig> {
        Rated(
            LoudnessConfig::new(Bed::Stereo.topology()),
            SampleRate::of(rate),
        )
    }

    /// The one-shot form must fold the streaming form exactly — the property
    /// `peaks.rs` holds, and the reason a batch/stream drift cannot appear.
    #[test]
    fn streaming_matches_one_shot() {
        let c = cfg(48_000.0);
        let buf = sine(48_000.0, 1.0, 1_000.0, 0.5);

        let one_shot =
            measure_loudness(&c, Interleaved::new(&buf, ChannelLayout::STEREO).unwrap()).unwrap();

        // 777 frames: not a multiple of any block size the meter uses.
        for frames in [1, 777, 4800, 48_000] {
            let mut meter = c.0.measure(c.1).unwrap();
            for chunk in buf.chunks(frames * 2) {
                meter
                    .push(
                        Interleaved::new(chunk, ChannelLayout::STEREO).unwrap(),
                        &mut (),
                    )
                    .unwrap();
            }
            assert_eq!(meter.finish(), one_shot, "chunks of {frames} frames");
        }
    }

    /// The rate is honoured, not assumed. A meter built for the wrong rate
    /// reads a different loudness for the same musical signal.
    #[test]
    fn the_configured_rate_is_used() {
        // Same *sample* data interpreted at two rates is a different signal
        // (different frequency, different duration), so the readings differ.
        let buf = sine(44_100.0, 2.0, 1_000.0, 0.5);
        let stereo = Interleaved::new(&buf, ChannelLayout::STEREO).unwrap();
        let at_44 = measure_loudness(&cfg(44_100.0), stereo).unwrap();
        let at_48 = measure_loudness(&cfg(48_000.0), stereo).unwrap();
        assert!(
            (at_44.lufs.get() - at_48.lufs.get()).abs() > 1e-4,
            "a meter that ignored its rate would report the same LUFS twice: \
             {at_44:?} vs {at_48:?}"
        );
    }

    /// A −6 dBFS 1 kHz sine reads about −6.7 LUFS. Derived from BS.1770's
    /// definition, not from this implementation:
    ///
    /// ```text
    /// LUFS = -0.691 + 10*log10( Σ_ch G_ch * mean_square_ch )
    /// mean square of a sine of amplitude a = a²/2 = 0.125
    /// stereo, both G = 1.0                  → Σ = 0.25
    /// -0.691 + 10*log10(0.25)               = -6.71 LUFS
    /// ```
    ///
    /// Note the stereo **sum**: two identical channels are ~3 dB louder than
    /// one, which is why this is not the −9.7 a single channel would read.
    /// The tolerance covers K-weighting's small lift at 1 kHz (the shelf is
    /// flat near 0 dB there, but not exactly).
    #[test]
    fn silence_floors_rather_than_erroring() {
        let silence = vec![0.0; 4800];
        let m = measure_loudness(
            &cfg(48_000.0),
            Interleaved::new(&silence, ChannelLayout::STEREO).unwrap(),
        )
        .unwrap();
        assert_eq!(m.true_peak, Db::FLOOR);
        assert!(m.lufs.get() <= -70.0, "gated silence, got {:?}", m.lufs);
    }

    /// Every reading must be finite, even when nothing passes R128's gate.
    ///
    /// `ebur128` returns `Ok(-inf)` rather than an error for an input shorter
    /// than the 400 ms gating block — which a short render legitimately is. An
    /// infinite LUFS silently poisons `gain_to` into `NaN`, and a `NaN` gain
    /// multiplies a whole render into `NaN`. Caught by an export test composing
    /// normalization over a 0.2 s buffer.
    #[test]
    fn a_reading_is_always_finite_even_below_the_gate() {
        // 100 ms — a quarter of the gating block.
        let short = sine(48_000.0, 0.1, 1_000.0, 0.5);
        let m = measure_loudness(
            &cfg(48_000.0),
            Interleaved::new(&short, ChannelLayout::STEREO).unwrap(),
        )
        .unwrap();
        assert!(
            m.lufs.get().is_finite(),
            "LUFS must be finite, got {:?}",
            m.lufs
        );
        assert!(m.range.get().is_finite());
        assert!(
            m.gain_to(Db(-14.0), Db(-1.0)).get().is_finite(),
            "a gain derived from a sub-gate reading must not be NaN"
        );
    }

    /// A chunk whose own width disagrees with the meter's is refused, not fed.
    ///
    /// The buffer carries its width, so the disagreement is detectable. Fed
    /// anyway, a quad chunk handed to a stereo meter would be split into twice
    /// as many frames of the wrong signal and silently folded into the
    /// integrated reading.
    #[test]
    fn a_chunk_of_the_wrong_width_is_refused() {
        let c = cfg(48_000.0);
        let buf = sine(48_000.0, 1.0, 1_000.0, 0.5);

        let mut meter = c.0.measure(c.1).unwrap();
        assert_eq!(
            meter.push(
                Interleaved::new(&buf, ChannelLayout::QUAD).unwrap(),
                &mut ()
            ),
            Err(Error::ChunkLayoutMismatch {
                chunk: ChannelLayout::QUAD,
                config: ChannelLayout::STEREO,
            })
        );
        let m = meter.finish();

        assert_eq!(
            m.true_peak,
            Db::FLOOR,
            "a mismatched chunk must not reach the meter, got {:?}",
            m.true_peak
        );
    }

    /// The gain is the plain difference when the ceiling is not in play.
    #[test]
    fn gain_to_targets_the_requested_loudness() {
        let m = Loudness {
            lufs: Db(-20.0),
            true_peak: Db(-12.0),
            range: Db(0.0),
        };
        // −20 → −14 is +6 dB; the peak lands at −6, under a −1 ceiling.
        assert!((m.gain_to(Db(-14.0), Db(-1.0)).get() - 6.0).abs() < 1e-5);
    }

    /// …and is pulled back by exactly the overshoot when it is.
    #[test]
    fn gain_to_respects_the_true_peak_ceiling() {
        let m = Loudness {
            lufs: Db(-20.0),
            true_peak: Db(-2.0),
            range: Db(0.0),
        };
        // +6 dB would put the peak at +4, which is 5 dB over a −1 ceiling.
        assert!((m.gain_to(Db(-14.0), Db(-1.0)).get() - 1.0).abs() < 1e-5);
    }

    /// A signal already under the ceiling must not be pushed *up* to meet it.
    #[test]
    fn gain_to_never_amplifies_to_reach_the_ceiling() {
        let m = Loudness {
            lufs: Db(-20.0),
            true_peak: Db(-40.0),
            range: Db(0.0),
        };
        assert!((m.gain_to(Db(-20.0), Db(-1.0)).get()).abs() < 1e-5);
    }

    /// Read-out tolerance for a steady tone, in LU: EBU Tech 3341's `±0.1 LU`
    /// for a conforming meter. The truncation of a 997 Hz tone to a window that
    /// is not a whole number of cycles adds at most
    /// `10 log10(1 + 1/(4 pi f T))` = 0.001 dB for the 400 ms window, so the
    /// published figure is the whole budget.
    const WINDOW_TOL: f32 = 0.1;

    /// Stereo (identical channels) 997 Hz sine, amplitude `amp(t)` at `t`
    /// seconds, with a continuous phase.
    fn windowed_sine(rate: f64, secs: f64, amp: impl Fn(f64) -> f64) -> Vec<f32> {
        let n = (rate * secs).round() as usize;
        (0..n)
            .flat_map(|i| {
                let t = i as f64 / rate;
                let s = (amp(t) * (std::f64::consts::TAU * 997.0 * t).sin()) as f32;
                [s, s]
            })
            .collect()
    }

    /// The reading for identical channels of amplitude `a`: BS.1770 gives
    /// `-0.691 + 10 log10(sum_ch G * a^2 / 2)`, and the K-weighting's gain at
    /// 997 Hz is the `+0.691` dB that offset cancels, so two channels read
    /// `10 log10(a^2) = 20 log10(a)`.
    fn stereo_lufs(a: f64) -> f32 {
        Db::from_amplitude(Amplitude::new(a as f32)).get()
    }

    fn windowed_cfg(rate: f64) -> Rated<WindowedLoudnessConfig> {
        Rated(
            WindowedLoudnessConfig::new(Bed::Stereo.topology()),
            SampleRate::of(rate),
        )
    }

    /// Feeds `buf` in `frames`-frame chunks, returning the reading after each.
    fn windowed_run(
        cfg: &Rated<WindowedLoudnessConfig>,
        buf: &[f32],
        frames: usize,
    ) -> Vec<WindowedReading> {
        let mut state = cfg.0.measure(cfg.1).unwrap();
        buf.chunks(frames * 2)
            .map(|chunk| {
                read(
                    &mut state,
                    Interleaved::new(chunk, ChannelLayout::STEREO).unwrap(),
                )
                .unwrap()
            })
            .collect()
    }

    /// A -20 dBFS stereo 997 Hz sine reads -20.0 LUFS in both windows, and
    /// each reads the floor until exactly its own window of frames is in.
    #[test]
    fn a_sine_reads_its_loudness_once_each_window_fills() {
        let want = stereo_lufs(0.1);
        assert!((want - -20.0).abs() < 1e-5);
        for rate in [44_100.0, 48_000.0] {
            let cfg = windowed_cfg(rate);
            let block = (rate / 100.0) as usize; // 10 ms
            let tone = windowed_sine(rate, 3.5, |_| 0.1);
            let r = windowed_run(&cfg, &tone, block);
            let at = |ms: usize| r[ms / 10 - 1];

            assert_eq!(at(390).momentary, WindowedReading::LOUDNESS_FLOOR, "{rate}");
            assert!(
                (at(400).momentary.get() - want).abs() < WINDOW_TOL,
                "{rate}: momentary {:?}",
                at(400).momentary
            );
            assert_eq!(
                at(2990).short_term,
                WindowedReading::LOUDNESS_FLOOR,
                "{rate}"
            );
            assert!(
                (at(3000).short_term.get() - want).abs() < WINDOW_TOL,
                "{rate}: short-term {:?}",
                at(3000).short_term
            );
            // The peak of an amplitude-0.1 tone is 20 log10(0.1) dBTP, a figure of
            // its own; the 4x oversampled meter adds under 0.3 dB, as in
            // `true_peak_is_the_signals_amplitude`.
            let peak_want = Db::from_amplitude(Amplitude::new(0.1)).get();
            assert!((at(3500).true_peak.get() - peak_want).abs() < 0.3);
        }
    }

    /// A 10 dB step (10x the power) moves momentary loudness along
    /// `10 log10(1 + 9 min(t/0.4, 1))` and short-term along the same with
    /// `t/3`: the windows are linear in power, so the share of the window past
    /// the step is `t / window`. Each is settled at its window and not before.
    #[test]
    fn a_ten_db_step_settles_after_each_window() {
        let rate = 48_000.0;
        let cfg = windowed_cfg(rate);
        let (before, step_at) = (0.05, 4.0);
        let after = before * 10f64.sqrt();
        let signal = windowed_sine(rate, step_at + 4.0, |t| {
            if t < step_at { before } else { after }
        });
        let r = windowed_run(&cfg, &signal, 480);
        let at = |t: f64| r[((step_at + t) * 100.0).round() as usize - 1];

        let (low, high) = (stereo_lufs(before), stereo_lufs(after));
        assert!((high - low - 10.0).abs() < 1e-4);
        assert!((at(0.0).momentary.get() - low).abs() < WINDOW_TOL);
        assert!((at(0.0).short_term.get() - low).abs() < WINDOW_TOL);

        let ramp = |t: f64, window: f64| {
            f64::from(low) + 10.0 * (1.0 + 9.0 * (t / window).min(1.0)).log10()
        };
        for t in [0.1, 0.2, 0.3, 0.4, 0.5, 1.0] {
            let got = at(t).momentary.get();
            assert!(
                (got - ramp(t, 0.4) as f32).abs() < WINDOW_TOL,
                "momentary +{t}s: {got}"
            );
        }
        for t in [0.5, 1.0, 2.0, 3.0, 3.5, 4.0] {
            let got = at(t).short_term.get();
            assert!(
                (got - ramp(t, 3.0) as f32).abs() < WINDOW_TOL,
                "short-term +{t}s: {got}"
            );
        }
        // Settled at the window, and visibly not before it.
        assert!((at(0.4).momentary.get() - high).abs() < WINDOW_TOL);
        assert!(at(0.3).momentary.get() < high - 1.0);
        assert!((at(3.0).short_term.get() - high).abs() < WINDOW_TOL);
        assert!(at(2.0).short_term.get() < high - 1.0);
    }

    /// Silence reads the floor in both windows and a floor peak; so does a
    /// window that signal has left, once the K-weighting's ring-out is under
    /// the floor.
    #[test]
    fn silence_reads_the_floor() {
        let cfg = windowed_cfg(48_000.0);
        let silence = vec![0.0f32; 48_000 * 2 * 4];
        let last = *windowed_run(&cfg, &silence, 4800).last().unwrap();
        assert_eq!(last.momentary, WindowedReading::LOUDNESS_FLOOR);
        assert_eq!(last.short_term, WindowedReading::LOUDNESS_FLOOR);
        assert_eq!(last.true_peak, Db::FLOOR);

        let mut left_behind = windowed_sine(48_000.0, 4.0, |_| 0.1);
        left_behind.extend(vec![0.0f32; 48_000 * 2]);
        let r = windowed_run(&cfg, &left_behind, 480);
        let last = r.last().unwrap();
        assert_eq!(last.momentary, WindowedReading::LOUDNESS_FLOOR);
        assert!(last.short_term.get() > -30.0, "{:?}", last.short_term);
        assert!(last.true_peak.get() > -21.0, "the peak is held");
    }

    /// The readings do not depend on how the stream is cut: 64-frame,
    /// 4096-frame and 777-frame chunks agree bit for bit wherever they have
    /// fed the same frames.
    #[test]
    fn readings_do_not_depend_on_the_chunking() {
        let cfg = windowed_cfg(48_000.0);
        // A level change and a quiet stretch, so the windows are not trivial.
        let signal = windowed_sine(48_000.0, 5.0, |t| {
            if t < 2.0 {
                0.2
            } else if t < 3.0 {
                0.01
            } else {
                0.4
            }
        });
        let fine = windowed_run(&cfg, &signal, 64);
        let coarse = windowed_run(&cfg, &signal, 4096);
        let total = signal.len() / 2;
        for (i, c) in coarse.iter().enumerate() {
            let frames = ((i + 1) * 4096).min(total);
            if frames % 64 == 0 {
                assert_eq!(*c, fine[frames / 64 - 1], "after {frames} frames");
            }
        }
        assert_eq!(coarse.last(), fine.last());

        let mut state = cfg.0.measure(cfg.1).unwrap();
        let mut odd = None;
        for chunk in signal.chunks(777 * 2) {
            odd = Some(
                read(
                    &mut state,
                    Interleaved::new(chunk, ChannelLayout::STEREO).unwrap(),
                )
                .unwrap(),
            );
        }
        assert_eq!(odd.as_ref(), fine.last());
    }

    /// `reset` returns the meter to its first state.
    #[test]
    fn reset_reads_like_a_fresh_meter() {
        let cfg = windowed_cfg(48_000.0);
        let tone = windowed_sine(48_000.0, 4.0, |_| 0.1);
        let mut state = cfg.0.measure(cfg.1).unwrap();
        read(
            &mut state,
            Interleaved::new(&tone, ChannelLayout::STEREO).unwrap(),
        )
        .unwrap();
        assert!(state.frames_seen() > 0);
        state.reset();
        assert_eq!(state.frames_seen(), 0);
        let quiet = vec![0.0f32; 48_000 * 2];
        let r = read(
            &mut state,
            Interleaved::new(&quiet, ChannelLayout::STEREO).unwrap(),
        )
        .unwrap();
        assert_eq!(r.momentary, WindowedReading::LOUDNESS_FLOOR);
        assert_eq!(r.short_term, WindowedReading::LOUDNESS_FLOOR);
        assert_eq!(r.true_peak, Db::FLOOR);
    }

    /// The true peak is the loudest channel's, wherever it is: a tone of
    /// amplitude 0.5 on the right alone peaks at `20 log10(0.5)` dBTP (4x
    /// oversampling adds under 0.3 dB), and on the left alone just the same.
    #[test]
    fn the_true_peak_is_the_loudest_channels() {
        let cfg = windowed_cfg(48_000.0);
        let want = Db::from_amplitude(Amplitude::new(0.5)).get();
        for (left, right) in [(0.0f64, 0.5f64), (0.5, 0.0), (0.1, 0.5)] {
            let signal: Vec<f32> = (0..24_000)
                .flat_map(|i| {
                    let s = (TAU as f64 * 997.0 * i as f64 / 48_000.0).sin();
                    [(left * s) as f32, (right * s) as f32]
                })
                .collect();
            let mut state = cfg.0.measure(cfg.1).unwrap();
            let r = read(
                &mut state,
                Interleaved::new(&signal, ChannelLayout::STEREO).unwrap(),
            )
            .unwrap();
            assert!(
                (r.true_peak.get() - want).abs() < 0.3,
                "L {left} R {right}: {:?}",
                r.true_peak
            );
        }
    }

    /// A bad chunk is refused without touching the meter.
    #[test]
    fn bad_settings_and_input_are_errors() {
        let cfg = windowed_cfg(48_000.0);
        let mut state = cfg.0.measure(cfg.1).unwrap();
        let tone = windowed_sine(48_000.0, 0.5, |_| 0.1);
        let fed = |state: &mut WindowedMeter, buf: &[f32]| {
            read(state, Interleaved::new(buf, ChannelLayout::STEREO).unwrap())
        };
        fed(&mut state, &tone).unwrap();
        let frames = state.frames_seen();

        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut chunk = tone.clone();
            chunk[7] = bad;
            assert_eq!(fed(&mut state, &chunk), Err(Error::NonFiniteInput));
            assert_eq!(state.frames_seen(), frames, "a refused chunk feeds nothing");
        }
        assert!(matches!(
            read(
                &mut state,
                Interleaved::new(&tone, ChannelLayout::QUAD).unwrap()
            ),
            Err(Error::ChunkLayoutMismatch { .. })
        ));
        assert_eq!(state.frames_seen(), frames);

        // A rate outside what a windowed meter is built for is refused where
        // the rate binds.
        for rate in [7_999.0, 192_001.0] {
            assert!(matches!(
                Measure::<f32>::measure(&cfg.0, SampleRate::of(rate)),
                Err(Error::SampleRateOutOfRange { .. })
            ));
        }
    }

    // The oracle is BS.1770-5 Table 5: the weight of each speaker of each BS.2051
    // system, and Annex 1's loudness `-0.691 + 10 log10(sum_i G_i z_i)` over the
    // mean squares `z_i` of the K-weighted channels. A sine of amplitude `a` has
    // `z = a^2 / 2 * k`, `k` the K-filter's power gain at its frequency, which
    // cancels against a reference channel of weight 1.0 at the same tone.

    const TONE_HZ: f32 = 1_000.0;
    const TONE_AMP: f32 = 0.1;

    /// Integrated loudness of `bed` with a tone of `TONE_AMP` on the `index`th
    /// channel alone, two seconds at 48 kHz.
    fn tone_on(bed: Bed, index: usize) -> Loudness {
        let width = usize::from(bed.width().count());
        let rate = SampleRate::SR_48K;
        let tone = tutti_fixtures::signal::sine(
            tutti_types::Hz(TONE_HZ),
            Amplitude(TONE_AMP),
            rate,
            tutti_types::Samples(2 * 48_000),
        );
        let mut buf = vec![0.0f32; tone.len() * width];
        for (frame, v) in tone.iter().enumerate() {
            buf[frame * width + index] = *v;
        }
        let c = Rated(LoudnessConfig::new(bed.topology()), rate);
        measure_loudness(&c, Interleaved::new(&buf, bed.width()).unwrap()).unwrap()
    }

    /// Table 5, per system: the weight of each speaker (1.00 or 1.41) in the
    /// bed's channel order, `None` for the LFE.
    fn table_5(bed: Bed) -> Vec<Option<f32>> {
        const W: Option<f32> = Some(1.41);
        const U: Option<f32> = Some(1.0);
        match bed {
            // System A, 0+2+0: M+030 M-030.
            Bed::Stereo => vec![U, U],
            // System B, 0+5+0: M+030 M-030 M+000 LFE M+110 M-110.
            Bed::Surround51 => vec![U, U, U, None, W, W],
            // System I, 0+7+0: M+030 M-030 M+000 LFE M+090 M-090 M+135 M-135.
            Bed::Surround71 => vec![U, U, U, None, W, W, U, U],
            // System J, 4+7+0: system I then U+045 U-045 U+135 U-135.
            Bed::Surround714 => vec![U, U, U, None, W, W, U, U, U, U, U, U],
            // System C, 2+5+0: system B then U+030 U-030.
            Bed::Surround512 => vec![U, U, U, None, W, W, U, U],
            // System D, 4+5+0: system B then U+030 U-030 U+110 U-110.
            Bed::Surround514 => vec![U, U, U, None, W, W, U, U, U, U],
            other => panic!("{other:?} is not pinned"),
        }
    }

    /// Each speaker of each BS.2051 bed the crate pins weighs what Table 5
    /// says: a tone on it reads `-0.691 + 10 log10(G z)`, so against the same
    /// tone on the front left (weight 1.0) it is `10 log10(G)` louder; and the
    /// LFE adds no loudness (its peak still counts).
    #[test]
    fn a_speaker_weighs_what_table_5_lists_for_its_system() {
        for bed in [
            Bed::Stereo,
            Bed::Surround51,
            Bed::Surround71,
            Bed::Surround512,
            Bed::Surround514,
            Bed::Surround714,
        ] {
            let reference = tone_on(bed, 0);
            for (index, weight) in table_5(bed).into_iter().enumerate() {
                let read = tone_on(bed, index);
                match weight {
                    Some(g) => {
                        let want = reference.lufs.get() + 10.0 * g.log10();
                        assert!(
                            (read.lufs.get() - want).abs() < 0.01,
                            "{bed:?} channel {index}: {:?} vs {want}",
                            read.lufs
                        );
                    }
                    None => {
                        assert_eq!(read.lufs, Db(-70.0), "{bed:?} LFE counted: {:?}", read.lufs);
                        // The LFE is unweighted, not unmetered: the file's peak
                        // is still the tone's amplitude, 20 log10 0.1 = -20 dBTP.
                        assert!(
                            (read.true_peak.get() + 20.0).abs() < 0.1,
                            "{bed:?} LFE peak {:?}",
                            read.true_peak
                        );
                    }
                }
            }
        }
    }

    /// The absolute figure, from Annex 1: a 1 kHz sine of amplitude 0.1 on one
    /// 1.0 speaker has mean square `a^2 / 2 = 0.005`, and K-weighting's gain at
    /// 1 kHz is 0.691 dB (the constant of the formula, cancelling at 997 Hz), so
    /// it reads `10 log10(0.005) = -23.01` LKFS; on a 1.41 speaker it reads
    /// 1.49 dB (`10 log10 1.41`) more.
    #[test]
    fn one_tone_reads_the_annex_1_figure_at_weight_one_and_one_point_four_one() {
        let unit = tone_on(Bed::Surround51, 0).lufs.get();
        let side = tone_on(Bed::Surround51, 4).lufs.get();
        let analytic = 10.0 * (f64::from(TONE_AMP).powi(2) / 2.0).log10() as f32;
        assert!((unit - analytic).abs() < 0.05, "{unit} vs {analytic}");
        assert!((side - (analytic + 1.4922)).abs() < 0.05, "{side}");
    }

    /// The 7.1's rears (M+-135) are 1.0 although the 5.1's M+-110 is 1.41, and
    /// the 7.1.4's upper speakers are 1.0: a tone on each reads as the front
    /// left's.
    #[test]
    fn rears_and_upper_speakers_weigh_one() {
        let front = tone_on(Bed::Surround714, 0).lufs.get();
        for index in [6, 7, 8, 9, 10, 11] {
            let read = tone_on(Bed::Surround714, index).lufs.get();
            assert!(
                (read - front).abs() < 0.01,
                "channel {index}: {read} vs {front}"
            );
        }
    }

    /// A topology in another order is weighed by speaker, not by index: the
    /// WAVE 5.1 (`FL FR FC LFE BL BR`) puts the 1.41 speakers on the BL and BR
    /// channels, which are the 5.1's M+-110.
    #[test]
    fn the_weight_follows_the_speaker_in_a_reordered_topology() {
        let reordered = ChannelTopology::new([
            Speaker::SideLeft,
            Speaker::FrontLeft,
            Speaker::LowFrequency,
            Speaker::FrontRight,
            Speaker::FrontCenter,
            Speaker::SideRight,
        ])
        .unwrap();
        let c = Rated(LoudnessConfig::new(reordered), SampleRate::SR_48K);
        let tone = tutti_fixtures::signal::sine(
            tutti_types::Hz(TONE_HZ),
            Amplitude(TONE_AMP),
            SampleRate::SR_48K,
            tutti_types::Samples(2 * 48_000),
        );
        let read = |channel: usize| {
            let mut buf = vec![0.0f32; tone.len() * 6];
            for (frame, v) in tone.iter().enumerate() {
                buf[frame * 6 + channel] = *v;
            }
            measure_loudness(
                &c,
                Interleaved::new(&buf, ChannelLayout::from(6u16)).unwrap(),
            )
            .unwrap()
            .lufs
            .get()
        };
        let (side, front, lfe) = (read(0), read(1), read(2));
        assert!((side - front - 10.0 * 1.41f32.log10()).abs() < 0.01);
        assert_eq!(lfe, -70.0);
    }

    /// Ambisonic input and speakers that are no known bed are refused with
    /// their own errors, by both configs, where the rate binds.
    #[test]
    fn a_topology_without_angles_is_refused_by_name() {
        let rate = SampleRate::SR_48K;
        let ambisonic: ChannelTopology = AmbisonicFormat::ambix(AmbisonicOrder::of(1)).into();
        let unplaced = ChannelTopology::unassigned(ChannelLayout::from(3u16));
        assert_eq!(
            Measure::<f32>::measure(&LoudnessConfig::new(ambisonic.clone()), rate).unwrap_err(),
            Error::AmbisonicLoudness
        );
        assert_eq!(
            Measure::<f32>::measure(&LoudnessConfig::new(unplaced.clone()), rate).unwrap_err(),
            Error::UnplacedLoudnessChannels {
                width: ChannelLayout::from(3u16)
            }
        );
        assert_eq!(
            Measure::<f32>::measure(&WindowedLoudnessConfig::new(ambisonic), rate).unwrap_err(),
            Error::AmbisonicLoudness
        );
        assert!(matches!(
            Measure::<f32>::measure(&WindowedLoudnessConfig::new(unplaced), rate),
            Err(Error::UnplacedLoudnessChannels { .. })
        ));
    }

    /// The windowed meter weighs by placement as the integrated one does: the
    /// momentary loudness of a tone on the 5.1's side speaker is `10 log10 1.41`
    /// above the same tone on its front left.
    #[test]
    fn the_windowed_meter_weighs_by_placement() {
        let cfg = Rated(
            WindowedLoudnessConfig::new(Bed::Surround51.topology()),
            SampleRate::SR_48K,
        );
        let tone = tutti_fixtures::signal::sine(
            tutti_types::Hz(TONE_HZ),
            Amplitude(TONE_AMP),
            SampleRate::SR_48K,
            tutti_types::Samples(48_000),
        );
        let momentary = |channel: usize| {
            let mut buf = vec![0.0f32; tone.len() * 6];
            for (frame, v) in tone.iter().enumerate() {
                buf[frame * 6 + channel] = *v;
            }
            let mut state = cfg.0.measure(cfg.1).unwrap();
            read(
                &mut state,
                Interleaved::new(&buf, Bed::Surround51.width()).unwrap(),
            )
            .unwrap()
            .momentary
            .get()
        };
        let (front, side, lfe) = (momentary(0), momentary(4), momentary(3));
        assert!((side - front - 10.0 * 1.41f32.log10()).abs() < 0.01);
        assert_eq!(lfe, WindowedReading::LOUDNESS_FLOOR.get());
    }

    /// Whether `label` is one `ebur128` weighs 1.41.
    fn weighs_141(label: Channel) -> bool {
        matches!(label, Channel::LeftSurround | Channel::RightSurround)
    }

    /// Table 4's edges: 1.41 needs |elevation| < 30 (open) and 60 <= |azimuth|
    /// <= 120 (closed), on either side. A `<=` on the elevation or an open
    /// azimuth range moves one of these rows.
    #[test]
    fn table_4_edges_are_closed_on_azimuth_and_open_on_elevation() {
        let at = |azimuth: f32, elevation: f32| {
            weighs_141(label_at(Direction::new(
                Azimuth(azimuth),
                Elevation(elevation),
            )))
        };
        for sign in [1.0f32, -1.0] {
            assert!(at(sign * 60.0, 0.0), "60 is inside");
            assert!(at(sign * 120.0, 0.0), "120 is inside");
            assert!(!at(sign * 59.9, 0.0), "59.9 is outside");
            assert!(!at(sign * 120.1, 0.0), "120.1 is outside");
            assert!(at(sign * 90.0, 29.9), "elevation 29.9 is inside");
            assert!(!at(sign * 90.0, 30.0), "elevation 30 is outside");
            assert!(at(sign * 90.0, -29.9), "elevation -29.9 is inside");
            assert!(!at(sign * 90.0, -30.0), "elevation -30 is outside");
        }
    }

    /// A quad given as VST3 spells it (`FL FR SL SR`) is the bed's M+-135
    /// backs, which weigh 1.0.
    #[test]
    fn a_quads_side_speakers_stand_for_its_backs_and_weigh_one() {
        let vst3_quad = ChannelTopology::new([
            Speaker::FrontLeft,
            Speaker::FrontRight,
            Speaker::SideLeft,
            Speaker::SideRight,
        ])
        .unwrap();
        let labels = channel_labels(&vst3_quad).unwrap();
        assert!(labels.iter().all(|&l| !weighs_141(l)), "{labels:?}");
    }

    /// The classic inter-sample case: a sine at a quarter of the rate, phase 45
    /// degrees, has samples of +-a/sqrt(2) and a true peak of `a`. At a = 0.5
    /// the sample peak is -9.03 dBFS and the true peak -6.02 dBTP.
    #[test]
    fn the_true_peak_meter_reads_the_inter_sample_peak() {
        let rate = SampleRate::SR_48K;
        let samples: Vec<f32> = (0..48_000)
            // The phase repeats every four samples: reduced first, so the f32
            // argument stays exact.
            .map(|n| {
                0.5 * ((n % 4) as f32 * std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_4)
                    .sin()
            })
            .collect();
        let sample_peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((sample_peak - 0.5 / 2f32.sqrt()).abs() < 1e-3);
        let mut state = TruePeakConfig.measure((rate, ChannelLayout::MONO)).unwrap();
        state
            .push(
                Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
                &mut (),
            )
            .unwrap();
        let peak = state.finish().get();
        let analytic = 20.0 * 0.5f32.log10();
        // The 4x oversampling filter reads 0.1 dB high at this phase.
        assert!((peak - analytic).abs() < 0.15, "{peak} vs {analytic}");
        assert!(peak > 20.0 * sample_peak.log10() + 2.9, "{peak}");
    }

    /// Silence reads the floor, and a chunk of another width is refused.
    #[test]
    fn the_true_peak_meter_floors_silence_and_refuses_a_wrong_width() {
        let mut state = TruePeakConfig
            .measure((SampleRate::SR_48K, ChannelLayout::STEREO))
            .unwrap();
        let loud = vec![0.9f32; 4_800 * 4];
        assert!(matches!(
            state.push(
                Interleaved::new(&loud, ChannelLayout::QUAD).unwrap(),
                &mut ()
            ),
            Err(Error::ChunkLayoutMismatch { .. })
        ));
        state
            .push(
                Interleaved::new(&[0.0f32; 960], ChannelLayout::STEREO).unwrap(),
                &mut (),
            )
            .unwrap();
        assert_eq!(state.finish(), Db::FLOOR);
    }

    /// The one-shot true peak is a meter fed the buffer, at any chunking.
    #[test]
    fn the_true_peak_one_shot_equals_a_chunked_meter() {
        let buf = sine(48_000.0, 0.5, 1_000.0, 0.7);
        let whole = Interleaved::new(&buf, ChannelLayout::STEREO).unwrap();
        let one_shot = measure_true_peak(whole, SampleRate::SR_48K).unwrap();
        for frames in [1, 777, 4800, 24_000] {
            let mut meter = TruePeakConfig
                .measure((SampleRate::SR_48K, ChannelLayout::STEREO))
                .unwrap();
            for chunk in buf.chunks(frames * 2) {
                meter
                    .push(
                        Interleaved::new(chunk, ChannelLayout::STEREO).unwrap(),
                        &mut (),
                    )
                    .unwrap();
            }
            assert_eq!(meter.finish(), one_shot, "chunks of {frames} frames");
        }
    }

    /// The same fs/4 figure at 16-bit: the samples are the same sine rounded to
    /// the format, and the meter reads its nominal level, -6.02 dBTP.
    #[test]
    fn the_true_peak_meter_reads_the_same_figure_at_i16() {
        let samples: Vec<i16> = (0..48_000)
            .map(|n| {
                let x = 0.5
                    * ((n % 4) as f32 * std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_4)
                        .sin();
                x.convert_to::<i16>()
            })
            .collect();
        let peak = measure_true_peak(
            Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
            SampleRate::SR_48K,
        )
        .unwrap()
        .get();
        assert!((peak - 20.0 * 0.5f32.log10()).abs() < 0.15, "{peak}");
    }

    /// The trait fold over uneven chunks (1, 7, 64 frames and the rest) is
    /// `measure_loudness`, and a push yields no item.
    #[test]
    fn the_trait_fold_over_uneven_chunks_equals_measure_loudness() {
        let c = cfg(48_000.0);
        let buf = sine(48_000.0, 1.0, 1_000.0, 0.5);
        let batch =
            measure_loudness(&c, Interleaved::new(&buf, ChannelLayout::STEREO).unwrap()).unwrap();
        let (items, loudness) = fold_uneven(&c.0, c.1, buf.len() / 2, |meter, at, out| {
            let chunk = Interleaved::new(&buf[at.start * 2..at.end * 2], ChannelLayout::STEREO);
            meter.push(chunk.unwrap(), out)
        })
        .unwrap();
        assert!(items.is_empty());
        assert_eq!(loudness, batch);
        assert!(batch.lufs.get() > -20.0, "the tone is audible: {batch:?}");
    }

    /// The trait fold over uneven chunks is `measure_true_peak`, which reads
    /// the inter-sample peak of this fs/4 sine.
    #[test]
    fn the_true_peak_trait_fold_over_uneven_chunks_equals_measure_true_peak() {
        let rate = SampleRate::SR_48K;
        let samples: Vec<f32> = (0..4_800)
            .map(|n| {
                0.5 * ((n % 4) as f32 * std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_4)
                    .sin()
            })
            .collect();
        let batch = measure_true_peak(
            Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
            rate,
        )
        .unwrap();
        let (items, peak) = fold_uneven(
            &TruePeakConfig,
            (rate, ChannelLayout::MONO),
            samples.len(),
            |meter, at, out| {
                meter.push(
                    Interleaved::new(&samples[at], ChannelLayout::MONO).unwrap(),
                    out,
                )
            },
        )
        .unwrap();
        assert!(items.is_empty());
        assert_eq!(peak, batch);
        // The samples reach 0.5 / sqrt 2 (-9.03 dB); the sine between them 0.5.
        assert!(
            (peak.get() - 20.0 * 0.5f32.log10()).abs() < 0.15,
            "{peak:?}"
        );
    }

    /// The windowed fold yields one reading per push, in order, and its last
    /// item and its output are the reading of one push of the whole stream: the
    /// reading does not depend on how the stream is cut.
    #[test]
    fn the_windowed_trait_fold_yields_one_reading_per_push_and_ends_on_the_whole_stream_reading() {
        let c = windowed_cfg(48_000.0);
        let buf = windowed_sine(48_000.0, 1.0, |_| 0.1);
        let (items, last) = fold_uneven(&c.0, c.1, buf.len() / 2, |meter, at, out| {
            let chunk = Interleaved::new(&buf[at.start * 2..at.end * 2], ChannelLayout::STEREO);
            meter.push(chunk.unwrap(), out)
        })
        .unwrap();
        assert_eq!(items.len(), 4, "one reading for each of the four pushes");
        let mut whole = c.0.measure(c.1).unwrap();
        let reading = read(
            &mut whole,
            Interleaved::new(&buf, ChannelLayout::STEREO).unwrap(),
        )
        .unwrap();
        assert_eq!(items.last(), Some(&reading));
        assert_eq!(last, reading);
        assert_eq!(
            reading.frame,
            Samples(buf.len() / 2),
            "the reading names its end frame"
        );
        assert!(reading.momentary.get() > -30.0, "{reading:?}");
    }

    /// A 997 Hz tone in one channel of a 5.1 stream over 5000 frames, which no
    /// conversion block divides: in a surround channel it reads the 1.41 weight
    /// (+1.49 dB) above the same tone in the left channel, and in the LFE it
    /// reads nothing. A sample moved to another channel by a run cut off a frame
    /// boundary breaks all three.
    #[test]
    fn a_wide_layout_keeps_every_sample_on_its_channel() {
        let c = Rated(
            LoudnessConfig::new(Bed::Surround51.topology()),
            SampleRate::SR_48K,
        );
        let frames = 48_000;
        let tone = |at: usize| {
            let mut buf = vec![0.0f32; frames * 6];
            for (n, frame) in buf.as_chunks_mut::<6>().0.iter_mut().enumerate() {
                frame[at] = 0.3 * (std::f32::consts::TAU * 997.0 * n as f32 / 48_000.0).sin();
            }
            measure_loudness(&c, Interleaved::new(&buf, Bed::Surround51.width()).unwrap())
                .unwrap()
                .lufs
                .get()
        };
        let (left, surround, lfe) = (tone(0), tone(4), tone(3));
        assert!(
            (surround - left - 10.0 * 1.41f32.log10()).abs() < 0.05,
            "{left} {surround}"
        );
        assert_eq!(lfe, -70.0, "the LFE adds no loudness");
    }

    /// The true peak of a sine in one channel of 3 and of 6 equals the mono
    /// true peak of that sine, to the bit: each channel's samples stay in its
    /// own stream whatever the run length.
    #[test]
    fn the_true_peak_of_a_channel_does_not_depend_on_the_layout_width() {
        let rate = SampleRate::SR_48K;
        let sine: Vec<f32> = (0..5000)
            .map(|n| 0.4 * (std::f32::consts::TAU * 3_001.0 * n as f32 / 48_000.0).sin())
            .collect();
        let mono =
            measure_true_peak(Interleaved::new(&sine, ChannelLayout::MONO).unwrap(), rate).unwrap();
        for width in [3u16, 6] {
            let layout = ChannelLayout::from(width);
            let w = usize::from(width);
            let mut buf = vec![0.0f32; 5000 * w];
            for (frame, &v) in buf.chunks_exact_mut(w).zip(&sine) {
                frame[w - 1] = v;
            }
            let peak = measure_true_peak(Interleaved::new(&buf, layout).unwrap(), rate).unwrap();
            assert_eq!(peak, mono, "{width} channels");
        }
        assert!((mono.get() - 20.0 * 0.4f32.log10()).abs() < 0.3, "{mono:?}");
    }
}
