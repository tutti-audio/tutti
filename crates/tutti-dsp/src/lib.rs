#![doc = include_str!("../README.md")]
//! Audio signal-processing algorithms over [`tutti_buffers`] views, with a real FFT whose
//! arithmetic does not depend on the CPU.
//!
//! Each algorithm is laid out as its controls (what a caller sets), its model (a [`System`],
//! and an [`Lti`] where it is linear and time-invariant) and its bank (the per-frame math
//! over buffers views). Values are `tutti_units` units.
//!
//! ## Catalogue
//!
//! - **Model** (`model/`): a [`System`] is what every processor answers (its
//!   [`latency`](System::latency) and [`tail`](System::tail), a [`DecayTime`]: how long the response
//!   rings until it is a floor below what it carried ([`TAIL_FLOOR`] by default)), and [`Lti`]
//!   the class of a linear time-invariant one
//!   ([`frequency_response`](Lti::frequency_response),
//!   [`spectral_radius`](Lti::spectral_radius), [`l1_norm`](Lti::l1_norm),
//!   [`hinf_norm`](Lti::hinf_norm), with [`Complex64`]); [`MaxDecay`] is the longest decay
//!   a reverb is built to.
//! - **Filters** (`filter/`): the SVF ([`SvfBank`], [`SvfDesign`], [`SvfModel`], [`SvfType`];
//!   Simper's trapezoidal state-variable filter, eight responses), the Moog ladder
//!   ([`LadderBank`], [`LadderDesign`], [`LadderBlend`], [`LadderModel`], [`LadderType`]) and the
//!   Linkwitz-Riley crossover ([`CrossoverBank`], [`CrossoverModel`]; a 2 to 6 band
//!   4th-order split), each one filter per channel over buffers views with its
//!   coefficients a [`Ramp`] the caller retargets; the envelope filter ([`EnvelopeFilterBank`]:
//!   an SVF whose cutoff follows the input's level); [`SimdLanes`], the arithmetic their steps
//!   run in, and the [`Biquad`] section they share. The state runs in `f32` or `f64`, and
//!   its transcendentals call `libm`, so results do not depend on the platform's C math
//!   library.
//! - **Cascaded sections** (`sos/`): [`Sos`], a list of [`Biquad`]s, an [`Lti`] whose
//!   [`tail`](System::tail) is the whole cascade's; the closed-form [`Butterworth`],
//!   [`ChebyshevI`], [`ChebyshevII`], [`Elliptic`] and [`LinkwitzRiley`] designs (up to 96 dB
//!   per octave of asymptotic slope), the [`MatchedBell`] peaking
//!   equalizer, the [`Tilt`] shelf, the Audio EQ [`Cookbook`] filters, the [`GraphicBands`]
//!   graphic equalizer design, the BS.1770 [`KWeighting`] and [`SosBank`], which runs
//!   one as state-variable sections over [`SosCoefficients`].
//! - **FIR design** (`fir/`): [`FirDesign`] describes a Kaiser windowed-sinc filter (a
//!   [`FirResponse`], a transition and an attenuation) and designs it at a rate into a
//!   [`Fir`], failing with a [`FirError`]; [`LinearPhaseDesign`] takes the product of
//!   [`Lti`] bands' magnitudes as a target and designs it as a linear-phase [`Fir`];
//!   [`kaiser_beta`] is the window's parameter.
//! - **Oversampling** (`oversample/`): [`Oversampling`] describes a cascade of linear-phase
//!   half-band 2x stages (an [`OversamplingFactor`], an attenuation and a [`Passband`]);
//!   [`plan`](Oversampling::plan) designs it at a rate into an [`OversamplingPlan`], whose
//!   [`state`](OversamplingPlan::state) is one channel's [`Oversampler`]. Its round trip is
//!   a whole number of base frames, [`latency`](OversamplingPlan::latency).
//! - **Dynamics** (`dynamics/`): the gain curves ([`LimiterCurve`] and the `compute_*`
//!   functions, [`amplitude_to_db`]); the compressor ([`CompressorBank`], its threshold a
//!   control signal, its makeup a [`Ramp`] over [`GAIN_RAMP`]) and the noise gate ([`GateBank`], its
//!   range ramping likewise), both reading a [`KeySource`] (a side-chain bus or the signal
//!   itself) through a [`KeyFilter`] if built with one; the lookahead limiter ([`LimiterBank`],
//!   its threshold and ceiling [`LimiterSignals`], its lookahead its latency); the brickwall
//!   clipper ([`BrickwallBank`], its ceiling a per-frame control signal); the transient shaper
//!   ([`TransientBank`]: one gain from a fast and a slow one-pole envelope); the loudness
//!   leveller ([`LevellerBank`]: one gain moving the BS.1770 short-term loudness, over the
//!   [`ChannelWeight`] of each channel, to a target); the dynamic EQ
//!   band ([`DynamicEqBank`]: an SVF bell whose gain follows the level of a band-passed key
//!   through the compressor's static curve, by a [`DynamicMode`]); and the multiband
//!   compressor ([`MultibandBank`]: a crossover tree per channel and a [`CompressorBank`]
//!   per band, each band's makeup, bypass and solo ramping over [`GAIN_RAMP`]).
//! - **Convolution** (`convolver/`): [`Convolver`], a uniform or non-uniform partitioned FFT
//!   convolution over an IR's shared, read-only [`IrSpectra`], and [`MatrixConvolver`] for
//!   a matrix of impulse responses.
//! - **Delay and modulation** (`delay/`, `modulation/`): [`DelayLine`], a fractional-read
//!   ring with an [`InterpolationMode`]; the feedback delay ([`DelayBank`] over a
//!   [`DelayLayout`]: a [`MaxDelay`], its cross-feedback a checked [`FeedbackRouting`], its time and
//!   feedback [`Ramp`]s and the per-frame [`DelaySignals`]); the chorus and flanger
//!   ([`ModDelayBank`] over a [`ModDelayLayout`]: its [`Sweep`] and [`Stagger`] checked, a
//!   [`Ramp`] per channel's delay, the LFO the caller's on the stream's clock); the phaser
//!   ([`PhaserChain`]: its [`Stages`] and [`SweepRange`] checked, a [`Ramp`] per channel's
//!   coefficient, [`sweep_hz`] and [`allpass_complement`] the pure pieces the sweep is solved
//!   from); the vibrato preset ([`ModDelayControls::vibrato`], a pitch swing in cents mapped to
//!   a depth by its closed form); the comb and all-pass delay filters ([`CombFilter`] and
//!   [`CombBank`] over a [`CombForm`], [`AllPassFilter`] in Schroeder's lattice form or
//!   Freeverb's); and the frequency shifter ([`FrequencyShifterBank`]: a Hilbert network of two
//!   all-pass chains, [`QUADRATURE_ERROR`] over [`quadrature_band`], and single-sideband
//!   modulation by a signed shift).
//! - **Reverb** (`reverb/`): the model, a [`Network`] of [`Stage`]s with analytic queries
//!   ([`Network::rt60`], [`tail`](System::tail), [`is_stable`](Lti::is_stable), the
//!   densities); [`FreeverbBank`] (Freeverb's combs and all-passes), [`FdnBank`] (the
//!   feedback delay network, over an [`FdnLayout`]) and
//!   [`PlateBank`] (Dattorro's plate), [`SpringReverb`] (Valimaki, Parker and Abel's spring:
//!   stretched all-pass cascades in a loop with a delay and a low-pass), their parameters [`Ramp`]s in [`FreeverbRamps`],
//!   [`FdnRamps`] and [`PlateRamps`]. [`rt60_from_gain`] is the RT60 of a loop of a
//!   given gain.
//! - **Waveshaping** (`waveshape.rs`): [`ShapeKind`], seven memoryless curves, and
//!   [`WaveshaperBank`], which runs one over a block at a ramping or per-frame drive, with
//!   [`Antialiasing`] of the first or second order.
//! - **Spatial** (`spatial/`): VBAP ([`SpeakerLayout`], Pulkki's pairs and triplets and the
//!   gain solve over them, [`SpeakerGeometryError`]; [`VbapKernel`] and [`VbapGains`], one
//!   direction's gains and one channel rendered along a ramp between two) and the HRTF
//!   binaural convolver ([`HrtfBinaural`]); ambisonics in AmbiX ([`Harmonics`] and
//!   [`AmbisonicEncoder`], [`Rotation`] and [`AmbisonicRotator`], [`AmbisonicDecoder`]).
//! - **FFT** (`fft/`): [`RealFft`], a real-input FFT whose arithmetic does not depend on the
//!   CPU (the convolver also runs it in steps), and [`FftScratch`], reusable plans and
//!   buffers; [`Correlator`] cross-correlates through it. It is rustfft's *scalar* planner
//!   under realfft's even-length real passes (an odd size runs a full-length complex
//!   transform), planned lazily per scratch on the cold path. A spectrum, a pitch or an onset
//!   is bit-identical on every CPU on the same OS, libm and rustfft version (the twiddles call
//!   the C library's `sin` and `cos`, so not across libms). Values cross as
//!   [`Complex32`] and [`Complex64`].
//! - **STFT vocabulary** (`stft/`): [`CosineWindow`] and the [`Window`] trait,
//!   [`StftGeometry`] (a [`WindowLength`], a [`HopSize`], shape and rate), the
//!   [`ColaGeometry`] an inverse accepts, and [`Grid`] with its typed indices; [`StftError`] is
//!   what a [`Grid`] or a [`ColaGeometry`] refuses with. [`PhaseVocoder`] stretches and
//!   shifts the pitch of any width on a [`ColaGeometry`], one phase solution for all channels,
//!   pulled by its output (`input_needed`, then `process`) under [`PhaseVocoderControls`].
//! - **Resampling** (`resample/`): [`WindowedSinc`] and [`StreamingResampler`] (made from a
//!   [`StreamingResamplerConfig`], sized by a [`ChunkSize`], failing with a [`ResampleError`]).
//! - **Dither** (`dither/`): [`DitherNoise`] ([`DitherShape`]) and [`DitherSink`], the
//!   `tutti_buffers::Sink` adapter that dithers on its way to a fixed-point format, with
//!   [`NoiseShaping`] as its optional error feedback.
//! - **Noise** (`noise.rs`): [`Noise`], a seeded white, pink (Kellet's filter) or brown
//!   (leaky integrator) source ([`NoiseColour`]).
//! - **Oscillator** (`oscillator.rs`): [`Oscillator`], a sine, or a saw, square or triangle
//!   band-limited by PolyBLEP and PolyBLAMP ([`OscillatorWave`]).
//! - **Stereo and utility** (`polarity.rs`, `stereo.rs`, `pan.rs`): [`Polarity`], the
//!   channels a block inverts; [`MidSide`] and [`WidthBank`] (the side scaled by a ramped
//!   [`StereoWidth`](tutti_units::StereoWidth)); [`PanLaw`] and [`pan_gains`].
//! - **Tremolo, auto-pan and ring modulation** (`tremolo.rs`, `ring_mod.rs`): [`Tremolo`]
//!   and [`AutoPan`], an [`Oscillator`] scaling the level or placing it by [`pan_gains`];
//!   [`RingModulator`], the input times a sine carrier.
//! - **Distance** (`distance.rs`): [`Distance`], a source at a [`Metres`](tutti_units::Metres)
//!   distance as the delay `r / c`, the level `r_ref / r` and optional air absorption, whose
//!   moving delay is the Doppler shift ([`DistanceSpec`], [`DistanceControls`]).
//! - **Bit crusher and DC blocker** (`bit_crush.rs`, `dc_block.rs`): [`BitCrusher`], a
//!   sample-and-hold every [`hold_frames`] frames, then [`quantize`] to a bit depth;
//!   [`DcBlocker`], a first-order high-pass with its pole `exp(-2 pi fc / fs)`.
//! - **Spectral features and onset novelty** (`spectral.rs`, `novelty.rs`): the shape of one
//!   magnitude spectrum ([`spectral_centroid`], [`spectral_flatness`], [`spectral_rolloff`]) and
//!   how much a frame differs from the one before ([`spectral_flux`],
//!   [`high_frequency_content`], [`complex_domain_deviation`], [`spectral_energy`]), and
//!   [`bin_magnitude`], the modulus they take of a bin.
//! - **Mel bank, DCT-II and MFCC** (`mel.rs`, `dct.rs`, `mfcc.rs`): [`MelFilterbank`]
//!   (from a [`MelConfig`], in a [`MelScale`] and [`MelNorm`]), the orthonormal [`Dct2`], and
//!   [`Mfcc`], the cepstral coefficients of one magnitude spectrum.
//! - **Sequence decoding** (`viterbi.rs`, `beat.rs`, `sequence.rs`): [`sticky_viterbi`], the best
//!   state path when every switch costs the same; [`track_beat_frames`], beat placement by
//!   dynamic programming over a novelty curve (Ellis, 2007); the [`median`] of a set and the
//!   [`local_maxima`] of a curve.
//! - **Band edges** (`bands.rs`): [`log_band_edges`] and [`octave_band_edges`], the edges of
//!   bands with equal ratios between neighbours.
//! - **Control and small kernels**: [`Ramp`], a value that moves to its target in steps
//!   (with [`Lerp`] and [`Steps`]); [`smooth_toward`], the one-pole step; [`COEFF_INTERVAL`] and
//!   [`CHANNEL_GROUP`], the solve interval and channel grouping of the filter banks; [`lerp`],
//!   [`cubic_hermite`] and [`parabolic_peak`]; [`rms`].
//!
//! ## Scope
//!
//! - **No graph, no framework, no ECS.** Nothing here is a graph node or knows about one.
//! - **No threading and no scheduling.** A host that wants these results on a background
//!   thread owns that plumbing itself.
//!
//! ## Import paths
//!
//! Every public item is re-exported at the crate root; import from there
//! (`tutti_dsp::RealFft`). No module is public.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod all_pass;
mod bands;
mod beat;
mod bit_crush;
mod channel_group;
mod comb;
mod control_rate;
mod convolver;
mod dc_block;
mod dct;
mod delay;
mod design;
mod distance;
mod dither;
mod dynamics;
mod fft;
mod filter;
mod fir;
mod follower;
mod frequency_shifter;
mod interp;
mod level;
mod loudness;
mod modulation;
mod noise;
mod novelty;
mod onepole;
mod oscillator;
mod oversample;
mod pan;
mod polarity;
mod position;
mod ramp;
mod ring_mod;
mod sample_convert;
mod stereo;
mod tremolo;
mod waveshape;

mod mel;
mod mfcc;
mod model;
mod resample;
mod reverb;
mod sequence;
mod smoother;
mod sos;
mod spatial;
mod spectral;
mod stft;
#[cfg(test)]
mod testing;
mod viterbi;

pub use all_pass::AllPassFilter;
pub use bands::{log_band_edges, octave_band_edges};
pub use beat::track_beat_frames;
pub use bit_crush::{BitCrusher, BitCrusherControls, hold_frames, quantize};
pub use channel_group::CHANNEL_GROUP;
pub use comb::{CombBank, CombControls, CombFilter, CombForm};
pub use control_rate::COEFF_INTERVAL;
pub use convolver::{
    Convolver, IrSpectra, MatrixConvolver, MatrixConvolverConfig, MatrixConvolverError,
};
pub use dc_block::{DC_CORNER, DcBlocker, DcBlockerControls, MIN_DC_CORNER};
pub use dct::Dct2;
pub(crate) use delay::delay_step;
pub use delay::{
    ChannelTimes, ChannelTimesError, DELAY_SLEW, DelayBank, DelayControls, DelayLayout,
    DelayLayoutBuilder, DelayLayoutError, DelayLine, DelayRamps, DelaySignals, DelayTiming,
    FeedbackRouting, FeedbackRoutingBuilder, FeedbackRoutingError, InterpolationMode,
    MAX_DELAY_CHANNELS, MaxDelay, MaxDelayError, NoteTimes, NoteTimesError,
};
pub use distance::{DISTANCE_GLIDE, Distance, DistanceControls, DistanceSpec};
pub use dither::shaping::NoiseShaping;
pub use dither::sink::DitherSink;
pub use dither::{DitherNoise, DitherShape};
pub use dynamics::{
    BandReductions, BandSettings, BankShape, BrickwallBank, BrickwallControls, BrickwallSignals,
    CompressorBank, CompressorControls, CompressorSignals, DynamicEqBank, DynamicEqControls,
    DynamicMode, GAIN_RAMP, GateBank, GateControls, GateSignals, KeyFilter, KeySource,
    LevellerBank, LevellerControls, LimiterBank, LimiterControls, LimiterCurve, LimiterSignals,
    Lookahead, LookaheadError, MultibandBands, MultibandBandsError, MultibandBank,
    MultibandSettings, MultibandSignals, PeakDetection, TransientBank, TransientControls,
    TruePeakError, amplitude_to_db, compute_compressor_gain_reduction, compute_limiter_gain,
};
pub use fft::{Complex, Complex32, Complex64, Correlator, FftFloat, FftScratch, RealFft};
pub use filter::{
    Biquad, CrossoverBands, CrossoverBandsError, CrossoverBank, CrossoverBlend, CrossoverControls,
    CrossoverError, CrossoverModel, EnvelopeFilterBank, EnvelopeFilterControls, LadderBank,
    LadderBlend, LadderCoefficients, LadderControls, LadderDesign, LadderModel, LadderType,
    MAX_SPLITS, SOLVE_CEILING, SimdLanes, SvfBank, SvfCoefficients, SvfControls, SvfDesign,
    SvfModel, SvfType, fit_to_rate, ladder_step, svf_step,
};
pub use fir::{
    Fir, FirDesign, FirError, FirResponse, LinearPhaseDesign, MAX_ATTENUATION, MAX_GRID,
    kaiser_beta,
};
pub use frequency_shifter::{
    FrequencyShifterBank, FrequencyShifterControls, QUADRATURE_ERROR, quadrature_band,
};
pub use interp::{ParabolicPeak, cubic_hermite, lerp, parabolic_peak};
pub use level::rms;
pub use loudness::{ChannelWeight, ChannelWeightError, ChannelWeights, channel_weights};
pub use mel::{
    MelBandCount, MelBandCountError, MelConfig, MelError, MelFilterbank, MelNorm, MelScale,
};
pub use mfcc::Mfcc;
pub use model::{DecayTime, Lti, MaxDecay, MaxDecayError, System, TAIL_FLOOR};
pub use modulation::allpass::{allpass_complement, sweep_hz};
pub use modulation::mod_delay::{
    MAX_MOD_DELAY_CHANNELS, MIN_RATE, ModDelayBank, ModDelayControls, ModDelayLayout,
    ModDelayLayoutBuilder, SweepDesign,
};
pub use modulation::phaser::{
    MAX_PHASER_CHANNELS, PhaserChain, PhaserControls, Stages, StagesError, SweepRange,
    SweepRangeError,
};
pub use modulation::sweep::{Stagger, StaggerError, Sweep, SweepBuilder, SweepError};
pub use noise::{Noise, NoiseColour};
pub use novelty::{
    bin_magnitude, complex_domain_deviation, high_frequency_content, spectral_energy, spectral_flux,
};
pub use oscillator::{Oscillator, OscillatorStep, OscillatorWave};
pub use oversample::{
    Oversampler, Oversampling, OversamplingFactor, OversamplingPlan, Passband, PassbandError,
};
pub use pan::{PanLaw, pan_gains};
pub use polarity::{ChannelOutOfRange, Polarity};
pub use ramp::{Lerp, Ramp, Steps};
pub use resample::{
    ChunkSize, ResampleError, StreamingResampler, StreamingResamplerConfig, WindowedSinc,
    needs_resampling, resampled_frames,
};
pub use reverb::fdn::bank::{FdnBank, FdnRamps};
pub use reverb::fdn::design::{FdnControls, FdnLayout, FreezeMode, MAX_PRE_DELAY, MIN_DECAY};
pub use reverb::freeverb::bank::{FreeverbBank, FreeverbRamps};
pub use reverb::freeverb::design::FreeverbControls;
pub(crate) use reverb::network::loop_gain;
pub use reverb::network::{
    Absorption, AllPass, AllPassForm, Band, Fdn, FirSummary, Full, Gains, Inline, Line,
    LineModulation, MAX_CHANNELS, MAX_DENSE, MAX_LINES, MAX_STAGES, MAX_TAPS, Matrix, Network,
    NetworkOutputMap, Stage, Tap, Taps, rt60_from_gain,
};
pub use reverb::plate::bank::{PlateBank, PlateRamps};
pub use reverb::plate::design::{MAX_PLATE_DECAY, PlateControls};
pub use reverb::spring::{
    SpringControls, SpringLayout, SpringLength, SpringLengthError, SpringReverb, SpringStages,
    SpringStagesError,
};
pub use ring_mod::{RingModControls, RingModulator};
pub use sequence::{local_maxima, median, median_of_sorted};
pub use smoother::smooth_toward;
pub use sos::{
    Butterworth, ButterworthOrder, ButterworthOrderError, ChebyshevI, ChebyshevII, Cookbook,
    CutResponse, Elliptic, EllipticTolerances, GraphicBands, KWeighting, LinkwitzRiley,
    MAX_GAIN_MAGNITUDE, MAX_SECTIONS, MatchedBell, PassbandRipple, PassbandRippleError, Sos,
    SosBank, SosCoefficients, SosError, StopbandAttenuation, StopbandAttenuationError, Tilt,
};
pub use spatial::{
    AmbisonicDecoder, AmbisonicEncoder, AmbisonicRotator, HRTF_LATENCY, HRTF_STEP,
    HarmonicNormalization, Harmonics, HrirMeasurement, HrirSphere, HrtfBinaural, HrtfBinauralError,
    MAX_PREPARED_RATE, MAX_SPHERE_RATE, MIN_SPHERE_RATE, Rotation, SpeakerGeometryError,
    SpeakerLayout, VbapGains, VbapKernel,
};
pub use spectral::{spectral_centroid, spectral_flatness, spectral_rolloff};
pub use stereo::{MidSide, WidthBank, WidthControls};
pub use stft::error::StftError;
pub use stft::geometry::{
    ColaGeometry, HopSize, HopSizeError, StftGeometry, WindowLength, WindowLengthError,
};
pub use stft::grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use stft::vocoder::{PhaseVocoder, PhaseVocoderControls, StreamCounts};
pub use stft::window::{CosineWindow, Window};
pub use tremolo::{AutoPan, AutoPanControls, Tremolo, TremoloControls};
pub use viterbi::sticky_viterbi;
pub use waveshape::{Antialiasing, ShapeKind, WaveshaperBank, WaveshaperControls};
