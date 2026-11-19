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
//!   [`latency`](System::latency) and [`tail`](System::tail), a [`Ring`] to a floor,
//!   [`TAIL_FLOOR`] by default), and [`Lti`] the class of a linear time-invariant one
//!   ([`frequency_response`](Lti::frequency_response),
//!   [`spectral_radius`](Lti::spectral_radius), [`l1_norm`](Lti::l1_norm),
//!   [`hinf_norm`](Lti::hinf_norm), with [`Complex64`]); [`MaxDecay`] is the longest decay
//!   a reverb is built to.
//! - **Filters** (`filter/`): the SVF ([`SvfBank`], [`SvfCoeffs`], [`SvfModel`], [`SvfType`];
//!   Simper's trapezoidal state-variable filter, eight responses), the Moog ladder
//!   ([`LadderBank`], [`LadderCoeffs`], [`LadderModel`], [`LadderType`]) and the
//!   Linkwitz-Riley crossover ([`CrossoverBank`], [`CrossoverModel`]; a 2 to 6 band
//!   4th-order split), each one filter per channel over buffers views with its
//!   coefficients a [`Glide`] the caller aims; the envelope filter ([`EnvelopeFilterBank`]:
//!   an SVF whose cutoff follows the input's level); [`Lanes`], the arithmetic their steps
//!   run in, and the [`Biquad`] section they share. The state runs in `f32` or `f64`, and
//!   its transcendentals call `libm`, so results do not depend on the platform's C math
//!   library.
//! - **Cascaded sections** (`sos/`): [`Sos`], a list of [`Biquad`]s, an [`Lti`] whose
//!   [`tail`](System::tail) is the whole cascade's; the closed-form [`Butterworth`] and
//!   [`LinkwitzRiley`] designs (6 to 96 dB per octave), the [`MatchedBell`] peaking
//!   equalizer, the [`Tilt`] shelf and [`SosBank`], which runs one as state-variable
//!   sections over [`SosCoefficients`].
//! - **FIR design** (`fir/`): [`FirDesign`] describes a Kaiser windowed-sinc filter (a
//!   [`FirResponse`], a transition and an attenuation) and designs it at a rate into a
//!   [`Fir`], failing with a [`FirError`]; [`kaiser_beta`] is the window's parameter.
//! - **Oversampling** (`oversample/`): [`Oversampling`] describes a cascade of linear-phase
//!   half-band 2x stages (an [`OversamplingFactor`], an attenuation and a [`Passband`]);
//!   [`plan`](Oversampling::plan) designs it at a rate into an [`OversamplingPlan`], whose
//!   [`state`](OversamplingPlan::state) is one channel's [`Oversampler`]. Its round trip is
//!   a whole number of base frames, [`latency`](OversamplingPlan::latency).
//! - **Dynamics** (`dynamics/`): the gain curves ([`LimiterCurve`] and the `compute_*`
//!   functions, [`amplitude_to_db`]); the compressor ([`CompressorBank`], its threshold a
//!   lane, its makeup a [`Glide`] over [`GAIN_GLIDE`]) and the noise gate ([`GateBank`], its
//!   range gliding likewise), both reading a [`KeySource`] (a side-chain bus or the signal
//!   itself) through a [`KeyFilter`] if built with one; the lookahead limiter ([`LimiterBank`],
//!   its threshold and ceiling [`LimiterLanes`], its lookahead its latency); the brickwall
//!   clipper ([`BrickwallBank`], its ceiling a per-frame lane); the transient shaper
//!   ([`TransientBank`]: one gain from a fast and a slow one-pole envelope); the dynamic EQ
//!   band ([`DynamicEqBank`]: an SVF bell whose gain follows the level of a band-passed key
//!   through the compressor's static curve, by a [`DynamicMode`]); and the multiband
//!   compressor ([`MultibandBank`]: a crossover tree per channel and a [`CompressorBank`]
//!   per band, each band's makeup, bypass and solo gliding over [`GAIN_GLIDE`]).
//! - **Convolution** (`convolver/`): [`Convolver`], a uniform or non-uniform partitioned FFT
//!   convolution over an IR's shared, read-only [`IrSpectra`], and [`MatrixConvolver`] for
//!   a matrix of impulse responses.
//! - **Delay and modulation** (`delay/`, `modulation/`): [`DelayLine`], a fractional-read
//!   ring with an [`InterpolationMode`]; the feedback delay ([`DelayBank`] over a
//!   [`DelayLayout`]: a [`MaxDelay`], its cross-feedback a checked [`Routing`], its time and
//!   feedback [`Glide`]s and the per-frame [`DelayLanes`]); the chorus and flanger
//!   ([`ModDelayBank`] over a [`ModDelayLayout`]: its [`Sweep`] and [`Stagger`] checked, a
//!   [`Glide`] per channel's delay, the LFO the caller's on the stream's clock); the phaser
//!   ([`PhaserChain`]: its [`Stages`] and [`SweepRange`] checked, a [`Glide`] per channel's
//!   coefficient, [`sweep_hz`] and [`allpass_complement`] the pure pieces the sweep is solved
//!   from).
//! - **Reverb** (`reverb/`): the model, a [`Network`] of [`Stage`]s with analytic queries
//!   ([`Network::rt60`], [`tail`](System::tail), [`is_stable`](Lti::is_stable), the
//!   densities); [`FreeverbBank`] (Freeverb's combs and all-passes), [`FdnBank`] (the
//!   feedback delay network, over an [`FdnLayout`]) and
//!   [`PlateBank`] (Dattorro's plate), their parameters [`Glide`]s in [`FreeverbGlides`],
//!   [`FdnGlides`] and [`PlateGlides`]. [`rt60_from_gain`] is the RT60 of a loop of a
//!   given gain.
//! - **Waveshaping** (`waveshape.rs`): [`ShapeKind`], six memoryless curves, and
//!   [`WaveshaperBank`], which runs one over a block at a gliding or per-frame drive, with
//!   [`Antialiasing`] of the first or second order.
//! - **Spatial** (`spatial/`): VBAP ([`SpeakerLayout`], Pulkki's pairs and triplets and the
//!   gain solve over them, [`SpeakerGeometryError`]; [`VbapKernel`] and [`VbapGains`], one
//!   direction's gains and one channel rendered along a glide between two) and the HRTF
//!   binaural convolver ([`HrtfBinaural`]).
//! - **FFT** (`fft/`): [`RealFft`], a real-input FFT whose arithmetic does not depend on the
//!   CPU (the convolver also runs it in steps), and [`FftScratch`], reusable plans and
//!   buffers; [`Correlator`] cross-correlates through it. It is rustfft's *scalar* planner
//!   under realfft's even-length real passes (an odd size runs a full-length complex
//!   transform), planned lazily per scratch on the cold path. A spectrum, a pitch or an onset
//!   is bit-identical on every CPU on the same OS, libm and rustfft version (the twiddles call
//!   the C library's `sin` and `cos`, so not across libms). Values cross as
//!   [`Complex32`] and [`Complex64`].
//! - **STFT vocabulary** (`stft/`): [`CosineWindow`] and the [`Window`] trait,
//!   [`StftGeometry`] (window, hop, shape and rate, validated together), and [`Grid`] with
//!   its typed indices; [`StftError`] is what they refuse with.
//! - **Resampling** (`resample/`): [`WindowedSinc`], [`FftResampler`] (failing with a
//!   [`ResampleError`]) and [`StreamingResampler`], sized by a [`ChunkSize`].
//! - **Dither** (`dither/`): [`DitherNoise`] ([`DitherShape`]) and [`DitherSink`], the
//!   `tutti_buffers::Sink` adapter that dithers on its way to a fixed-point format.
//! - **Oscillator** (`oscillator.rs`): [`Oscillator`], a sine, or a saw, square or triangle
//!   band-limited by PolyBLEP and PolyBLAMP ([`OscillatorWave`]).
//! - **Stereo and utility** (`polarity.rs`, `stereo.rs`, `pan.rs`): [`Polarity`], the
//!   channels a block inverts; [`MidSide`] and [`WidthBank`] (the side scaled by a glided
//!   [`StereoWidth`](tutti_units::StereoWidth)); [`PanLaw`] and [`pan_gains`].
//! - **Tremolo, auto-pan and ring modulation** (`tremolo.rs`, `ring_mod.rs`): [`Tremolo`]
//!   and [`AutoPan`], an [`Oscillator`] scaling the level or placing it by [`pan_gains`];
//!   [`RingModulator`], the input times a sine carrier.
//! - **Bit crusher and DC blocker** (`bit_crush.rs`, `dc_block.rs`): [`BitCrusher`], a
//!   sample-and-hold every [`hold_frames`] frames, then [`quantize`] to a bit depth;
//!   [`DcBlocker`], a first-order high-pass with its pole `exp(-2 pi fc / fs)`.
//! - **Control and small kernels**: [`Glide`], a value that moves to its target in steps
//!   (with [`Lerp`] and [`Steps`]); [`smooth_toward`], the one-pole step; [`COEFF_INTERVAL`] and
//!   [`LANES`], the solve interval and channel grouping of the filter banks; [`lerp`],
//!   [`cubic_hermite`] and [`parabolic_peak`]; [`rms`]; [`Xorshift32`], a seeded noise
//!   source.
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

mod bit_crush;
mod control;
mod convert;
mod convolver;
mod dc_block;
mod delay;
mod design;
mod dither;
mod dynamics;
mod fft;
mod filter;
mod finite;
mod fir;
mod follower;
mod glide;
mod interp;
mod level;
mod modulation;
mod onepole;
mod oscillator;
mod oversample;
mod pan;
mod polarity;
mod position;
mod ring_mod;
mod stereo;
mod tremolo;
mod waveshape;

mod model;
mod resample;
mod reverb;
mod smoother;
mod sos;
mod spatial;
mod stft;
#[cfg(test)]
mod testing;
mod xorshift;

pub use bit_crush::{BitCrusher, BitCrusherControls, hold_frames, quantize};
pub use control::{COEFF_INTERVAL, LANES};
pub use convolver::{Convolver, IrSpectra, MatrixConvolver, MatrixConvolverError};
pub use dc_block::{DC_CORNER, DcBlocker, DcBlockerControls, MIN_DC_CORNER};
pub(crate) use delay::delay_step;
pub use delay::{
    ChannelTimes, ChannelTimesError, DELAY_SLEW, DelayBank, DelayControls, DelayGlides, DelayLanes,
    DelayLayout, DelayLine, InterpolationMode, MAX_DELAY_CHANNELS, MaxDelay, MaxDelayError,
    Routing, RoutingError,
};
pub use dither::sink::DitherSink;
pub use dither::{DitherNoise, DitherShape};
pub use dynamics::{
    BandReductions, BandSettings, BankShape, BrickwallBank, BrickwallControls, BrickwallLanes,
    CompressorBank, CompressorControls, CompressorLanes, DynamicEqBank, DynamicEqControls,
    DynamicMode, GAIN_GLIDE, GateBank, GateControls, GateLanes, KeyFilter, KeySource, LimiterBank,
    LimiterControls, LimiterCurve, LimiterLanes, Lookahead, LookaheadError, MultibandBank,
    MultibandLanes, MultibandSettings, PeakDetection, TransientBank, TransientControls,
    TruePeakError, amplitude_to_db, compute_compressor_gain_reduction, compute_limiter_gain,
};
pub use fft::{Complex, Complex32, Complex64, Correlator, FftFloat, FftScratch, RealFft};
pub use filter::{
    Biquad, CrossoverBank, CrossoverCoefficients, CrossoverControls, CrossoverError,
    CrossoverModel, EnvelopeFilterBank, EnvelopeFilterControls, LadderBank, LadderCoefficients,
    LadderCoeffs, LadderControls, LadderDesign, LadderModel, LadderType, Lanes, MAX_SPLITS,
    SOLVE_CEILING, SvfBank, SvfCoefficients, SvfCoeffs, SvfControls, SvfModel, SvfType,
    fit_to_rate, ladder_step, svf_step,
};
pub use fir::{Fir, FirDesign, FirError, FirResponse, MAX_ATTENUATION, kaiser_beta};
pub use glide::{Glide, Lerp, Steps};
pub use interp::{ParabolicPeak, cubic_hermite, lerp, parabolic_peak};
pub use level::rms;
pub use model::{Lti, MaxDecay, MaxDecayError, Ring, System, TAIL_FLOOR};
pub use modulation::allpass::{allpass_complement, sweep_hz};
pub use modulation::mod_delay::{
    MAX_MOD_DELAY_CHANNELS, MIN_RATE, ModDelayBank, ModDelayControls, ModDelayLayout, SweepDesign,
};
pub use modulation::phaser::{
    MAX_PHASER_CHANNELS, PhaserChain, PhaserControls, Stages, StagesError, SweepRange,
    SweepRangeError,
};
pub use modulation::sweep::{Stagger, StaggerError, Sweep, SweepError};
pub use oscillator::{Oscillator, OscillatorStep, OscillatorWave};
pub use oversample::{
    Oversampler, Oversampling, OversamplingFactor, OversamplingPlan, Passband, PassbandError,
};
pub use pan::{PanLaw, pan_gains};
pub use polarity::{ChannelOutOfRange, Polarity};
pub use resample::{
    ChunkSize, FftResampler, ResampleError, StreamingResampler, WindowedSinc, needs_resampling,
    resampled_frames,
};
pub use reverb::fdn::bank::{FdnBank, FdnGlides};
pub use reverb::fdn::design::{FdnControls, FdnLayout, MAX_PRE_DELAY, MIN_DECAY};
pub use reverb::freeverb::bank::{FreeverbBank, FreeverbGlides};
pub use reverb::freeverb::design::FreeverbControls;
pub(crate) use reverb::network::loop_gain;
pub use reverb::network::{
    Absorption, AllPass, AllPassForm, Band, Fdn, FirSummary, Full, Gains, Inline, Line,
    LineModulation, MAX_CHANNELS, MAX_DENSE, MAX_LINES, MAX_STAGES, MAX_TAPS, Matrix, Network,
    OutputMap, Stage, Tap, Taps, rt60_from_gain,
};
pub use reverb::plate::bank::{PlateBank, PlateGlides};
pub use reverb::plate::design::{MAX_PLATE_DECAY, PlateControls};
pub use ring_mod::{RingModControls, RingModulator};
pub use smoother::smooth_toward;
pub use sos::{
    Butterworth, ButterworthOrder, ButterworthOrderError, CutResponse, LinkwitzRiley,
    MAX_GAIN_MAGNITUDE, MAX_SECTIONS, MatchedBell, Sos, SosBank, SosCoefficients, SosError, Tilt,
};
pub use spatial::{
    HRTF_LATENCY, HRTF_STEP, HrirMeasurement, HrirSphere, HrtfBinaural, HrtfBinauralError,
    MAX_PREPARED_RATE, MAX_SPHERE_RATE, MIN_SPHERE_RATE, SpeakerGeometryError, SpeakerLayout,
    VbapGains, VbapKernel,
};
pub use stereo::{MidSide, WidthBank, WidthControls};
pub use stft::error::StftError;
pub use stft::geometry::StftGeometry;
pub use stft::grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use stft::window::{CosineWindow, Window};
pub use tremolo::{AutoPan, AutoPanControls, Tremolo, TremoloControls};
pub use waveshape::{Antialiasing, ShapeKind, WaveshaperBank, WaveshaperControls};
pub use xorshift::Xorshift32;
