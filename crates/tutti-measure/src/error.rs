//! Failures of the analysers, one error per call family.
//!
//! `Result`, not `Option`, and not `assert!`: a caller who passes a bad
//! setting deserves to know which invariant broke, and a caller feeding a
//! short buffer deserves better than a panic on the audio path. A family's error
//! carries only what its calls can return: [`FramingError`], [`AboveNyquist`] and
//! [`TooManyFrames`] are the shared refusals the families wrap.

use tutti_buffers::ChannelLayout;
use tutti_dsp::FrameCount;
use tutti_units::{Bpm, Db, Hz, SampleRate, Samples};

/// A frequency above half the sample rate, refused by every call that takes a
/// band or a bound against a rate.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("frequency {freq} is above Nyquist {nyquist}")]
pub struct AboveNyquist {
    /// The offending bound, in [`Hz`].
    pub freq: Hz,
    /// Half the sample rate, in [`Hz`].
    pub nyquist: Hz,
}

/// More frames than one call analyses, which bounds its output.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("{frames} frames is more than the {max} one call analyses")]
pub struct TooManyFrames {
    /// The frames the input would make.
    pub frames: FrameCount,
    /// The most accepted.
    pub max: FrameCount,
}

/// A frequency band with `min` not below `max`, or a bound not finite and above
/// zero ([`FrequencyBand`](crate::FrequencyBand)'s `TryFrom`), or one a framing cannot
/// hold (fewer than three bins, no whole band).
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("empty frequency range: min {min} is not below max {max}")]
pub struct EmptyFrequencyRange {
    /// The lower bound in [`Hz`]; not below `max`.
    pub min: Hz,
    /// The upper bound in [`Hz`].
    pub max: Hz,
}

/// A tempo range with `min` not below `max`, or a bound outside what the analysis
/// searches, or one that leaves no whole-frame lag to search.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("empty tempo range: min {min} is not below max {max}, or no lag falls in it")]
pub struct EmptyTempoRange {
    /// The slower bound, in [`Bpm`].
    pub min: Bpm,
    /// The faster bound, in [`Bpm`].
    pub max: Bpm,
}

/// A velocity range whose floor is not below its ceiling.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("velocity floor {floor} is not below ceiling {ceiling}")]
pub struct EmptyVelocityRange {
    /// The level read as velocity 1.
    pub floor: Db,
    /// The level read as velocity 127.
    pub ceiling: Db,
}

/// A window and hop the analysis refuses ([`Framing`](crate::Framing)): zero, outside the accepted
/// window range, a hop wider than the window, or so small that too many frames overlap.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum FramingError {
    /// A zero window: a frame would hold nothing.
    #[error("analysis window is zero-length")]
    ZeroWindow,
    /// A zero hop: frames would never advance.
    #[error("hop size is zero: frames would not advance")]
    ZeroHop,
    /// A window outside what the analysis accepts: the FFT works on the
    /// window, so an absurd one would ask for gigabytes, and a tiny one
    /// resolves nothing.
    #[error("window of {window} is outside {min}..={max}")]
    WindowOutOfRange {
        /// The geometry's window, in [`Samples`].
        window: Samples,
        /// The smallest accepted window.
        min: Samples,
        /// The largest accepted window.
        max: Samples,
    },
    /// A hop wider than the window or frame: samples between frames would be
    /// read by none.
    #[error("hop {hop} is wider than the window {window}")]
    HopWiderThanWindow {
        /// The hop, in [`Samples`].
        hop: Samples,
        /// The window or frame, in [`Samples`].
        window: Samples,
    },
    /// A hop so small that more than `max` frames overlap in one window; each
    /// frame is a whole transform, so this bounds the cost of a tiny hop.
    #[error("a hop of {hop} makes more than {max} frames overlap in a window of {window}")]
    TooManyOverlaps {
        /// The window or frame, in [`Samples`].
        window: Samples,
        /// The hop, in [`Samples`].
        hop: Samples,
        /// The most overlapping frames accepted.
        max: usize,
    },
}

/// What the short-time transform calls refuse ([`stft`](crate::stft), [`StftConfig`](crate::StftConfig)'s geometry,
/// [`resynthesize_masked`](crate::Stft::resynthesize_masked)).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum TransformError {
    /// A window or hop the transform refuses.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// The shared STFT infrastructure refused a geometry or a grid (a hop that does
    /// not divide the window under COLA, a grid whose shape disagrees with its data).
    #[error(transparent)]
    Dsp(#[from] tutti_dsp::StftError),
    /// Two grids that must share a shape do not.
    #[error("grids that must share a shape have different shapes")]
    GridShapeDisagreement,
}

/// What [`mfcc`](crate::mfcc) refuses: a filterbank that does not fit the transform, or more coefficients than bands.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum MfccError {
    /// A mel filterbank that does not fit the transform it was asked to read.
    #[error(transparent)]
    Mel(#[from] tutti_dsp::MelError),
    /// More cepstral coefficients asked of a bank than it has bands.
    #[error(transparent)]
    Coefficients(#[from] tutti_dsp::TooManyOutputs),
}

/// What the pitch tracks refuse ([`yin`](crate::yin), [`yin_track`](crate::yin_track), [`pyin_track`](crate::pyin_track) and their `buffer_size`).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum PitchError {
    /// A band above Nyquist.
    #[error(transparent)]
    Nyquist(#[from] AboveNyquist),
    /// A track of more frames than one call analyses.
    #[error(transparent)]
    TooManyFrames(#[from] TooManyFrames),
    /// Input shorter than the algorithm's minimum.
    #[error("need at least {needed} samples, got {got}")]
    InsufficientInput {
        /// The algorithm's minimum, in [`Samples`].
        needed: Samples,
        /// What the caller supplied, in [`Samples`].
        got: Samples,
    },
    /// A sample that is NaN or infinite.
    #[error("input holds a NaN or infinite sample")]
    NonFiniteInput,
    /// A refusal of binding the meter that no variant above names.
    #[error(transparent)]
    Meter(Box<MeterError>),
    /// A frequency band whose pitch grid holds more bins than a track decodes.
    #[error("the band spans {bins} pitch bins, more than the {max} a track decodes")]
    PitchGridTooLarge {
        /// The bins the band would span.
        bins: usize,
        /// The most accepted.
        max: usize,
    },
}

/// What note transcription refuses ([`transcribe`](crate::transcribe), [`frame_levels`](crate::frame_levels), [`notes_from_track`](crate::notes_from_track)).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum NoteError {
    /// The pitch track refused.
    #[error(transparent)]
    Pitch(#[from] PitchError),
    /// A hop or window the transcription refuses.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// More frames than one call analyses.
    #[error(transparent)]
    TooManyFrames(#[from] TooManyFrames),
    /// Input shorter than the algorithm's minimum.
    #[error("need at least {needed} samples, got {got}")]
    InsufficientInput {
        /// The algorithm's minimum, in [`Samples`].
        needed: Samples,
        /// What the caller supplied, in [`Samples`].
        got: Samples,
    },
    /// A sample that is NaN or infinite.
    #[error("input holds a NaN or infinite sample")]
    NonFiniteInput,
    /// A value that must not be negative is.
    #[error("input holds a negative value")]
    NegativeInput,
    /// Nothing to correlate: silent input, or a band holding no spectrum bin.
    #[error("no signal to correlate: silent input or an empty band")]
    NoSignal,
    /// A voiced frequency of zero or below.
    #[error("a voiced frequency is not above zero")]
    NonPositiveFrequency,
    /// A YIN frame longer than a note analysis accepts (a `min_freq` that low
    /// is not a note).
    #[error("yin frame of {frame} is longer than {max}")]
    YinFrameTooLong {
        /// The YIN frame, in [`Samples`].
        frame: Samples,
        /// The longest accepted.
        max: Samples,
    },
    /// An onset detector that is not spectral flux, the only function whose
    /// peak has a known lag behind its attack.
    #[error("note onsets need spectral flux")]
    OnsetNotSpectralFlux,
    /// Levels and a pitch track of different lengths.
    #[error("{levels} levels for a pitch track of {track} frames")]
    LevelsLengthMismatch {
        /// Levels supplied.
        levels: usize,
        /// Frames in the track.
        track: usize,
    },
    /// A refusal of binding the meter that no variant above names.
    #[error(transparent)]
    Meter(Box<MeterError>),
    /// A smoothing span of more frames than the median's cost allows.
    #[error("smoothing of {frames} frames is more than {max}")]
    SmoothingTooLong {
        /// The span, in frames at the hop in use.
        frames: FrameCount,
        /// The most accepted.
        max: FrameCount,
    },
}

/// What tempo estimation and beat tracking refuse ([`estimate_tempo`](crate::estimate_tempo), [`track_beats`](crate::track_beats) and their novelty forms).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum TempoEstimateError {
    /// A hop of zero frames.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// More frames than one call analyses.
    #[error(transparent)]
    TooManyFrames(#[from] TooManyFrames),
    /// A refusal of binding the meter that no variant above names.
    #[error(transparent)]
    Meter(Box<MeterError>),
    /// A tempo range the call cannot hold.
    #[error(transparent)]
    EmptyTempoRange(#[from] EmptyTempoRange),
    /// A tempo whose beat period is not between two frames and the novelty
    /// curve's length.
    #[error("tempo {bpm} has no beat period between two frames and the curve's length")]
    UntrackableTempo {
        /// The tempo refused.
        bpm: Bpm,
    },
    /// Input shorter than the algorithm's minimum.
    #[error("need at least {needed} samples, got {got}")]
    InsufficientInput {
        /// The algorithm's minimum, in [`Samples`].
        needed: Samples,
        /// What the caller supplied, in [`Samples`].
        got: Samples,
    },
    /// A sample that is NaN or infinite.
    #[error("input holds a NaN or infinite sample")]
    NonFiniteInput,
    /// Nothing to correlate: silent input, or a band holding no spectrum bin.
    #[error("no signal to correlate: silent input or an empty band")]
    NoSignal,
}

/// What key estimation refuses ([`estimate_key`](crate::estimate_key), [`estimate_key_from_mean`](crate::estimate_key_from_mean)).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyEstimateError {
    /// The chroma of the audio, which a [`KeyMeter`](crate::KeyMeter) reads
    /// the key from, was refused.
    #[error(transparent)]
    Chroma(#[from] MeterError),
    /// A grid of chroma that is not twelve columns wide.
    #[error("chroma is {columns} columns wide, not 12")]
    NotTwelveClasses {
        /// The grid's width.
        columns: usize,
    },
    /// A sample that is NaN or infinite.
    #[error("input holds a NaN or infinite sample")]
    NonFiniteInput,
    /// A value that must not be negative is.
    #[error("input holds a negative value")]
    NegativeInput,
    /// No pitch to read a key from: no frames, silent, or a chroma so flat
    /// that every key correlates alike.
    #[error("no tonal content: silent or flat chroma")]
    NoTonalContent,
}

/// What chord detection refuses ([`detect_chords`](crate::detect_chords)).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ChordError {
    /// A window or hop of zero.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// The chroma of the audio, which a [`ChordMeter`](crate::ChordMeter) reads
    /// the chords from, was refused.
    #[error(transparent)]
    Chroma(#[from] MeterError),
    /// More frames than one call analyses.
    #[error(transparent)]
    TooManyFrames(#[from] TooManyFrames),
    /// A grid of chroma that is not twelve columns wide.
    #[error("chroma is {columns} columns wide, not 12")]
    NotTwelveClasses {
        /// The grid's width.
        columns: usize,
    },
    /// A sample that is NaN or infinite.
    #[error("input holds a NaN or infinite sample")]
    NonFiniteInput,
    /// A value that must not be negative is.
    #[error("input holds a negative value")]
    NegativeInput,
    /// A chord span whose position is beyond what a sample count holds.
    #[error("a span lies beyond a sample count")]
    SpanBeyondSampleCount,
}

/// What [`align`](crate::align) refuses.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum AlignError {
    /// A band above Nyquist.
    #[error(transparent)]
    Nyquist(#[from] AboveNyquist),
    /// More frames than one call analyses.
    #[error(transparent)]
    TooManyFrames(#[from] TooManyFrames),
    /// A refusal of binding the meter that no variant above names.
    #[error(transparent)]
    Meter(Box<MeterError>),
    /// Input shorter than the algorithm's minimum.
    #[error("need at least {needed} samples, got {got}")]
    InsufficientInput {
        /// The algorithm's minimum, in [`Samples`].
        needed: Samples,
        /// What the caller supplied, in [`Samples`].
        got: Samples,
    },
    /// A sample that is NaN or infinite.
    #[error("input holds a NaN or infinite sample")]
    NonFiniteInput,
    /// Nothing to correlate: silent input, or a band holding no spectrum bin.
    #[error("no signal to correlate: silent input or an empty band")]
    NoSignal,
}

/// What building a meter ([`Measure::measure`](crate::Measure::measure)) or pushing a chunk ([`StreamMeter::push`](crate::StreamMeter::push)) refuses, and the batch calls over them.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum MeterError {
    /// The shared STFT infrastructure refused a grid.
    #[error(transparent)]
    Dsp(#[from] tutti_dsp::StftError),
    /// A window or hop the meter refuses.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// A band above Nyquist.
    #[error(transparent)]
    Nyquist(#[from] AboveNyquist),
    /// More frames than one call analyses.
    #[error(transparent)]
    TooManyFrames(#[from] TooManyFrames),
    /// What a note analysis refuses at the rate it is bound to
    /// ([`NoteConfig`](crate::NoteConfig)).
    #[error(transparent)]
    Note(#[from] NoteError),
    /// What a pitch track refuses at the rate it is bound to
    /// ([`PyinConfig`](crate::PyinConfig)).
    #[error(transparent)]
    Pitch(#[from] PitchError),
    /// The reference of an alignment is at another sample rate than the take:
    /// nothing resamples.
    #[error("the reference is at {reference}, and the take at {take}")]
    ReferenceRate {
        /// The reference's rate.
        reference: SampleRate,
        /// The take's rate.
        take: SampleRate,
    },
    /// The reference of an alignment cannot be folded to one channel.
    #[error("the reference cannot be folded to mono: {0}")]
    ReferenceFold(#[from] tutti_buffers::FoldError),
    /// A waveform block of zero samples.
    #[error("block size is zero: a block would summarize nothing")]
    ZeroBlock,
    /// A band the call cannot hold.
    #[error(transparent)]
    EmptyFrequencyRange(#[from] EmptyFrequencyRange),
    /// A spectrum band that holds no bin: the window is too short for it.
    #[error(
        "band {band} ({lower}..{upper}) holds no bin: a window of {window} samples \
         holds every band"
    )]
    EmptyBand {
        /// The first band with no bin, counting from the lowest.
        band: usize,
        /// Its lower edge, in [`Hz`].
        lower: Hz,
        /// Its upper edge, in [`Hz`].
        upper: Hz,
        /// A window, in [`Samples`], at least as wide as the narrowest band
        /// needs, so that every band holds a bin.
        window: Samples,
    },
    /// A sample rate outside the range an analysis is built for.
    #[error("sample rate {rate} is outside {min}..={max}")]
    SampleRateOutOfRange {
        /// The rate refused.
        rate: SampleRate,
        /// The lowest accepted rate.
        min: SampleRate,
        /// The highest accepted rate.
        max: SampleRate,
    },
    /// Input shorter than the algorithm's minimum.
    #[error("need at least {needed} samples, got {got}")]
    InsufficientInput {
        /// The algorithm's minimum, in [`Samples`].
        needed: Samples,
        /// What the caller supplied, in [`Samples`].
        got: Samples,
    },
    /// More spectrum bands than are accepted.
    #[error("{bands} bands is more than {max}")]
    TooManyBands {
        /// The bands the banding makes.
        bands: usize,
        /// The most accepted.
        max: usize,
    },
    /// A spectrogram of more cells than are accepted.
    #[error("a spectrogram of {cells} cells is more than {max}")]
    SpectrogramTooLarge {
        /// Columns times outputs.
        cells: usize,
        /// The most accepted.
        max: usize,
    },
    /// A peak hold of more frames than the counter holds.
    #[error("a peak hold of {frames} frames is more than {max}")]
    PeakHoldTooLong {
        /// The hold, in frames at the hop in use.
        frames: usize,
        /// The most accepted.
        max: usize,
    },
    /// A chunk whose layout is not the config's.
    #[error("chunk layout {chunk:?} differs from the config's {config:?}")]
    ChunkLayoutMismatch {
        /// The chunk's layout.
        chunk: ChannelLayout,
        /// The config's layout.
        config: ChannelLayout,
    },
    /// An ambisonic topology has no speaker angles to weigh loudness by.
    #[error(
        "ambisonic input has no speaker placement to weigh loudness by: decode it to speakers first"
    )]
    AmbisonicLoudness,
    /// Speakers that are not a known bed's have no angles to weigh loudness by.
    #[error(
        "{width:?} speakers are not a known bed, so their placement is unknown and loudness cannot be weighted"
    )]
    UnplacedLoudnessChannels {
        /// The channel count of the refused topology.
        width: ChannelLayout,
    },
    /// The loudness meter could not be built.
    #[error("the loudness meter could not be built")]
    MeterUnavailable,
    /// The loudness meter refused the frames fed to it.
    #[error("the loudness meter rejected the frames")]
    MeterRejectedFrames,
    /// A sample that is NaN or infinite.
    #[error("input holds a NaN or infinite sample")]
    NonFiniteInput,
}

impl From<tutti_dsp::ChannelWeightError> for MeterError {
    fn from(e: tutti_dsp::ChannelWeightError) -> Self {
        match e {
            tutti_dsp::ChannelWeightError::Ambisonic => Self::AmbisonicLoudness,
            tutti_dsp::ChannelWeightError::Unplaced { width } => {
                Self::UnplacedLoudnessChannels { width }
            }
        }
    }
}

/// What a one-shot call gets of a meter's refusal: the variant the family names,
/// or `Meter` for one it has no name for.
macro_rules! from_meter_error {
    ($family:ident { $($pattern:pat => $value:expr),* $(,)? }) => {
        impl From<MeterError> for $family {
            fn from(error: MeterError) -> Self {
                match error {
                    $($pattern => $value,)*
                    other => Self::Meter(Box::new(other)),
                }
            }
        }
    };
}

from_meter_error!(NoteError {
    MeterError::Note(e) => e,
    MeterError::Pitch(e) => Self::Pitch(e),
    MeterError::Framing(e) => Self::Framing(e),
    MeterError::Nyquist(e) => Self::Pitch(PitchError::Nyquist(e)),
    MeterError::TooManyFrames(e) => Self::TooManyFrames(e),
    MeterError::NonFiniteInput => Self::NonFiniteInput,
});

from_meter_error!(PitchError {
    MeterError::Pitch(e) => e,
    MeterError::Nyquist(e) => Self::Nyquist(e),
    MeterError::TooManyFrames(e) => Self::TooManyFrames(e),
    MeterError::NonFiniteInput => Self::NonFiniteInput,
});

from_meter_error!(TempoEstimateError {
    MeterError::Framing(e) => Self::Framing(e),
    MeterError::TooManyFrames(e) => Self::TooManyFrames(e),
    MeterError::NonFiniteInput => Self::NonFiniteInput,
});

from_meter_error!(AlignError {
    MeterError::Nyquist(e) => Self::Nyquist(e),
    MeterError::TooManyFrames(e) => Self::TooManyFrames(e),
    MeterError::NonFiniteInput => Self::NonFiniteInput,
});
