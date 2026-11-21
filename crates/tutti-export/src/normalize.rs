//! Normalization: choosing a gain from a measurement of the whole signal.
//!
//! Normalization needs the whole signal measured before a gain can be chosen,
//! which is two passes, so an output that asks for it
//! ([`FileTarget::normalize`](crate::FileTarget::normalize)) is not
//! streamed to its encoder while the graph renders: [`render`](crate::render())
//! keeps what the graph rendered for it (`spill`: memory up to a budget, then a
//! temporary file beside the target), measures it with a [`Measurer`] at the
//! file's rate, and writes it again through the same chain a streamed output
//! uses, with the gain. The graph is rendered once.
//!
//! A caller who holds planes and wants the reading for itself uses
//! [`Normalize::gain_for_rendered`] and [`Rendered::apply_gain`](crate::Rendered::apply_gain).

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{Float, Interleaved, InterleavedMut, Planes, Sample, convert, interleave};
use tutti_dsp::DitherShape;
use tutti_measure::{
    Loudness, LoudnessConfig, LoudnessMeter, Measure, Measurement as _, TruePeakConfig,
    TruePeakMeter,
};
use tutti_units::{Amplitude, Db, SampleRate, Samples};

use crate::Result;
use crate::Unmeasured;
#[cfg(doc)]
use crate::Written;
use crate::control::{Control, Outcome, Stage, Weights};
use crate::output::FileTarget;

/// A gain chosen by measuring the rendered signal.
///
/// Both variants measure with the same EBU R128 meter, so both cost the same
/// second pass; they differ only in what they aim at. In particular `Peak`
/// targets true peak, not sample peak — an
/// inter-sample peak a sample-peak reading misses is exactly what clips on a
/// consumer DAC after resampling. The meter oversamples 4× below 96 kHz and 2×
/// from 96 kHz to below 192 kHz; at 192 kHz and above it reads the sample peak.
///
/// # Placement
///
/// [`Lufs`](Self::Lufs) weights each channel by where its speaker stands
/// (BS.1770-5 Table 4): 1.41 for the side speakers, 1.0 for the rest, and the
/// LFE adds no loudness (its peak still counts). A 5.1 mix with only the LFE
/// active reads -70 LUFS.
///
/// The placement is the file's [`topology`](crate::FileTarget::topology). A
/// file that states none is taken to be [`ChannelTopology::default_for`] for its
/// width (1, 2, 4, 6, 8 or 12 channels) and its [`Written`] reports
/// [`Placement::Default`](tutti_buffers::Placement::Default). Any other width
/// has no default layout, so a `Lufs` output of it is refused when the
/// render starts ([`TargetError::UnplacedLoudness`](crate::TargetError)):
/// state a topology. [`Peak`](Self::Peak) needs no placement; true peak folds
/// over every channel, and a `Peak` file with no placement reports no loudness
/// ([`Written::measured`] is `None`).
///
/// # Measured at the output rate
///
/// When the file is written at another rate, the conversion happens *before*
/// the measurement. Sample-rate conversion moves the true peak (its
/// interpolation overshoots between the original samples), so a gain chosen at
/// the render rate and applied to resampled audio misses its target, and a
/// ceiling chosen to prevent clipping does not prevent it. Measuring the
/// converted signal is what makes the dBTP figure a promise about the file
/// rather than about an intermediate nobody hears. (The render is converted
/// twice, once to be measured and once to be written, so as to hold no
/// converted copy; the converter is deterministic and both see the same
/// samples.)
///
/// # Headroom
///
/// For an integer format (`Pcm::Int16`, `Pcm::Int24`) the peak target of
/// [`Peak`](Self::Peak), and the ceiling of [`Lufs`](Self::Lufs), are pulled
/// down to leave one LSB plus the dither amplitude of headroom below full
/// scale (an LSB for no dither, 1.5 for rectangular, 2 for triangular): a peak
/// normalized to 0 dBTP, plus dither, would be clamped by the quantizer. A
/// target already under the limit is unchanged. [`Written::clipped_samples`]
/// counts what is clamped all the same, and [`Written::gain`] is the gain to
/// the lowered target. An output that takes another's gain
/// ([`GainSource::FromOutput`](crate::GainSource::FromOutput)) keeps that
/// output's headroom, not its own.
///
/// # Quiet audio
///
/// [`Peak`](Self::Peak) refuses only exact silence. Near-silence (-100 dBTP,
/// say) is audio with a reading and gets its gain (about +99 dB); read
/// [`Written::gain`] to see what was applied.
///
/// [`Written::gain`] reports the gain applied and [`Written::measured`] the
/// reading it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Normalize {
    /// Bring the loudest true peak to `target` dBTP.
    ///
    /// Any audio with a reading is measurable, however quiet: a render whose
    /// true peak is -100 dBTP gets about +99 dB, and the gain is applied
    /// (it is reported in [`Written::gain`]). Only exact silence, a true
    /// peak at the floor ([`Db::FLOOR`]), is refused
    /// ([`RenderError::Unmeasurable`](crate::RenderError::Unmeasurable)); there is no
    /// invented threshold between the two.
    Peak {
        /// True-peak level to land on, in dBTP. Typically negative — 0 leaves no
        /// headroom for a consumer DAC's reconstruction filter.
        target: Db,
    },
    /// Bring integrated loudness to `target` LUFS, pulled back so the true peak
    /// does not exceed `ceiling` dBTP.
    Lufs {
        /// Integrated loudness to land on, in LUFS.
        target: Db,
        /// True-peak limit in dBTP. When the loudness gain would push the peak
        /// past this, the ceiling wins and the file lands quieter than
        /// `target`. [`Normalize::lufs`] sets it to −1.0.
        ceiling: Db,
    },
}

impl Normalize {
    /// Loudness normalization with a −1.0 dBTP ceiling — the value streaming
    /// platforms ask for, carried here so callers need not know the number.
    pub const fn lufs(target: Db) -> Self {
        Self::Lufs {
            target,
            ceiling: Db::new(-1.0),
        }
    }

    /// Peak normalization to `target` dBTP.
    pub const fn peak(target: Db) -> Self {
        Self::Peak { target }
    }

    /// The gain that normalizes `rendered`, whose channels feed the speakers of
    /// `topology`: loudness is weighted by where each stands (BS.1770-5 Table 4)
    /// and the LFE adds no loudness (its peak still counts).
    ///
    /// For a caller that already holds a [`Rendered`](crate::Rendered) and is
    /// not writing it with a normalizing output — handing the PCM to
    /// another encoder, say. Returning the gain rather than applying it keeps
    /// the reading available to log or gate on, and is the same value a
    /// normalizing output uses, so the two can never disagree. (A normalizing
    /// output also pulls its target and ceiling down to leave an
    /// integer format its headroom, which this cannot: it does not know the
    /// format. See "Headroom" on [`Normalize`].)
    ///
    /// # Errors
    ///
    /// [`RenderError::TopologyWidth`](crate::RenderError::TopologyWidth) when `topology`
    /// is not as wide as `rendered`.
    ///
    /// [`RenderError::Unmeasurable`](crate::RenderError::Unmeasurable) when the audio has
    /// no usable reading, and `reason` says which:
    ///
    /// - [`Unmeasured::Format`]: the meter will not
    ///   read this signal (EBU R128 accepts 1–64 channels at 16 Hz–2.8 MHz);
    /// - [`Unmeasured::Placement`], for [`Lufs`](Self::Lufs): `topology` has
    ///   no speaker angles to weigh by (ambisonic, or speakers that are not a
    ///   known bed). [`Peak`](Self::Peak) needs no placement;
    /// - [`Unmeasured::BelowGate`], for
    ///   [`Lufs`](Self::Lufs): nothing passed the integrated loudness gate
    ///   (silence, under -70 LUFS, or under 400 ms), so the reading is the
    ///   gate's floor and not a measurement;
    /// - [`Unmeasured::Silent`], for
    ///   [`Peak`](Self::Peak): the true peak is at the floor.
    ///
    /// An error rather than a `None` the caller might discard, or a gain made
    /// up from the floor (+47 dB for a LUFS target of -23, up to +99 dB for a
    /// peak one): writing a hiss at full scale, or the file un-normalized, and
    /// reporting success is silent — nothing in [`Written`] records it.
    pub fn gain_for_rendered<F: Float>(
        &self,
        rendered: &crate::Rendered<F>,
        topology: &ChannelTopology,
    ) -> Result<Db> {
        if topology.width() != rendered.layout() {
            return Err(crate::RenderError::TopologyWidth {
                topology: topology.width(),
                rendered: rendered.layout(),
            });
        }
        let mut control = Control::none();
        control.weigh(Weights::Normalized);
        let total = rendered.frames();
        let outcome = self.measure(rendered, topology, Samples::new(0), total, &mut control)?;
        Ok(finished(outcome).gain())
    }

    /// The reading [`gain_for_rendered`](Self::gain_for_rendered) takes its
    /// gain from, refused when it is not a measurement of the programme.
    ///
    /// Measured by a [`Measurer`] a chunk at a time, so `control` is checked
    /// between chunks and each chunk reads as [`Stage::Measuring`]: `done`
    /// frames of `total` were already spent in that stage (the conversion
    /// before it), and this reading counts `rendered.frames()` more.
    ///
    /// # Errors
    ///
    /// As [`gain_for_rendered`](Self::gain_for_rendered).
    fn measure<F: Float>(
        &self,
        rendered: &crate::Rendered<F>,
        topology: &ChannelTopology,
        done: Samples,
        total: Samples,
        control: &mut Control<'_>,
    ) -> Result<Outcome<Reading>> {
        let unmeasurable = |reason| crate::RenderError::Unmeasurable {
            output: None,
            channels: rendered.layout(),
            sample_rate: rendered.sample_rate(),
            reason,
        };
        let mut measurer =
            Measurer::new(rendered.sample_rate(), topology, *self).map_err(unmeasurable)?;
        let planes = rendered.planes();
        let frames = rendered.frames().get();
        let chunk = chunk_frames(rendered.sample_rate()).get();
        let mut at = 0;
        while at < frames {
            if control.is_cancelled() {
                return Ok(Outcome::Cancelled);
            }
            let end = (at + chunk).min(frames);
            measurer.feed(planes, at..end)?;
            at = end;
            control.advance(Stage::Measuring, done + Samples::new(at), total);
        }
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let reading = measurer.finish();
        reading.check().map_err(unmeasurable)?;
        Ok(Outcome::Finished(reading))
    }

    /// This mode with its peak target (`Peak`) or ceiling (`Lufs`) pulled down
    /// to what the file's format can hold.
    ///
    /// An integer format clamps at full scale, and dither adds noise to every
    /// sample before it does, so a peak at 0 dBTP clips about half its crests.
    /// The limit is one LSB plus the dither's amplitude (0 for none, half an
    /// LSB for rectangular, one for triangular) below full scale; a target or
    /// ceiling already under it is unchanged, and a format that does not
    /// quantize ([`Pcm::Float32`](tutti_codec::Pcm::Float32)) has no
    /// limit.
    pub(crate) fn within(self, target: &FileTarget) -> Self {
        let Some(limit) = headroom_ceiling(target) else {
            return self;
        };
        let cap = |db: Db| db.min(limit);
        match self {
            Self::Peak { target } => Self::Peak {
                target: cap(target),
            },
            Self::Lufs { target, ceiling } => Self::Lufs {
                target,
                ceiling: cap(ceiling),
            },
        }
    }
}

/// The value of an outcome no token could have cancelled.
fn finished<T>(outcome: Outcome<T>) -> T {
    outcome
        .into_finished()
        .expect("a control with no token is never cancelled")
}

/// The loudest peak, in dBTP, an integer format can take with its dither
/// ([`Normalize::within`]), or `None` for a format that does not quantize.
fn headroom_ceiling(target: &FileTarget) -> Option<Db> {
    let depth = target.format().int_depth()?;
    // One LSB in the `[-1, 1]` float domain, as the dither scales it.
    let lsb = depth.lsb().get();
    let dither_peak = match target.dither {
        None => 0.0,
        Some(DitherShape::Rectangular) => 0.5,
        Some(DitherShape::Triangular) => 1.0,
    };
    Some(Db::from_amplitude(Amplitude::new(
        1.0 - (1.0 + dither_peak) * lsb,
    )))
}

/// The most frames a measurement takes at once: one second of audio at `rate`,
/// rounded up, and at least one. A cancel waits for at most one chunk, and the
/// chunk is the unit the conversion before it is cut in too.
pub(crate) fn chunk_frames(rate: SampleRate) -> Samples {
    rate.one_second()
}

/// What a peak-normalizing [`Measurer`] read: a placement-weighted loudness,
/// or the true peak alone for channels with no placement.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PeakReading {
    /// Loudness, range and true peak, weighted by the channels' placement.
    Placed(Loudness),
    /// The true peak, in dBTP, and nothing else.
    TruePeak(Db),
}

impl PeakReading {
    fn true_peak(&self) -> Db {
        match self {
            Self::Placed(loudness) => loudness.true_peak,
            Self::TruePeak(peak) => *peak,
        }
    }
}

/// What a [`Measurer`] read, with the mode it read for: a `Lufs` reading is
/// always a placed [`Loudness`], and carries the target and ceiling its gain
/// is chosen against.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Reading {
    /// An integrated loudness, to bring to `target` under `ceiling`.
    Lufs {
        loudness: Loudness,
        target: Db,
        ceiling: Db,
    },
    /// A true peak, to bring to `target`.
    Peak { measured: PeakReading, target: Db },
}

impl Reading {
    /// The gain the mode asks for.
    ///
    /// Both arms move a measured dB figure to a target, and only the loudness
    /// arm has a ceiling to respect (which
    /// [`Loudness::gain_to`](tutti_measure::Loudness::gain_to) applies).
    pub(crate) fn gain(&self) -> Db {
        match *self {
            Self::Lufs {
                loudness,
                target,
                ceiling,
            } => loudness.gain_to(target, ceiling),
            // `Db - Db` is a gain difference, which is why `Db` implements
            // `Sub` — see the `Additive` note on `Db` in `units/level.rs`.
            Self::Peak { measured, target } => target - measured.true_peak(),
        }
    }

    /// The loudness, when the channels had a placement to weigh it by.
    pub(crate) fn loudness(&self) -> Option<Loudness> {
        match *self {
            Self::Lufs { loudness, .. } => Some(loudness),
            Self::Peak { measured, .. } => match measured {
                PeakReading::Placed(loudness) => Some(loudness),
                PeakReading::TruePeak(_) => None,
            },
        }
    }

    /// Whether this measures the programme, for what the mode aims at.
    ///
    /// The loudness meter reports its absolute gate (-70 LUFS) when nothing
    /// passed it, and the true-peak floor for silence: those are the readings
    /// of "no signal", not of a quiet one, and a gain from either is invented.
    pub(crate) fn check(&self) -> std::result::Result<(), Unmeasured> {
        match self {
            // An integrated loudness is always above the gate when it was
            // measured, so the gate itself is the "passed nothing" sentinel.
            Self::Lufs { loudness, .. } if loudness.lufs <= ABSOLUTE_GATE_LUFS => {
                Err(Unmeasured::BelowGate)
            }
            Self::Peak { measured, .. } if measured.true_peak() <= Db::FLOOR => {
                Err(Unmeasured::Silent)
            }
            _ => Ok(()),
        }
    }
}

/// A loudness meter over channels with a placement.
struct PlacedMeter {
    layout: ChannelLayout,
    meter: LoudnessMeter<f32>,
}

/// A true-peak meter over channels with no placement.
struct UnplacedMeter {
    layout: ChannelLayout,
    meter: TruePeakMeter<f32>,
}

/// What a peak-normalizing output measures with: loudness when its channels
/// have a placement, the true peak alone when they have none.
enum PeakMeter {
    Placed(PlacedMeter),
    TruePeak(UnplacedMeter),
}

/// The meter of one mode, which fixes what it can read: a `Lufs` meter is
/// always placed, so its reading is always a loudness.
enum Meter {
    Lufs {
        meter: PlacedMeter,
        target: Db,
        ceiling: Db,
    },
    Peak {
        meter: PeakMeter,
        target: Db,
    },
}

/// A reading taken a chunk at a time, over planes.
///
/// The smallest thing normalize needs to be stoppable: `tutti-measure`'s
/// `LoudnessMeter` already accumulates (and is what `measure_loudness` folds),
/// so this only adds the planar-to-interleaved copy of one chunk, so that no
/// whole interleaved copy of the render is made (the one-shot reading made
/// one), and the chunking the caller checks `Control` between. Chunks end on
/// frame boundaries, where the reading does not depend on where the stream is
/// cut.
pub(crate) struct Measurer {
    meter: Meter,
    /// One chunk, interleaved: reused, so a measurement allocates once.
    scratch: Vec<f32>,
    /// What a block of the render's own format is converted to, the meter
    /// reading `f32`.
    converted: Vec<f32>,
}

impl Measurer {
    /// A meter for frames at `rate` in `topology`, for `normalize`.
    ///
    /// Loudness is weighted by placement, so a topology with angles reads a
    /// loudness. One without reads the true peak alone under
    /// [`Peak`](Normalize::Peak), and is [`Unmeasured::Placement`] under
    /// [`Lufs`](Normalize::Lufs). [`Unmeasured::Format`] for a rate or width
    /// EBU R128 does not take.
    pub(crate) fn new(
        rate: SampleRate,
        topology: &ChannelTopology,
        normalize: Normalize,
    ) -> std::result::Result<Self, Unmeasured> {
        let layout = topology.width();
        let placed = LoudnessConfig::new(topology.clone())
            .measure(rate)
            .map(|meter| PlacedMeter { layout, meter });
        let meter = match (normalize, placed) {
            (Normalize::Lufs { target, ceiling }, Ok(meter)) => Meter::Lufs {
                meter,
                target,
                ceiling,
            },
            (Normalize::Peak { target }, Ok(meter)) => Meter::Peak {
                meter: PeakMeter::Placed(meter),
                target,
            },
            (
                Normalize::Peak { target },
                Err(
                    tutti_measure::Error::AmbisonicLoudness
                    | tutti_measure::Error::UnplacedLoudnessChannels { .. },
                ),
            ) => Meter::Peak {
                meter: PeakMeter::TruePeak(UnplacedMeter {
                    layout,
                    meter: TruePeakConfig
                        .measure((rate, layout))
                        .map_err(|_| Unmeasured::Format)?,
                }),
                target,
            },
            (
                Normalize::Lufs { .. },
                Err(
                    tutti_measure::Error::AmbisonicLoudness
                    | tutti_measure::Error::UnplacedLoudnessChannels { .. },
                ),
            ) => return Err(Unmeasured::Placement),
            // The meter cannot be built at this rate or width, and
            // `tutti_measure::Error` is non-exhaustive.
            (_, Err(_)) => return Err(Unmeasured::Format),
        };
        Ok(Self {
            meter,
            scratch: Vec::new(),
            converted: Vec::new(),
        })
    }

    /// The width of the frames fed.
    fn layout(&self) -> ChannelLayout {
        match &self.meter {
            Meter::Lufs { meter, .. }
            | Meter::Peak {
                meter: PeakMeter::Placed(meter),
                ..
            } => meter.layout,
            Meter::Peak {
                meter: PeakMeter::TruePeak(meter),
                ..
            } => meter.layout,
        }
    }

    /// Feeds the frames `span` of `planes`.
    fn feed<F: Float>(
        &mut self,
        planes: &[Vec<F>],
        span: std::ops::Range<usize>,
    ) -> crate::Result<()> {
        let layout = self.layout();
        // The scratch is taken out so the meter and the chunk borrow apart.
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        buf.resize(layout.interleaved_len(Samples::new(span.len())), 0.0);
        let fed = (|| -> crate::Result<()> {
            let mut chunk = InterleavedMut::new(&mut buf, layout)?;
            let span = Samples::new(span.start)..Samples::new(span.end);
            interleave(Planes::from_vecs(planes)?.slice(span), chunk.reborrow())?;
            self.push(chunk.as_interleaved());
            Ok(())
        })();
        self.scratch = buf;
        fed
    }

    /// Feeds `frames`, already interleaved in this meter's layout, in any
    /// sample format: the meter reads them as `f32`.
    pub(crate) fn feed_interleaved<S: Sample>(
        &mut self,
        frames: Interleaved<'_, S>,
    ) -> crate::Result<()> {
        let mut converted = std::mem::take(&mut self.converted);
        converted.resize(frames.samples().len(), 0.0);
        convert(
            frames,
            InterleavedMut::new(&mut converted, frames.layout())?,
        )?;
        self.push(Interleaved::new(&converted, frames.layout())?);
        self.converted = converted;
        Ok(())
    }

    /// Pushes `frames` into the meter.
    fn push(&mut self, frames: Interleaved<'_>) {
        match &mut self.meter {
            Meter::Lufs { meter, .. }
            | Meter::Peak {
                meter: PeakMeter::Placed(meter),
                ..
            } => {
                let pushed = meter.meter.push(frames, &mut ());
                debug_assert!(
                    pushed.is_ok(),
                    "the frames are the meter's width: {pushed:?}"
                );
            }
            Meter::Peak {
                meter: PeakMeter::TruePeak(meter),
                ..
            } => {
                let pushed = meter.meter.push(frames, &mut ());
                debug_assert!(
                    pushed.is_ok(),
                    "the frames are the meter's width: {pushed:?}"
                );
            }
        }
    }

    /// The reading of everything fed.
    pub(crate) fn finish(self) -> Reading {
        match self.meter {
            Meter::Lufs {
                meter,
                target,
                ceiling,
            } => Reading::Lufs {
                loudness: meter.meter.finish(),
                target,
                ceiling,
            },
            Meter::Peak { meter, target } => Reading::Peak {
                measured: match meter {
                    PeakMeter::Placed(meter) => PeakReading::Placed(meter.meter.finish()),
                    PeakMeter::TruePeak(meter) => PeakReading::TruePeak(meter.meter.finish()),
                },
                target,
            },
        }
    }
}

/// EBU R128's absolute gate, in LUFS: what the meter's integrated loudness
/// reads when no block passed it.
const ABSOLUTE_GATE_LUFS: Db = Db::new(-70.0);

#[cfg(test)]
mod tests;
