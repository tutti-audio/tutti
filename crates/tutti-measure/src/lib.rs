#![doc = include_str!("../README.md")]
//! Audio measurement over [`tutti_buffers`] views: waveform blocks,
//! onsets, pitch, EBU R128 loudness, stereo correlation, spectra, spectral shape,
//! silence regions, vectorscope points, tempo, beats, key, chords and the short-time
//! Fourier transform. These are measurements, not processors: they read audio and
//! return numbers. The processors (filters, dynamics, convolution, delay, reverb) and
//! the CPU-independent FFT these run on are in `tutti-dsp`.
//!
//! Use it for offline or background analysis of audio you already hold as
//! samples: drawing a waveform, finding transients, tracking pitch, measuring
//! loudness or checking a stereo image. The algorithms have no opinion about
//! where the results go:
//!
//! - `summarize` — min/max/RMS waveform blocks for a timeline, **per channel**.
//!   Folding to one series is `PeakBlocks::to_mono`, a caller's choice rather than
//!   this crate's default.
//! - `detect_onsets` — onset detection over four selectable detection functions.
//! - `yin` — monophonic pitch estimation (de Cheveigné and Kawahara, 2002).
//! - `pyin_track` — pitch tracking under a probabilistic threshold (Mauch and Dixon, 2014).
//! - `correlate` — inter-channel phase correlation and stereo image.
//! - `measure_loudness` — EBU R128 integrated loudness, loudness range and true
//!   peak.
//! - `WindowedLoudnessConfig` — momentary (400 ms) and short-term (3 s)
//!   loudness and true peak of a running stream, after every chunk.
//! - `SpectrumConfig` — a streaming spectrum: power in dB per bin, or per
//!   third-octave or log band, with smoothing, peak hold and a rolling
//!   spectrogram.
//! - `spectral_features` — centroid, flatness and rolloff of every STFT frame.
//! - `mfcc` — mel-frequency cepstral coefficients of every STFT frame.
//! - `strip_silence` — the sample ranges that hold signal, with a minimum silence
//!   length and padding.
//! - `vectorscope_points` (`VectorscopeConfig` for blocks) — a stereo pair
//!   as (side, mid) points for a goniometer.
//! - `cross_correlate` — FFT cross-correlation of two buffers at every lag.
//! - `align` — GCC-PHAT delay between two takes: lag, sub-sample offset, confidence.
//! - `estimate_tempo` / `track_beats` — tempo (BPM, confidence, octave alternatives) by
//!   autocorrelation under a log-Gaussian prior, and beat positions by dynamic
//!   programming (Ellis 2007); `_from_novelty` forms take your own detection curve.
//! - `transcribe` — monophonic audio to notes: a pitch (`Note` plus a cents bend),
//!   a span in samples and a level, split on onsets and pitch changes;
//!   `notes_from_track` takes your own pitch track.
//! - `chroma` / `ChromaConfig` — harmonic pitch class profile (Gomez
//!   2006) of spectral peaks, with an estimated tuning in cents; `estimate_key`
//!   — Krumhansl-Schmuckler key with a confidence and a runner-up;
//!   `detect_chords` — major and minor triads smoothed by Viterbi, as spans in
//!   samples.
//! - `stft` / `istft_transform` — the short-time Fourier transform, in three result
//!   types so invertibility is a compile-time question.
//!
//! ## Scope
//!
//! - **No graph, no framework, no ECS.** Nothing here is a graph node or knows
//!   about one.
//! - **No threading and no scheduling.** "Live" is a property of a call site, never
//!   of an algorithm, so nothing here is named for it. A host that wants these
//!   results on a background thread owns that plumbing itself.
//! - **No display.** These return numbers, not pixels.
//!
//! ## Configs, and meters where audio arrives in chunks
//!
//! Every analysis takes a **config**: a value that describes the analysis and
//! nothing about the audio. It holds thresholds, bands in hertz, durations in
//! seconds and a [`Framing`] in samples, never a sample rate or a channel width
//! the audio carries. The call that takes the audio binds the rate, derives the
//! geometry and checks what depends on two settings together (a band against
//! Nyquist, a window against its hop), answering with a named [`Error`].
//!
//! Each analysis has the same two terminals:
//!
//! - **one-shot**, like scipy: `x(&config, view, rate)` over a whole buffer;
//! - **streaming**: the config is a [`Measure`] and `config.measure(context)`
//!   makes its meter, which is a [`StreamMeter`]: `meter.push(view, &mut out)` takes
//!   the audio in chunks of any length and extends `out` with the items each
//!   chunk completes, and `meter.finish()` reads the [`Output`](StreamMeter::Output)
//!   of the whole run. `meter.reset()` starts over. Import the two traits to
//!   call them; a consumer that is generic over `A: Measure<S>` takes any analysis
//!   (a [`StreamMeter`] is not object-safe).
//!
//! The one-shot feeds a meter the whole buffer, so the two cannot drift, with
//! one stated exception: `chroma` under `TuningReference::Estimate` finds the tuning in a
//! first pass over the whole buffer, which a streaming meter cannot do. A meter
//! changes only through `push` and `reset`, counts the frames pushed (a push
//! takes no position) and owns its transform scratch. `push` returns a `Result`
//! exactly where the chunk can be refused (a NaN, a width that is not the
//! meter's). A config is a `Measure<S>` for every sample format `S`, and its
//! meter reads views of `S`, converting block by block on the stack;
//! `Measure::<i16>::measure(&config, context)` names the format when a push
//! does not fix it.
//!
//! Every analysis reads its input in whatever format the caller holds and runs
//! at `f64`: spectra, statistics and readings are `f64`. A reading is never
//! narrowed on the way out. The transform scratch (`tutti_dsp::FftScratch<f64>`)
//! is never an argument: a meter owns its own, and a one-shot makes one for the
//! call.
//!
//! | Analysis | Config | One-shot | Meter | Context | Input | Item | Output | Audio thread |
//! |---|---|---|---|---|---|---|---|---|
//! | silence | [`SilenceConfig`] | [`strip_silence`] | [`SilenceMeter`] | `SampleRate` | `Mono` | `()` | `Vec<Region>` | no: grows per region |
//! | onsets | [`OnsetConfig`] | [`detect_onsets`] | [`OnsetMeter`] | `SampleRate` | `Mono` | [`Novelty`], a frame's novelty | `Vec<Onset>` | no: grows per frame |
//! | spectrum | [`SpectrumConfig`] | none | [`SpectrumMeter`] | `SampleRate` | `Mono` | `()` per analysed frame | `Vec<Db>`, the levels | no: a transform per frame, but no allocation |
//! | chroma | [`ChromaConfig`] | [`chroma()`] | [`ChromaMeter`] | `SampleRate` | `Mono` | [`ChromaFrame`], a frame's chroma | [`ChromaSummary`] | no: a transform per frame, no allocation |
//! | loudness | [`LoudnessConfig`] | [`measure_loudness`] | [`LoudnessMeter`] | `SampleRate` | `Interleaved` | `()` | [`Loudness`] | no: grows with the run |
//! | windowed loudness | [`WindowedLoudnessConfig`] | none | [`WindowedMeter`] | `SampleRate` | `Interleaved` | [`WindowedReading`] per push | the last [`WindowedReading`] | no: reads 3 s per push, no allocation |
//! | true peak | [`TruePeakConfig`] | [`measure_true_peak`] | [`TruePeakMeter`] | `(SampleRate, ChannelLayout)` | `Interleaved` | `()` | dBTP as `Db` | no: the meter's own buffers grow, and a push converts on a fixed scratch |
//! | waveform blocks | [`PeakConfig`] | [`summarize`] | [`PeakMeter`] | `ChannelLayout` | `Interleaved` | [`ChannelBlock`], a whole block of a channel | [`PeakBlocks`] | no: grows per block |
//! | vectorscope | [`VectorscopeConfig`] | [`vectorscope_points`] | [`VectorscopeMeter`] | `()` | `StereoPlanes` | [`ScopePoint`] | `()` | no: grows the caller's collection |
//!
//! [`StereoSmoother`] (of a [`Ballistics`](tutti_units::Ballistics)) is not a [`StreamMeter`]: it is a display
//! filter over stereo level readings, with no stream behind it. It allocates
//! nothing and costs a few flops, so it is fit for the audio thread.
//!
//! A config has no width where the width is the audio's, which travels with the
//! view (`Mono`, `Interleaved`, `StereoPlanes`); [`LoudnessConfig`] and
//! [`WindowedLoudnessConfig`] hold a topology because the weight of each speaker
//! is part of the analysis, and check the view's width against it
//! ([`Error::ChunkLayoutMismatch`]).
//!
//! The rest are stateless: `yin`, `correlate`, `cross_correlate`, `align`,
//! `estimate_tempo`, `transcribe` and `spectral_features` are pure functions of
//! their input, and `stft` is batch-only: there is no incremental transform.
//!
//! ## Quick start
//!
//! ```rust
//! use tutti_measure::{
//!     correlate, detect_onsets, summarize, yin, DetectionFunction,
//!     OnsetConfig, PeakConfig, YinConfig,
//! };
//! use tutti_units::{SampleRate, Samples};
//! use tutti_buffers::ChannelLayout;
//! use tutti_buffers::{Interleaved, Mono, StereoPlanes};
//!
//! let sample_rate = SampleRate::SR_44K1;
//! let samples: Vec<f32> = vec![0.0; 44100];
//!
//! // Waveform blocks for display — one series per channel.
//! let blocks = summarize(
//!     &PeakConfig::new(Samples::new(512)),
//!     Interleaved::new(&samples, ChannelLayout::STEREO).unwrap(),
//! )?;
//! let left = blocks.channel(0).expect("stereo has a channel 0");
//! // A meter wants one number per block; a waveform draws both channels.
//! let merged = blocks.to_mono();
//!
//! // Onsets, via spectral flux: the config describes the detection, the call
//! // takes the audio and its rate.
//! let onsets = detect_onsets(
//!     &OnsetConfig::new(DetectionFunction::SpectralFlux),
//!     Mono::new(&samples),
//!     sample_rate,
//! )?;
//!
//! // Pitch. A band above Nyquist is refused here, not silently unvoiced later.
//! let pitch = yin(&YinConfig::new(), Mono::new(&samples), sample_rate)?;
//!
//! // Stereo correlation. The planes are paired once — a length mismatch is
//! // refused here rather than silently truncated inside the measurement.
//! let planes = StereoPlanes::new(&samples, &samples).expect("equal lengths");
//! let reading = correlate(planes);
//! # Ok::<(), tutti_measure::Error>(())
//! ```
//!
//! ## Reading from a running graph
//!
//! Nothing here knows about the graph, so the seam is a tap (`tutti-graph`'s
//! `Editor::open_tap`, or a session's `Graph::taps().open_ports`): the audio thread copies
//! the sources into a ring a lane and a control thread drains them through the
//! `TapReader`. What arrives on this side is an ordinary `&[f32]`, which is the
//! whole reason these algorithms need no engine vocabulary.
//!
//! ```rust
//! use tutti_buffers::MaxBlock;
//! use tutti_measure::correlate;
//! use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, TransportPosition, Unforkable};
//! use tutti_graph::{OutputPort, PortSource};
//! use tutti_buffers::ChannelLayout;
//! use tutti_units::{Beat, Bpm, SampleRate, Samples};
//! use tutti_buffers::{PlanesMut, StereoPlanes};
//! use tutti_graph::NodeKey;
//! use tutti_graph::{TapId, TapOptions};
//!
//! /// A stereo source: 0.5 on the left, -0.5 on the right.
//! struct Tone;
//! impl Node for Tone {
//!     fn shape(&self) -> Shape {
//!         Shape::source_unassigned(ChannelLayout::STEREO)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         io.output(0).fill(0.5);
//!         io.output(1).fill(-0.5);
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! // A tap on the graph's two outputs.
//! let (mut editor, mut executor) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))));
//! editor.insert(NodeKey(1), "tone", Unforkable(Tone));
//! let out = |port| PortSource::Node(OutputPort { node: NodeKey(1), port });
//! editor.spec_mut().topology.set_outputs([out(0), out(1)]);
//! let mut tap = editor.open_tap(TapId::of(1), vec![out(0), out(1)], TapOptions::new(Samples::new(256))).expect("the ports exist");
//! editor.commit().expect("compiles");
//! executor.apply_pending();
//! editor.collect();
//!
//! // The audio-callback side: a block of 2 frames.
//! let (mut l, mut r) = ([0.0f32; 2], [0.0f32; 2]);
//! let transport = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
//! executor.process(2, &transport, &[], &mut [&mut l[..], &mut r[..]]);
//!
//! // The analysis side: two frames, popped a plane to a lane.
//! let (mut left, mut right) = ([0.0f32; 2], [0.0f32; 2]);
//! let mut lanes = [&mut left[..], &mut right[..]];
//! assert_eq!(tap.pop_planes(PlanesMut::new(&mut lanes).expect("two planes")), Ok(Samples::new(2)));
//! let planes = StereoPlanes::new(&left, &right).expect("drained in lockstep");
//!
//! // `Correlation` is a MEASUREMENT type, deliberately distinct from the control
//! // types (`Mix`, `Depth`) despite the coinciding range.
//! let reading = correlate(planes);
//! ```
//!
//! ## Constraint: a bad setting is unwritable, and a bad combination is refused by the call that takes it
//!
//! A config is `Config::new(required..)` and then chained options (`self` in,
//! a bare name, `Self` out); `new` takes only what has no default. An option
//! cannot fail: a setting with one range of its own is a range-checked type
//! (`HarmonicCount`, `Tightness`, `SpectrumFloor`, `NoteSpan`, `FiniteLevel`,
//! `PitchHop`, `OnsetThreshold`, ...) whose `new` refuses a value outside the
//! range, and a setting that spans two values is one type (`FrequencyBand`,
//! `TempoRange`: a low below a high). What depends on the rate, the framing or
//! the input is checked by the call that takes the config: a band against
//! Nyquist, a banding against the window, a tempo against the curve's length.
//! That call answers with a named [`Error`], where a detector that *accepted*
//! an inverted range would report "unvoiced" forever instead, which is a
//! silent wrong answer rather than an error.
//!
//! The one-shot entry points are correspondingly thin on failure. `correlate`
//! is infallible — the pairing check already happened, in
//! `StereoPlanes::new`. `strip_silence` has nothing to refuse.
//! `yin`, `detect_onsets`, `summarize` and `measure_loudness` return `Result`,
//! and refuse before touching a sample.
//!
//! `Result`, not `assert!`, throughout: a caller feeding a short buffer gets an
//! error rather than a panic.
//!
//! ## Allocation
//!
//! The one-shot entry points allocate their results, so call them off the audio
//! thread. A meter's `push` reuses its own buffers across
//! calls. A `SpectrumMeter` allocates nothing once built, and a
//! `WindowedMeter` allocates nothing either, but reads three-second windows
//! on every call, so it belongs on the consumer side of a tap's `TapReader`, not
//! in an audio callback.
//!
//! ## Features
//!
//! - `serde`: `Serialize` and `Deserialize` for [`PeakBlocks`] and [`PeakBlock`]. Off by default.
//!
//! ## Items
//!
//! - Framing: a [`Framing`] is a window and a hop in samples, taken by the
//!   framed configs ([`OnsetConfig`], [`SpectrumConfig`], [`ChromaConfig`],
//!   [`TempoConfig`], [`ChordConfig`]).
//! - Transform: [`stft`], [`stft_magnitude`], [`stft_polar`] and
//!   [`istft_transform`], over a [`StftConfig`] with a
//!   [`CosineWindow`](tutti_dsp::CosineWindow) and a [`SampleRange`] of the
//!   buffer, producing [`Stft`],
//!   [`StftMagnitude`] or [`StftPolar`].
//! - Pitch: [`yin()`] and [`yin_track`] with a [`YinConfig`], returning
//!   [`PitchEstimate`] and [`Pitch`]; [`pyin_track`] with a [`PyinConfig`] decodes
//!   a [`PyinFrame`] track under a Beta prior on the threshold; [`median_filter`] and [`penalize_jumps`]
//!   clean up a track.
//! - Onsets: [`detect_onsets`] with an [`OnsetConfig`] and a
//!   [`DetectionFunction`], or incrementally with an [`OnsetMeter`];
//!   [`OnsetMeter::novelty`] reads the raw detection curve.
//! - Waveform blocks: [`summarize`] with a [`PeakConfig`] into [`PeakBlocks`],
//!   or incrementally with a [`PeakMeter`].
//! - Loudness: [`measure_loudness`] with a [`LoudnessConfig`] into
//!   [`Loudness`], or incrementally with a [`LoudnessMeter`]; the true peak
//!   alone with [`measure_true_peak`] or a [`TruePeakMeter`].
//! - Windowed loudness: a [`WindowedMeter`] from a [`WindowedLoudnessConfig`]
//!   reads momentary (400 ms), short-term (3 s) and true peak as a
//!   [`WindowedReading`] after every chunk.
//! - Spectrum: a [`SpectrumMeter`] from a [`SpectrumConfig`] streams power in dB
//!   per bin or [`Banding`] band, with [`Smoothing`], held peaks and a rolling
//!   spectrogram.
//! - Stereo: [`correlate`] into a [`StereoReading`], smoothed for a meter by a
//!   [`StereoSmoother`].
//! - Spectral shape: [`spectral_features`] over every frame of a
//!   [`StftMagnitude`], into [`SpectralFeatures`]; the per-spectrum kernels
//!   are [`tutti_dsp`]'s (`spectral_centroid`, `spectral_flatness`,
//!   `spectral_rolloff`).
//! - Cepstrum: [`mfcc`] from an [`MfccConfig`] over every frame of a
//!   [`StftMagnitude`], into a `frames x coefficients` `Grid`; the per-spectrum
//!   kernel is [`tutti_dsp`]'s `Mfcc`.
//! - Strip silence: [`strip_silence`] with a [`SilenceConfig`] into
//!   [`Region`]s, or incrementally with a [`SilenceMeter`].
//! - Vectorscope: [`vectorscope_points`] turns a stereo pair into
//!   [`ScopePoint`]s (side, mid), or block by block with a [`VectorscopeMeter`].
//! - Cross-correlation: [`cross_correlate`] into a [`CrossCorrelation`], read
//!   at a lag or for its [`CorrelationPeak`].
//! - Take alignment: [`align()`] with an [`AlignConfig`] finds an [`Alignment`]
//!   (whole-sample lag, sub-sample offset, confidence) by GCC-PHAT.
//! - Tempo: [`estimate_tempo()`] with a [`TempoConfig`] finds a [`Tempo`] (BPM,
//!   confidence, runner-up octaves); [`track_beats()`] places a [`BeatGrid`] by
//!   dynamic programming; the `_from_novelty` forms take your own curve.
//! - Notes: [`transcribe`] with a [`NoteConfig`] turns a mono buffer into
//!   [`NoteEvent`]s (note, cents bend, span, level); [`notes_from_track`]
//!   takes your own pitch track, [`frame_levels`] and onsets;
//!   [`VelocityRange`] maps a level to a MIDI velocity.
//! - Harmony: [`chroma()`] with a [`ChromaConfig`] folds spectral peaks into
//!   twelve pitch classes and estimates the [`TuningReference`], or incrementally with a
//!   [`ChromaMeter`]; [`estimate_key`] finds a [`Key`] by
//!   Krumhansl-Schmuckler over a [`KeyProfile`]; [`detect_chords`] with a
//!   [`ChordConfig`] smooths triads into [`ChordSpan`]s by Viterbi.
//!
//! In the source, `meters/` holds the real-time readings (the [`Measure`]
//! configs and their meters) and `analyses/` the offline analyses; every public
//! item is exported from this root.
#![forbid(unsafe_code)]

mod analyses;
mod convert;
mod error;
mod framing;
mod level;
mod measure;
mod meters;
#[cfg(test)]
mod range_contracts;
#[cfg(test)]
mod test_rng;

pub use analyses::align::{AlignConfig, Alignment, align};
pub use analyses::chord::{
    ChordConfig, ChordQuality, ChordSpan, SelfTransition, SelfTransitionError, Sharpness,
    SharpnessError, detect_chords,
};
pub use analyses::features::{SpectralFeatures, spectral_features};
pub use analyses::key::{Key, KeyEstimate, KeyProfile, Mode, estimate_key, estimate_key_from_mean};
pub use analyses::mfcc::{MfccConfig, mfcc};
pub use analyses::notes::{
    NoteConfig, NoteEvent, NoteSpan, NoteSpanError, PitchHop, PitchHopError, PitchSplit,
    PitchSplitError, VelocityRange, frame_levels, notes_from_track, transcribe,
};
pub use analyses::pyin::{
    PyinConfig, PyinFrame, PyinSwitch, PyinSwitchError, PyinThresholdMean, PyinThresholdMeanError,
    PyinTransitionWidth, PyinTransitionWidthError, pyin_track,
};
pub use analyses::tempo::{
    Alternatives, AlternativesError, BeatGrid, PriorSpread, PriorSpreadError, Tempo,
    TempoCandidate, TempoConfig, TempoRange, Tightness, TightnessError, estimate_tempo,
    estimate_tempo_from_novelty, track_beats, track_beats_from_novelty,
};
pub use analyses::transform::{
    HopPolicy, NormalizedMagnitudes, RawMagnitudes, SampleRange, Stft, StftConfig, StftMagnitude,
    StftPolar, istft as istft_transform, stft, stft_magnitude, stft_polar,
};
pub use analyses::xcorr::{CorrelationPeak, CrossCorrelation, cross_correlate};
pub use analyses::yin::{
    Pitch, PitchEstimate, YinConfig, YinThreshold, YinThresholdError, median_filter,
    penalize_jumps, yin, yin_track,
};
pub use error::{Error, Result};
pub use framing::Framing;
#[cfg(test)]
pub(crate) use measure::fold_uneven;
pub use measure::{Measure, StreamMeter};
pub use meters::chroma::{
    Chroma, ChromaConfig, ChromaFrame, ChromaMeter, ChromaSummary, FrequencyBand, HarmonicCount,
    HarmonicCountError, HarmonicDecay, HarmonicDecayError, PeakFloor, PeakFloorError, TuningOffset,
    TuningOffsetError, TuningReference, chroma,
};
pub use meters::loudness::{
    Loudness, LoudnessConfig, LoudnessMeter, TruePeakConfig, TruePeakMeter, WindowedLoudnessConfig,
    WindowedMeter, WindowedReading, measure_loudness, measure_true_peak,
};
pub use meters::onset::{
    DetectionFunction, Novelty, Onset, OnsetConfig, OnsetGap, OnsetGapError, OnsetMeter,
    OnsetSensitivity, OnsetSensitivityError, OnsetThreshold, OnsetThresholdError, detect_onsets,
    suppress_close_onsets,
};
pub use meters::peaks::{
    ChannelBlock, PeakBlock, PeakBlocks, PeakConfig, PeakMeter, summarize, summarize_block,
};
pub use meters::silence::{
    FiniteLevel, FiniteLevelError, Region, SilenceConfig, SilenceMeter, strip_silence,
};
pub use meters::spectrum::{
    BandCount, BandCountError, Banding, BandsPerOctave, BandsPerOctaveError, PeakDecay,
    PeakDecayError, Smoothing, SpectrumConfig, SpectrumFloor, SpectrumFloorError, SpectrumMeter,
    SpectrumTime, SpectrumTimeError,
};
pub use meters::stereo::{StereoLevels, StereoReading, StereoSmoother, correlate};
pub use meters::vectorscope::{
    ScopePoint, VectorscopeConfig, VectorscopeMeter, vectorscope_points,
};

/// Notes and pitch classes, re-exported from the engine vocabulary: a
/// [`Pitch`] names one, and callers should not need a second import to read it.
pub use tutti_music::{Note, PitchClass};
