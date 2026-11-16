#![doc = include_str!("../README.md")]
//! Pure DSP algorithms over `tutti_buffers` views for the Tutti audio engine: a
//! CPU-independent real FFT and the algorithms behind the built-in effect nodes: filters, dynamics,
//! the partitioned convolver, delay lines, reverb, waveshapers and the export
//! resampler. The analyzers (loudness, pitch, onsets, spectra, ...) are `tutti-measure`,
//! built on this crate's FFT, windows and `StftGeometry` (whose refusals are `StftError`).
//!
//! ## Effect algorithms
//!
//! `tutti-nodes` wraps each of these in a graph node that owns the params, the
//! ramps and the graph shape. Here they take plain typed values per call.
//!
//! - **Filters** — `SvfCoeffs` and `SvfBank<F>` (Simper's trapezoidal
//!   state-variable filter, eight responses by `SvfType`); `LadderCoeffs` and
//!   `LadderBank<F>` (the Moog ladder, `tanh` feedback); `CrossoverModel` and
//!   `CrossoverBank`, a 2 to 6 band Linkwitz-Riley 4th-order split on trapezoidal
//!   state-variable sections. Each bank is one filter per channel over buffers views,
//!   its coefficients a `Glide` the caller aims. `F` is the `f32` or `f64` the state
//!   runs in; its transcendentals call `libm`, so results do not depend on the
//!   platform's C math library.
//! - **Dynamics** — the curves
//!   `compute_compressor_gain_reduction`, `compute_limiter_gain` and
//!   `LimiterCurve`; the compressor and the noise gate over buffers views (`CompressorBank` and
//!   `GateBank`, each on a typed `KeySource`, a side-chain bus or the signal itself; the
//!   threshold a per-frame lane, the makeup and the range `Glide`s over `GAIN_GLIDE`); the
//!   lookahead limiter (`LimiterBank`: threshold and ceiling per-frame lanes, every coefficient
//!   landing whole, the lookahead its latency); the brickwall clipper (`BrickwallBank`); and
//!   the multiband compressor (`MultibandBank`: a crossover tree per channel and a
//!   `CompressorBank` per band, each band's makeup, bypass and solo gliding over
//!   `GAIN_GLIDE`).
//! - **Convolution** — `Convolver`, a uniform or non-uniform partitioned FFT
//!   convolution over an IR's shared, read-only `IrSpectra`.
//! - **Delay** — `DelayLine`, a fractional-read ring with an `InterpolationMode`;
//!   `DelayBank`, the feedback delay over buffers views (a `DelayLayout` of a checked
//!   `MaxDelay`, `Routing` and `ChannelTimes`; its time and feedback `Glide`s and per-frame
//!   `DelayLanes`); `ModDelayBank`, the chorus and flanger over buffers views (a
//!   `ModDelayLayout` of a checked `Sweep` and `Stagger`; one `Glide` per channel's delay and one
//!   for the feedback, the LFO's corners solved by the caller every `COEFF_INTERVAL` frames of
//!   the stream and interpolated between); and
//!   `PhaserChain`, the phaser's swept all-pass chain over buffers views (a checked `Stages`,
//!   `SweepRange` and `Stagger`; one `Glide` per channel's coefficient and one for the
//!   feedback).
//! - **Waveshaping** — `ShapeKind`, six memoryless curves, and `WaveshaperBank`, which runs
//!   one over a block at a gliding or per-frame drive.
//! - **Reverb** — the reverb model, a `Network` with analytic queries
//!   (RT60, tail, stability, densities), `FreeverbBank`, Freeverb's combs and
//!   all-passes over buffers views, and `FdnBank`, the feedback delay network.
//! - **Spatial** — vector base amplitude panning (`SpeakerLayout`, `VbapKernel`) and the
//!   HRTF binaural convolver (`HrtfBinaural`).
//! - **Resampling and dither** — `FftResampler` and `StreamingResampler` for export, and
//!   `DitherNoise` with `DitherSink`, the dither path export and recording share.
//!
//! Each algorithm is laid out as its design (controls to a model), its model (a `System`, and
//! an `Lti` where it is linear and time-invariant) and its bank (the per-frame math over
//! buffers views).
//!
//! FFT is `RealFft`, the engine's one real-input FFT: rustfft's
//! *scalar* planner under realfft's even-length real passes (an odd size runs a
//! full-length complex transform). Its arithmetic does not depend on the CPU, so
//! a spectrum, a pitch or an onset is bit-identical on every CPU on the same OS,
//! libm and rustfft version (the twiddles call the C library's `sin` and `cos`, so not across
//! libms). `tutti-sampler`'s vocoder and `tutti-nodes`' convolver use the same
//! transform, so values cross freely as `num_complex::Complex<f32>`. This crate
//! plans lazily, per scratch, on the cold path.
//!
//! ## Scope
//!
//! - **No graph, no framework, no ECS.** Nothing here is a graph node or knows
//!   about one.
//! - **No threading and no scheduling.** A host that wants these results on a
//!   background thread owns that plumbing itself.
//!
//! The `tutti` crate re-exports this one as `tutti::dsp` behind its `dsp` feature. `tutti-nodes`, `tutti-sampler` and
//! `tutti-export` use it.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Items
//!
//! By algorithm, as the source is laid out (each one's design, model and bank side
//! by side):
//!
//! - **The model** (`model/`): a [`System`] is what every processor answers (its
//!   [`latency`](System::latency) and [`tail`](System::tail), a [`Ring`] to a floor,
//!   [`TAIL_FLOOR`] by default), and [`Lti`] the class of a linear time-invariant one
//!   ([`frequency_response`](Lti::frequency_response),
//!   [`spectral_radius`](Lti::spectral_radius), [`l1_norm`](Lti::l1_norm),
//!   [`hinf_norm`](Lti::hinf_norm), with [`Complex64`]); [`MaxDecay`] is the longest decay
//!   a reverb is built to.
//! - **Filters** (`filter/`): the SVF ([`SvfBank`], [`SvfModel`], [`SvfType`]), the
//!   ladder ([`LadderBank`], [`LadderModel`], [`LadderType`]) and the Linkwitz-Riley
//!   crossover ([`CrossoverBank`], [`CrossoverModel`]), each one filter per channel over
//!   buffers views; [`Lanes`], the arithmetic their steps run in, and the [`Biquad`]
//!   section they share.
//! - **Dynamics** (`dynamics/`): the
//!   gain curves ([`LimiterCurve`] and the `compute_*` functions); the lookahead limiter over
//!   buffers views ([`LimiterBank`], its threshold and ceiling [`LimiterLanes`], its lookahead
//!   its latency, its delay a [`DelayLine`] read by whole frames); the brickwall clipper
//!   ([`BrickwallBank`], its ceiling a per-frame lane); the compressor ([`CompressorBank`],
//!   keyed, its threshold a lane, its makeup a [`Glide`] over [`GAIN_GLIDE`]) and the noise
//!   gate ([`GateBank`], its range gliding likewise). Both read a [`KeySource`]: a side-chain
//!   bus or the signal itself. The multiband compressor ([`MultibandBank`]) is a crossover tree
//!   per channel and a [`CompressorBank`] per band, each band's makeup, bypass and solo
//!   gliding over [`GAIN_GLIDE`].
//! - **Reverb** (`reverb/`): the model, a [`Network`] of [`Stage`]s with
//!   analytic queries ([`Network::rt60`], [`tail`](System::tail),
//!   [`is_stable`](Lti::is_stable), the densities) and a [`NetworkCapacity`] worst case;
//!   [`FreeverbBank`] (its comb feedback and damping [`Glide`]s in [`FreeverbGlides`]) and the
//!   feedback delay network over buffers views ([`FdnBank`] over an [`FdnLayout`], its
//!   parameters [`Glide`]s in [`FdnGlides`]). [`loop_gain`] and [`rt60_from_gain`] are the RT60 formula both ways.
//! - **FIR design** (`fir.rs`): [`FirDesign`] describes a Kaiser windowed-sinc filter (a
//!   [`FirResponse`], a transition and an attenuation) and designs it at a rate into a
//!   [`Fir`], failing with a [`FirError`]; [`kaiser_beta`] is the window's parameter.
//! - **Convolution**: [`Convolver`] over a shared [`IrSpectra`].
//! - **Delay and modulation** (`delay.rs`, `modulation/`): [`DelayLine`] with an
//!   [`InterpolationMode`]; the feedback delay ([`DelayBank`] over a [`DelayLayout`]: a
//!   [`MaxDelay`], its cross-feedback a checked [`Routing`], its time and feedback
//!   [`Glide`]s and the per-frame [`DelayLanes`]); the chorus and flanger over buffers views ([`ModDelayBank`] over a [`ModDelayLayout`]: its
//!   [`Sweep`] and [`Stagger`] checked, a [`Glide`] per channel's delay, the LFO the caller's on
//!   the stream's clock); the phaser
//!   ([`PhaserChain`]: its [`Stages`] and [`SweepRange`] checked, a [`Glide`] per channel's
//!   coefficient, [`sweep_hz`] and [`allpass_coeff`] the pure pieces the sweep is solved from).
//! - **Spatial** (`spatial/`): VBAP ([`SpeakerLayout`], Pulkki's pairs and triplets and
//!   the gain solve over them, [`SpeakerGeometryError`]; [`VbapKernel`] and
//!   [`VbapGains`], one direction's gains and one channel rendered along a glide between two) and the HRTF
//!   convolver ([`HrtfBinaural`]).
//! - **FFT** (`fft/`): [`RealFft`], a real-input FFT whose arithmetic does not depend on
//!   the CPU (the convolver also runs it in steps); [`FftScratch`], reusable
//!   plans and buffers.
//! - **STFT vocabulary** (`stft/`): [`CosineWindow`] and the [`Window`] trait,
//!   [`StftGeometry`] (window, hop, shape and rate, validated together), and [`Grid`]
//!   with its typed indices; [`StftError`] is what they refuse with.
//! - **Resampling** (`resample/`): [`WindowedSinc`], [`FftResampler`] (failing with a
//!   [`ResampleError`]) and [`StreamingResampler`], sized by a [`ChunkSize`].
//! - **Dither** (`dither/`): [`DitherNoise`] ([`DitherShape`]) and [`DitherSink`], the
//!   `tutti_buffers::Sink` adapter export and recording share.
//! - **Oscillator** (`oscillator.rs`): [`Oscillator`], a sine, or a saw, square or
//!   triangle band-limited by PolyBLEP and PolyBLAMP ([`OscillatorWave`]).
//! - **Small kernels**: [`cubic_hermite`], [`ShapeKind`], [`Smoother`].
//!
//! ## Import paths
//!
//! Every public item is re-exported at the crate root; import from there
//! (`tutti_dsp::RealFft`). No module is public.

mod control;
mod convolver;
mod delay;
mod dither;
mod dynamics;
mod fft;
mod filter;
mod fir;
mod glide;
mod interp;
mod level;
mod modulation;
mod oscillator;
mod pan;
mod waveshape;

mod model;
mod resample;
mod reverb;
mod smoother;
mod spatial;
mod stft;
#[cfg(test)]
mod testing;
mod xorshift;

pub use control::{COEFF_INTERVAL, LANES};
pub use convolver::{Convolver, IrSpectra, MatrixConvolver, MatrixConvolverError};
pub use delay::{
    ChannelTimes, ChannelTimesError, DELAY_SLEW, DelayBank, DelayControls, DelayGlides, DelayLanes,
    DelayLayout, DelayLine, InterpolationMode, MAX_DELAY_CHANNELS, MaxDelay, MaxDelayError,
    Routing, RoutingError, delay_step,
};
pub use dither::sink::{DitherSink, DitherSpec};
pub use dither::{DitherNoise, DitherShape};
pub use dynamics::{
    BandReductions, BandSettings, BankShape, BrickwallBank, BrickwallControls, BrickwallLanes,
    CompressorBank, CompressorControls, CompressorLanes, GAIN_GLIDE, GateBank, GateControls,
    GateLanes, KeySource, LimiterBank, LimiterControls, LimiterCurve, LimiterLanes, Lookahead,
    LookaheadError, MultibandBank, MultibandLanes, MultibandSettings, amplitude_to_db,
    compute_compressor_gain_reduction, compute_limiter_gain,
};
pub use fft::{Complex32, Complex64, FftScratch, RealFft};
pub use filter::{
    Biquad, CrossoverBank, CrossoverCoefficients, CrossoverControls, CrossoverError,
    CrossoverModel, DirectForm1, LadderBank, LadderCoefficients, LadderCoeffs, LadderControls,
    LadderDesign, LadderModel, LadderType, Lanes, MAX_SPLITS, SOLVE_CEILING, SvfBank,
    SvfCoefficients, SvfCoeffs, SvfControls, SvfModel, SvfType, fit_to_rate, ladder_step, svf_step,
};
pub use fir::{Fir, FirDesign, FirError, FirResponse, MAX_ATTENUATION, kaiser_beta};
pub use glide::{Glide, Lerp, Steps};
pub use interp::{cubic_hermite, lerp};
pub use model::{Lti, MaxDecay, MaxDecayError, Ring, System, TAIL_FLOOR};
pub use modulation::allpass::{allpass_coeff, sweep_hz};
pub use modulation::mod_delay::{
    MAX_MOD_DELAY_CHANNELS, MIN_RATE, ModDelayBank, ModDelayControls, ModDelayLayout, SweepDesign,
};
pub use modulation::phaser::{
    MAX_PHASER_CHANNELS, PhaserChain, PhaserControls, Stages, StagesError, SweepRange,
    SweepRangeError,
};
pub use modulation::sweep::{Stagger, StaggerError, Sweep, SweepError};
pub use oscillator::{Oscillator, OscillatorWave};
pub use pan::{PanLaw, pan_gains};
pub use resample::{ChunkSize, FftResampler, ResampleError, StreamingResampler, WindowedSinc};
pub use reverb::fdn::bank::{FdnBank, FdnGlides};
pub use reverb::fdn::design::{FdnControls, FdnLayout, MAX_PRE_DELAY, MIN_DECAY};
pub use reverb::freeverb::bank::{FreeverbBank, FreeverbGlides};
pub use reverb::freeverb::design::FreeverbControls;
pub use reverb::network::{
    Absorption, AllPass, AllPassForm, Band, Fdn, FirSummary, Full, Gains, Inline, Line,
    LineModulation, MAX_CHANNELS, MAX_DENSE, MAX_LINES, MAX_STAGES, MAX_TAPS, Matrix, Network,
    NetworkCapacity, OutputMap, Stage, Tap, Taps, loop_gain, pre_delay_frames, rt60_from_gain,
};
pub use smoother::Smoother;
pub use spatial::{
    HRTF_LATENCY, HRTF_STEP, HrirMeasurement, HrirSphere, HrtfBinaural, HrtfBinauralError,
    MAX_PREPARED_RATE, MAX_SPHERE_RATE, MIN_SPHERE_RATE, SpeakerGeometryError, SpeakerLayout,
    VbapGains, VbapKernel,
};
pub use stft::error::StftError;
pub use stft::geometry::StftGeometry;
pub use stft::grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use stft::window::{CosineWindow, Window};
pub use waveshape::{ShapeKind, WaveshaperBank, WaveshaperControls};
pub use xorshift::Xorshift32;
