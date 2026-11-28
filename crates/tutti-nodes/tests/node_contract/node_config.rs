//! Every node config's `NodeConfig`: its initials are the values the built node starts
//! at, a change of values keeps the structure, and a change of structure does not.

use std::fmt::Debug;
use std::sync::Arc;

use tutti_buffers::{
    AmbisonicFormat, AmbisonicOrder, ChannelIndex, ChannelLayout, ChannelTopology, Interleaved,
    Mono, Wave,
};
use tutti_dsp::{
    Algorithm, ButterworthOrder, ClipperDrive, CombDelay, CombForm, CompressorControls,
    CrossoverControls, CutAlignment, CutResponse, DynamicMode, FdnControls, FdnLayout, FmPatch,
    FreeverbControls, FreezeControls, GateControls, GraphicBands, HarmonicNormalization, HopSize,
    HrirSphere, HysteresisDrive, Inline, KeyFilter, KeySource, LadderSaturation, LadderType,
    LimiterControls, LinearPhaseDesign, Lookahead, LowestPitch, MatrixResponses, MaxDecay,
    MaxGrains, ModDelayControls, Mode, Modes, OperatorCount, OperatorFrequency, OperatorSpec,
    OscillatorWave, OversamplingFactor, PanLaw, Partial, Partials, PeakDetection, PhaserControls,
    ReferenceDistance, ShapeKind, ShimmerControls, ShimmerLayout, SpringLength, SpringStages,
    Stages, Tap, TapSet, Tilt, TubeDrive, VelvetDensity, Waveshape, Wavetable,
};
use tutti_graph::{HostValues, IntoNode, NodeConfig, ParamSet};
use tutti_nodes::{
    AdditiveConfig, AmbisonicEncoderConfig, AmbisonicRotatorConfig, AutoPanConfig,
    BellFilterConfig, BitCrusherConfig, BrickwallLimiterConfig, ClickNodeConfig, ClickSettings,
    CombFilterConfig, CompressorConfig, ConvolverConfig, CrossoverConfig, CutFilterConfig,
    DcBlockerConfig, DecorrelatorConfig, DelayConfig, DelayControls, DelayLayout, DelaySync,
    DiodeClipperConfig, DistanceConfig, DistortionConfig, DynamicEqConfig, EarlyReflectionsConfig,
    EnvelopeFilterConfig, EqBandConfig, FdnConfig, FdnNodeControls, FirConvolverConfig, FmConfig,
    FreeverbConfig, FrequencyShifterConfig, GateConfig, GranularConfig, GranularSource,
    GraphicEqConfig, HrtfBinauralConfig, HysteresisConfig, ImpulseResponses, InterpolationMode,
    IrLayout, KarplusStrongConfig, LadderFilterConfig, LevellerConfig, LimiterConfig, MatrixLayout,
    MaxDelay, MidSideConfig, ModDelayConfig, ModalBankConfig, MultibandBands,
    MultibandCompressorConfig, MultibandControls, PhaserConfig, PitchShifterConfig,
    PlateReverbConfig, PolarityConfig, RingModConfig, ShimmerConfig, SpringReverbConfig, Stagger,
    StereoWidthConfig, SvfFilterConfig, SvfType, SweepRange, TiltFilterConfig,
    TransientShaperConfig, TremoloConfig, TubeStageConfig, WaveguideStringConfig, WavetableConfig,
};
use tutti_units::{
    Amplitude, ArcDegrees, Azimuth, Bipolar, BitDepth, Cents, CompressionRatio, Db, Direction,
    Drive, Elevation, Feedback, FrequencyRatio, Gain, HoldFactor, Hz, Metres, Mix, ModulationIndex,
    Normalized, Q, Resonance, SampleRate, SampleSpan, Samples, Seconds, Semitones, Speed,
    StereoWidth,
};

use tutti_fixtures::hrir::synthetic_hrir_sphere;

/// The params a built node's controls address.
trait Params {
    fn params(&self) -> &ParamSet;
}

impl Params for ParamSet {
    fn params(&self) -> &ParamSet {
        self
    }
}

impl Params for MultibandControls {
    fn params(&self) -> &ParamSet {
        MultibandControls::params(self)
    }
}

/// One config's row: `base`, a variant `values` that moves every value it has (each
/// param and each kinded setting; `None` for a config that holds none), and variants that
/// each change one structure field.
///
/// Checks (a) the initials of `base` and `values` are what their built nodes start at,
/// params and setting words; (b) `values` is the same structure as `base`, both ways,
/// and moves every initial; (c) each structure variant is not, both ways. A config
/// whose cells do not round-trip their values exactly fails (b), since the structure
/// compare assembles one config's cells over the other's.
fn check<C>(name: &str, base: &C, values: Option<&C>, structures: &[C])
where
    C: NodeConfig<f64> + Debug,
    C::Node: IntoNode<f64>,
    <C::Node as IntoNode<f64>>::Controls: Params,
{
    for config in std::iter::once(base).chain(values) {
        let initials = config.initials();
        let (_, controls) = config.build(&HostValues::new()).into_node();
        let controls = controls.params();
        assert_eq!(
            controls.params().count(),
            initials.params().len(),
            "{name}: one initial per declared param"
        );
        for &(addr, raw) in initials.params() {
            assert_eq!(
                controls.get(addr),
                Some(raw),
                "{name}: {addr:?} starts at its initial"
            );
        }
        let settings = config.build(&HostValues::new()).into_parts().settings;
        let words: Vec<_> = settings
            .iter()
            .map(|s| (s.kind(), s.cell().load_word()))
            .collect();
        assert_eq!(
            words,
            initials.settings(),
            "{name}: each setting starts at its word"
        );
        assert!(
            config.same_structure(config),
            "{name}: a config is its own structure"
        );
    }
    if let Some(values) = values {
        let (was, now) = (base.initials(), values.initials());
        assert!(
            !was.params().is_empty() || !was.settings().is_empty(),
            "{name}: has values"
        );
        assert_eq!(
            was.params().len(),
            now.params().len(),
            "{name}: the same params"
        );
        for (&(addr, a), &(other, b)) in was.params().iter().zip(now.params()) {
            assert_eq!(addr, other, "{name}: the same params");
            assert_ne!(a, b, "{name}: the values variant moves {addr:?}");
        }
        for (a, b) in was.settings().iter().zip(now.settings()) {
            assert_ne!(a, b, "{name}: the values variant moves setting {a:?}");
        }
        assert!(
            base.same_structure(values),
            "{name}: values keep the structure"
        );
        assert!(
            values.same_structure(base),
            "{name}: values keep the structure (reversed)"
        );
    }
    for (k, other) in structures.iter().enumerate() {
        assert!(
            !base.same_structure(other),
            "{name}: structure variant {k} is a new node"
        );
        assert!(
            !other.same_structure(base),
            "{name}: structure variant {k} (reversed)"
        );
    }
}

/// A node with no param and no kinded setting: its config lists no initial value, is its own
/// structure, and each variant is a new node.
fn check_plain<C>(name: &str, base: &C, structures: &[C])
where
    C: NodeConfig<f64> + Debug,
{
    let initials = base.initials();
    assert!(
        initials.params().is_empty() && initials.settings().is_empty(),
        "{name}: lists no initial value"
    );
    let parts = base.build(&HostValues::new()).into_parts();
    assert!(parts.settings.is_empty(), "{name}: the node has no setting");
    assert!(
        base.same_structure(&base.clone()),
        "{name}: a config is its own structure"
    );
    for (k, other) in structures.iter().enumerate() {
        assert!(
            !base.same_structure(other) && !other.same_structure(base),
            "{name}: structure variant {k} is a new node"
        );
    }
}

/// A bus of `n` channels in the engine's order.
fn bus(n: u16) -> ChannelTopology {
    ChannelTopology::default_for(ChannelLayout::from(n)).unwrap()
}

fn mono() -> ChannelLayout {
    ChannelLayout::MONO
}

fn stereo() -> ChannelLayout {
    ChannelLayout::STEREO
}

#[test]
fn each_config_lists_its_values_and_compares_its_structure() {
    // Filters.
    let svf = SvfFilterConfig::new(mono(), SvfType::LowPass);
    check(
        "svf",
        &svf,
        Some(&svf.freq(Hz::new(440.0)).q(Q::new(2.0)).gain(Db::new(-3.0))),
        &[
            SvfFilterConfig::new(mono(), SvfType::HighPass),
            SvfFilterConfig::new(stereo(), SvfType::LowPass),
        ],
    );
    let bell = BellFilterConfig::new(mono(), Hz::new(1000.0), Q::new(1.0), Db::new(3.0));
    check(
        "bell",
        &bell,
        Some(&BellFilterConfig::new(
            mono(),
            Hz::new(2000.0),
            Q::new(2.0),
            Db::new(-3.0),
        )),
        &[BellFilterConfig::new(
            stereo(),
            Hz::new(1000.0),
            Q::new(1.0),
            Db::new(3.0),
        )],
    );
    let tilt = TiltFilterConfig::new(mono(), Hz::new(1000.0), Db::new(3.0));
    check(
        "tilt",
        &tilt,
        Some(&TiltFilterConfig::new(
            mono(),
            Hz::new(2000.0),
            Db::new(-3.0),
        )),
        &[TiltFilterConfig::new(
            stereo(),
            Hz::new(1000.0),
            Db::new(3.0),
        )],
    );
    let cut = CutFilterConfig::new(
        mono(),
        CutResponse::LowPass,
        CutAlignment::LinkwitzRiley(ButterworthOrder::of(4)),
    );
    check(
        "cut",
        &cut,
        Some(&cut.cutoff(Hz::new(500.0))),
        &[
            CutFilterConfig::new(
                mono(),
                CutResponse::HighPass,
                CutAlignment::LinkwitzRiley(ButterworthOrder::of(4)),
            ),
            CutFilterConfig::new(
                mono(),
                CutResponse::LowPass,
                CutAlignment::Butterworth(ButterworthOrder::of(4)),
            ),
        ],
    );
    let envelope = EnvelopeFilterConfig::new(mono(), Hz::new(300.0), SvfType::LowPass);
    check(
        "envelope",
        &envelope,
        Some(
            &EnvelopeFilterConfig::new(mono(), Hz::new(600.0), SvfType::LowPass)
                .depth(Semitones::new(12.0))
                .q(Q::new(3.0))
                .attack(Seconds::new(0.02))
                .release(Seconds::new(0.3)),
        ),
        &[
            EnvelopeFilterConfig::new(mono(), Hz::new(300.0), SvfType::HighPass),
            EnvelopeFilterConfig::new(stereo(), Hz::new(300.0), SvfType::LowPass),
        ],
    );
    let ladder = LadderFilterConfig::new(mono(), LadderType::LP24);
    check(
        "ladder",
        &ladder,
        Some(
            &ladder
                .freq(Hz::new(800.0))
                .res(Resonance::new(0.4))
                .drive(Drive::new(2.0)),
        ),
        &[
            LadderFilterConfig::new(mono(), LadderType::HP12),
            ladder.saturation(LadderSaturation::Input),
        ],
    );
    let graphic = GraphicEqConfig::new(bus(1), GraphicBands::Octave);
    check(
        "graphic eq",
        &graphic,
        None,
        &[
            GraphicEqConfig::new(bus(2), GraphicBands::Octave),
            GraphicEqConfig::new(bus(1), GraphicBands::ThirdOctave),
        ],
    );
    let crossover = CrossoverConfig::new(
        bus(2),
        CrossoverControls::try_from(&[Hz::new(1000.0)][..]).unwrap(),
    )
    .unwrap();
    check(
        "crossover",
        &crossover,
        Some(
            &CrossoverConfig::new(
                bus(2),
                CrossoverControls::try_from(&[Hz::new(2000.0)][..]).unwrap(),
            )
            .unwrap(),
        ),
        &[
            CrossoverConfig::new(
                bus(1),
                CrossoverControls::try_from(&[Hz::new(1000.0)][..]).unwrap(),
            )
            .unwrap(),
            CrossoverConfig::new(
                bus(2),
                CrossoverControls::try_from(&[Hz::new(500.0), Hz::new(4000.0)][..]).unwrap(),
            )
            .unwrap(),
        ],
    );

    // Dynamics.
    let compressor = CompressorConfig::new(bus(2));
    check(
        "compressor",
        &compressor,
        Some(&compressor.controls(CompressorControls {
            threshold: Db::new(-18.0),
            knee: Db::new(6.0),
            ratio: CompressionRatio::new(4.0),
            attack: Seconds::new(0.01),
            release: Seconds::new(0.2),
            makeup: Db::new(3.0),
            key_filter: KeyFilter::HighPass,
            key_cutoff: Hz::new(200.0),
            key_q: Q::new(2.0),
        })),
        &[
            CompressorConfig::new(bus(1)),
            compressor.key(KeySource::Sidechain),
        ],
    );
    let gate = GateConfig::new(bus(2));
    check(
        "gate",
        &gate,
        Some(&gate.controls(GateControls {
            threshold: Db::new(-30.0),
            attack: Seconds::new(0.01),
            hold: Seconds::new(0.05),
            release: Seconds::new(0.2),
            range: Db::new(-30.0),
            ratio: CompressionRatio::new(4.0),
            key_filter: KeyFilter::BandPass,
            key_cutoff: Hz::new(300.0),
            key_q: Q::new(2.0),
        })),
        &[GateConfig::new(bus(1)), gate.key(KeySource::Sidechain)],
    );
    let limiter = LimiterConfig::new(bus(2));
    check(
        "limiter",
        &limiter,
        Some(&limiter.controls(LimiterControls {
            threshold: Db::new(-12.0),
            ceiling: Db::new(-2.0),
            release: Seconds::new(0.2),
        })),
        &[
            LimiterConfig::new(bus(1)),
            limiter.lookahead(Lookahead::of(Seconds::new(0.002))),
            limiter.detection(PeakDetection::TruePeak),
        ],
    );
    let brickwall = BrickwallLimiterConfig::new(bus(2), Db::new(-3.0));
    check(
        "brickwall",
        &brickwall,
        Some(&BrickwallLimiterConfig::new(bus(2), Db::new(-9.0))),
        &[BrickwallLimiterConfig::new(bus(1), Db::new(-3.0))],
    );
    let transient = TransientShaperConfig::new(mono());
    check(
        "transient",
        &transient,
        Some(&transient.attack(Db::new(6.0)).sustain(Db::new(-6.0))),
        &[
            TransientShaperConfig::new(stereo()),
            transient.fast(Seconds::new(0.002)),
            transient.slow(Seconds::new(0.2)),
        ],
    );
    let dynamic_eq = DynamicEqConfig::new(mono(), Hz::new(2000.0), Q::new(1.0));
    check(
        "dynamic eq",
        &dynamic_eq,
        Some(
            &DynamicEqConfig::new(mono(), Hz::new(4000.0), Q::new(2.0))
                .threshold(Db::new(-20.0))
                .knee(Db::new(6.0))
                .ratio(CompressionRatio::new(4.0))
                .attack(Seconds::new(0.01))
                .release(Seconds::new(0.2))
                .range(Db::new(-12.0)),
        ),
        &[
            DynamicEqConfig::new(stereo(), Hz::new(2000.0), Q::new(1.0)),
            dynamic_eq.mode(DynamicMode::Upward),
            dynamic_eq.key(KeySource::Sidechain),
        ],
    );
    let leveller = LevellerConfig::new(bus(2)).unwrap();
    check(
        "leveller",
        &leveller,
        Some(
            &leveller
                .clone()
                .target(Db::new(-16.0))
                .max_boost(Db::new(6.0))
                .max_cut(Db::new(6.0))
                .gate(Db::new(-40.0))
                .attack(Seconds::new(0.2))
                .release(Seconds::new(1.0)),
        ),
        &[LevellerConfig::new(bus(1)).unwrap()],
    );

    // Delay and modulation.
    let max = MaxDelay::of(Seconds::new(1.0));
    let layout = DelayLayout::new(ChannelTopology::MONO, max);
    let delay = DelayConfig::new(layout);
    check(
        "delay",
        &delay,
        Some(&delay.controls(DelayControls {
            sync: DelaySync::Synced,
            time: Seconds::new(0.25),
            feedback: Feedback::new(0.5),
            cross_feedback: Feedback::new(0.25),
        })),
        &[
            DelayConfig::new(DelayLayout::new(ChannelTopology::STEREO, max)),
            DelayConfig::new(layout.interpolation(InterpolationMode::CubicHermite)),
        ],
    );
    let mod_delay = ModDelayConfig::chorus(bus(2));
    check(
        "mod delay",
        &mod_delay,
        Some(&mod_delay.controls(ModDelayControls {
            rate: Hz::new(2.0),
            depth: Seconds::new(0.004),
            feedback: Feedback::new(0.6),
        })),
        &[
            ModDelayConfig::chorus(bus(1)),
            ModDelayConfig::flanger(bus(2)),
        ],
    );
    let phaser = PhaserConfig::new(bus(2), Stages::of(4), Stagger::NONE).unwrap();
    check(
        "phaser",
        &phaser,
        Some(&phaser.controls(PhaserControls {
            rate: Hz::new(1.0),
            depth: Bipolar::of(0.8),
            feedback: Feedback::new(0.4),
        })),
        &[
            PhaserConfig::new(bus(1), Stages::of(4), Stagger::NONE).unwrap(),
            PhaserConfig::new(bus(2), Stages::of(6), Stagger::NONE).unwrap(),
            phaser.range(SweepRange::of(Hz::new(300.0), Hz::new(3000.0))),
        ],
    );
    let tremolo = TremoloConfig::new(mono());
    check(
        "tremolo",
        &tremolo,
        Some(&tremolo.rate(Hz::new(6.0)).depth(Bipolar::of(0.8))),
        &[
            TremoloConfig::new(stereo()),
            tremolo.wave(OscillatorWave::Square),
        ],
    );
    let auto_pan = AutoPanConfig::new();
    check(
        "auto pan",
        &auto_pan,
        Some(&auto_pan.rate(Hz::new(3.0)).depth(Bipolar::of(0.5))),
        &[
            auto_pan.wave(OscillatorWave::Square),
            auto_pan.law(PanLaw::SquareRoot),
        ],
    );
    let comb = CombFilterConfig::new(
        mono(),
        CombForm::Feedback,
        CombDelay::of(Seconds::new(0.004)),
    );
    check(
        "comb",
        &comb,
        Some(&comb.gain(Feedback::new(0.7))),
        &[
            CombFilterConfig::new(
                stereo(),
                CombForm::Feedback,
                CombDelay::of(Seconds::new(0.004)),
            ),
            CombFilterConfig::new(
                mono(),
                CombForm::Feedforward,
                CombDelay::of(Seconds::new(0.004)),
            ),
            CombFilterConfig::new(
                mono(),
                CombForm::Feedback,
                CombDelay::of(Seconds::new(0.008)),
            ),
        ],
    );
    let ring = RingModConfig::new(mono());
    check(
        "ring mod",
        &ring,
        Some(&ring.frequency(Hz::new(300.0))),
        &[RingModConfig::new(stereo())],
    );
    let shifter = FrequencyShifterConfig::new(mono());
    check(
        "frequency shifter",
        &shifter,
        Some(&shifter.shift(Hz::new(50.0))),
        &[FrequencyShifterConfig::new(stereo())],
    );
    let pitch = PitchShifterConfig::new(mono());
    check(
        "pitch shifter",
        &pitch,
        Some(&pitch.pitch(Cents::new(700.0))),
        &[
            PitchShifterConfig::new(stereo()),
            pitch.hop(HopSize::of(Samples::new(250))),
        ],
    );
    let dc = DcBlockerConfig::new(mono());
    check(
        "dc blocker",
        &dc,
        Some(&dc.corner(Hz::new(20.0))),
        &[DcBlockerConfig::new(stereo())],
    );
    let stereo_width = StereoWidthConfig::new();
    check(
        "stereo width",
        &stereo_width,
        Some(&stereo_width.width(StereoWidth::new(1.5))),
        &[],
    );
    let decorrelator = DecorrelatorConfig::new(stereo());
    check(
        "decorrelator",
        &decorrelator,
        None,
        &[
            DecorrelatorConfig::new(mono()),
            decorrelator.density(VelvetDensity::new(Hz::new(20_000.0)).unwrap()),
            decorrelator.seed(7),
        ],
    );

    // Distortion.
    let distortion = DistortionConfig::new(mono(), Waveshape::new(ShapeKind::Tanh));
    check(
        "distortion",
        &distortion,
        Some(&distortion.drive(Drive::new(3.0))),
        &[
            DistortionConfig::new(stereo(), Waveshape::new(ShapeKind::Tanh)),
            DistortionConfig::new(mono(), Waveshape::new(ShapeKind::HardClip)),
            distortion.oversampling(OversamplingFactor::X4),
        ],
    );
    let bits = BitCrusherConfig::new(mono());
    check(
        "bit crusher",
        &bits,
        Some(&bits.bits(BitDepth::new(12.0)).hold(HoldFactor::new(4.0))),
        &[BitCrusherConfig::new(stereo())],
    );
    let diode = DiodeClipperConfig::new(mono());
    check(
        "diode clipper",
        &diode,
        Some(
            &diode
                .cutoff(Hz::new(4000.0))
                .drive(ClipperDrive::of(Db::new(-6.0))),
        ),
        &[
            DiodeClipperConfig::new(stereo()),
            diode.oversampling(OversamplingFactor::X2),
        ],
    );
    let tube = TubeStageConfig::new(mono());
    check(
        "tube stage",
        &tube,
        Some(&tube.drive(TubeDrive::of(Db::new(12.0)))),
        &[
            TubeStageConfig::new(stereo()),
            tube.oversampling(OversamplingFactor::X2),
        ],
    );
    let hysteresis = HysteresisConfig::new(mono());
    check(
        "hysteresis",
        &hysteresis,
        Some(&hysteresis.drive(HysteresisDrive::of(Db::new(-6.0)))),
        &[
            HysteresisConfig::new(stereo()),
            hysteresis.saturation(Normalized::of(0.25)),
            hysteresis.coercivity(Normalized::of(0.25)),
            hysteresis.oversampling(OversamplingFactor::X2),
        ],
    );

    // Reverbs.
    let network = FdnControls {
        decay: [Seconds::new(3.0), Seconds::new(2.0), Seconds::new(1.0)],
        size: Mix::new(0.4),
        diffusion: Mix::new(0.3),
        pre_delay: Seconds::new(0.02),
        modulation: Bipolar::of(0.2),
        rate: Hz::new(0.7),
        width: StereoWidth::new(0.5),
        wet: Db::new(-6.0),
    };
    let fdn = FdnConfig::new();
    check(
        "fdn",
        &fdn,
        Some(&fdn.controls(FdnNodeControls {
            network,
            kind: FreezeControls { freeze: true },
        })),
        &[fdn.layout(FdnLayout::new(4, MaxDecay::DEFAULT).unwrap())],
    );
    let freeverb = FreeverbConfig::new();
    check(
        "freeverb",
        &freeverb,
        Some(&freeverb.controls(FreeverbControls {
            room: Mix::new(0.9),
            damp: Mix::new(0.1),
        })),
        &[],
    );
    let plate = PlateReverbConfig::new();
    check(
        "plate",
        &plate,
        Some(
            &plate
                .decay(Seconds::new(4.0))
                .damping(Mix::new(0.3))
                .predelay(Seconds::new(0.02))
                .bandwidth(Hz::new(6000.0))
                .diffusion(Mix::new(0.5))
                .modulation(Bipolar::of(0.3)),
        ),
        &[],
    );
    let shimmer = ShimmerConfig::new();
    check(
        "shimmer",
        &shimmer,
        Some(&shimmer.controls(FdnNodeControls {
            network,
            kind: ShimmerControls {
                pitch: Cents::new(700.0),
                send: Feedback::new(0.25),
            },
        })),
        &[shimmer.layout(ShimmerLayout::new(
            FdnLayout::new(4, MaxDecay::DEFAULT).unwrap(),
        ))],
    );
    let spring = SpringReverbConfig::new(mono());
    check(
        "spring",
        &spring,
        Some(
            &spring
                .decay(Seconds::new(3.0))
                .tone(Hz::new(3000.0))
                .dispersion(Mix::new(0.4)),
        ),
        &[
            SpringReverbConfig::new(stereo()),
            spring.stages(SpringStages::of(30)),
            spring.length(SpringLength::of(Seconds::new(0.03))),
        ],
    );
    let taps = |gain| TapSet {
        taps: Inline::from_slice(&[Tap {
            delay: SampleSpan::new(100.0).unwrap(),
            gain: Gain::new(gain),
            channel: 0,
        }])
        .unwrap(),
    };
    check(
        "early reflections",
        &EarlyReflectionsConfig::new(1u16, taps(0.5)).unwrap(),
        None,
        &[
            EarlyReflectionsConfig::new(2u16, taps(0.5)).unwrap(),
            EarlyReflectionsConfig::new(1u16, taps(0.25)).unwrap(),
        ],
    );

    // Strings, bodies and sources.
    let karplus = KarplusStrongConfig::new(mono());
    check(
        "karplus strong",
        &karplus,
        Some(&karplus.frequency(Hz::new(330.0)).decay(Seconds::new(1.0))),
        &[
            KarplusStrongConfig::new(stereo()),
            karplus.lowest(LowestPitch::new(Hz::new(50.0)).unwrap()),
        ],
    );
    let waveguide = WaveguideStringConfig::new(mono());
    check(
        "waveguide string",
        &waveguide,
        Some(&waveguide.frequency(Hz::new(330.0)).decay(Seconds::new(1.0))),
        &[
            WaveguideStringConfig::new(stereo()),
            waveguide.excitation(Normalized::of(0.3)),
            waveguide.pickup(Normalized::of(0.3)),
            waveguide.lowest(LowestPitch::new(Hz::new(50.0)).unwrap()),
        ],
    );
    let mode = |hz| Mode {
        frequency: Hz::new(hz),
        decay: Seconds::new(1.0),
        gain: Gain::new(1.0),
    };
    check(
        "modal bank",
        &ModalBankConfig::new(mono(), Modes::try_from(&[mode(440.0)][..]).unwrap()),
        None,
        &[
            ModalBankConfig::new(stereo(), Modes::try_from(&[mode(440.0)][..]).unwrap()),
            ModalBankConfig::new(mono(), Modes::try_from(&[mode(880.0)][..]).unwrap()),
        ],
    );
    let distance = DistanceConfig::new(mono(), Metres::of(100.0));
    check(
        "distance",
        &distance,
        Some(&distance.distance(Metres::of(10.0))),
        &[
            DistanceConfig::new(stereo(), Metres::of(100.0)),
            DistanceConfig::new(mono(), Metres::of(200.0)),
            distance.reference(ReferenceDistance::of(Metres::of(2.0))),
            distance.speed(Speed::new(300.0).unwrap()),
            distance.air_absorption(Hz::new(8000.0)),
        ],
    );

    // Synthesis kernels.
    let ratio = |x: f64| OperatorFrequency::Ratio(FrequencyRatio::new(x).unwrap());
    let patch =
        |count: u8| {
            FmPatch::new(Algorithm::chain(OperatorCount::of(count)))
                .operators(&vec![OperatorSpec::new(ratio(1.0)); usize::from(count)])
        };
    let fm = FmConfig::new(patch(2));
    check(
        "fm",
        &fm,
        Some(
            &fm.frequency(Hz::new(330.0))
                .level(Amplitude::new(0.5))
                .index(ModulationIndex::of(2.0))
                .feedback(ModulationIndex::of(0.5)),
        ),
        &[
            FmConfig::new(patch(3)),
            fm.oversampling(OversamplingFactor::X4),
        ],
    );
    let partial =
        |ratio: f64| Partial::new(FrequencyRatio::new(ratio).unwrap(), Amplitude::new(0.5));
    let partials = |ratios: &[f64]| {
        Partials::try_from(&ratios.iter().map(|&r| partial(r)).collect::<Vec<_>>()[..]).unwrap()
    };
    let additive = AdditiveConfig::new(partials(&[1.0, 2.0]));
    check(
        "additive",
        &additive,
        Some(
            &additive
                .clone()
                .frequency(Hz::new(330.0))
                .level(Amplitude::new(0.5)),
        ),
        &[AdditiveConfig::new(partials(&[1.0, 3.0]))],
    );

    // Ambisonics.
    let side = Direction::new(Azimuth::new(90.0), Elevation::new(30.0));
    let encoder = AmbisonicEncoderConfig::new(AmbisonicOrder::of(1), HarmonicNormalization::Sn3d);
    check(
        "ambisonic encoder",
        &encoder,
        Some(&encoder.direction(side)),
        &[
            AmbisonicEncoderConfig::new(AmbisonicOrder::of(2), HarmonicNormalization::Sn3d),
            AmbisonicEncoderConfig::new(AmbisonicOrder::of(1), HarmonicNormalization::N3d),
        ],
    );
    let rotator = AmbisonicRotatorConfig::new(AmbisonicOrder::of(1), HarmonicNormalization::Sn3d);
    check(
        "ambisonic rotator",
        &rotator,
        Some(&rotator.front(side)),
        &[
            AmbisonicRotatorConfig::new(AmbisonicOrder::of(2), HarmonicNormalization::Sn3d),
            // The roll is fixed for the life of the node, not a param: structure.
            rotator.roll(ArcDegrees::new(45.0)),
        ],
    );

    // Hand configs: no params, so the whole config is structure.
    check_plain(
        "mid/side",
        &MidSideConfig::encode(),
        &[MidSideConfig::decode()],
    );
    check_plain(
        "polarity",
        &PolarityConfig::only(stereo(), [ChannelIndex::of(1)]).unwrap(),
        &[
            PolarityConfig::all(stereo()).unwrap(),
            PolarityConfig::only(stereo(), [ChannelIndex::of(0)]).unwrap(),
            PolarityConfig::only(4u16, [ChannelIndex::of(1)]).unwrap(),
        ],
    );
    check_plain(
        "click",
        &ClickNodeConfig::new(Arc::new(ClickSettings::new())),
        &[ClickNodeConfig::new(Arc::new(ClickSettings::new()))],
    );
    let band = EqBandConfig::new(SvfType::Bell);
    check(
        "eq band",
        &band,
        Some(&band.freq(Hz::new(440.0)).q(Q::new(2.0)).gain(Db::new(-3.0))),
        &[EqBandConfig::new(SvfType::LowShelf)],
    );

    // Assets: equal by identity, so a loaded again resource is a new node.
    let wave = |frames: usize| {
        let mut wave = Wave::<f64>::new(1, SampleRate::SR_48K);
        wave.append(Interleaved::new(&vec![0.5; frames], ChannelLayout::MONO).unwrap())
            .unwrap();
        Arc::new(wave)
    };
    let source = wave(64);
    let granular = GranularConfig::new(GranularSource::new(mono(), Arc::clone(&source)).unwrap());
    check(
        "granular",
        &granular,
        Some(
            &granular
                .clone()
                .density(Hz::new(20.0))
                .rate(Cents::new(100.0))
                .length(Seconds::new(0.05))
                .jitter([Normalized::of(0.25), Normalized::of(0.5)])
                .position(Normalized::of(0.5)),
        ),
        &[
            GranularConfig::new(GranularSource::new(mono(), wave(64)).unwrap()),
            GranularConfig::new(GranularSource::new(stereo(), Arc::clone(&source)).unwrap()),
            granular.clone().max_grains(MaxGrains::of(8)),
            granular.clone().seed(1),
        ],
    );
    let frame = [0.25_f64; 16];
    let table = Wavetable::new(&[Mono::new(&frame)]).unwrap();
    let wavetable = WavetableConfig::new(mono(), Arc::clone(&table));
    check(
        "wavetable",
        &wavetable,
        Some(
            &wavetable
                .clone()
                .frequency(Hz::new(220.0))
                .level(Amplitude::new(0.5))
                .position(Normalized::of(0.5)),
        ),
        &[
            WavetableConfig::new(mono(), Wavetable::new(&[Mono::new(&frame)]).unwrap()),
            WavetableConfig::new(stereo(), table),
        ],
    );
    let sphere = |rate| Arc::new(HrirSphere::from_bytes(&synthetic_hrir_sphere(rate, 64)).unwrap());
    let first = sphere(48_000);
    let hrtf = HrtfBinauralConfig::new(Arc::clone(&first));
    check(
        "hrtf",
        &hrtf,
        Some(
            &hrtf
                .clone()
                .azimuth(Azimuth::new(45.0))
                .elevation(Elevation::new(10.0))
                .blend(Mix::new(0.5)),
        ),
        &[HrtfBinauralConfig::new(sphere(48_000))],
    );

    // Rows form and owned assets: all structure, with the declared params at their starts.
    let multiband = |input, bands| MultibandCompressorConfig::new(input, MultibandBands::of(bands));
    check(
        "multiband",
        &multiband(bus(2), 3),
        None,
        &[multiband(bus(1), 3), multiband(bus(2), 2)],
    );
    let responses = Arc::new(ImpulseResponses::new(&[Mono::new(&[1.0_f64, 0.5])]));
    let convolver = |output, layout, responses: &Arc<ImpulseResponses<f64>>| {
        ConvolverConfig::new(output, layout, Arc::clone(responses)).unwrap()
    };
    let base = convolver(ChannelTopology::MONO, IrLayout::PerChannel, &responses);
    check(
        "convolver",
        &base,
        None,
        &[
            convolver(ChannelTopology::STEREO, IrLayout::PerChannel, &responses),
            convolver(
                ChannelTopology::MONO,
                IrLayout::PerChannel,
                &Arc::new(ImpulseResponses::new(&[Mono::new(&[1.0_f64, 0.5])])),
            ),
            base.clone().block_size(Samples::new(64)),
            base.clone().zero_latency(),
        ],
    );
    let tilt = Tilt::new(Db::new(6.0))
        .design(Hz::new(2_000.0), SampleRate::SR_48K)
        .unwrap();
    let fir = Arc::new(
        LinearPhaseDesign::new(Hz::new(200.0))
            .design(SampleRate::SR_48K, &[tilt])
            .unwrap(),
    );
    let fir_config = FirConvolverConfig::new(ChannelTopology::MONO, Arc::clone(&fir)).unwrap();
    check(
        "fir convolver",
        &fir_config,
        None,
        &[
            FirConvolverConfig::new(ChannelTopology::STEREO, fir).unwrap(),
            fir_config.clone().block_size(Samples::new(64)),
        ],
    );
}

/// A convolver config is its responses' handle: two configs built separately from one handle
/// are the same structure, and one from another handle with equal content is a new node, for
/// every layout (an app re-declares its config on every patch, and a rebuilt node loses its
/// convolution tail).
#[test]
fn convolver_configs_follow_the_handle_of_their_responses_for_every_layout() {
    let taps = [1.0_f64, 0.5];
    let foa = MatrixLayout::Ambisonic(AmbisonicFormat::ambix(AmbisonicOrder::of(1)));
    let irs = |n: usize| vec![Mono::new(&taps[..]); n];
    let direct = |layout: IrLayout, output: ChannelTopology, n: usize| {
        let handle = Arc::new(ImpulseResponses::new(&irs(n)));
        let make = move |h: &Arc<ImpulseResponses<f64>>| {
            ConvolverConfig::new(output, layout, Arc::clone(h)).unwrap()
        };
        let other = Arc::new(ImpulseResponses::new(&irs(n)));
        (make(&handle), make(&handle), make(&other))
    };
    let matrix = |layout: MatrixLayout, output: ChannelTopology, inputs: u16, outputs: u16| {
        let n = usize::from(inputs * outputs);
        let set =
            || Arc::new(MatrixResponses::new(inputs.into(), outputs.into(), &irs(n)).unwrap());
        let (handle, other) = (set(), set());
        let make = move |h: &Arc<MatrixResponses<f64>>| {
            ConvolverConfig::matrix(output, layout, Arc::clone(h)).unwrap()
        };
        (make(&handle), make(&handle), make(&other))
    };
    let cases = [
        ("per-channel", direct(IrLayout::PerChannel, bus(2), 2)),
        ("folded", direct(IrLayout::Folded, bus(2), 2)),
        (
            "true stereo",
            matrix(MatrixLayout::TrueStereo, ChannelTopology::STEREO, 2, 2),
        ),
        ("ambisonic", matrix(foa, ChannelTopology::STEREO, 4, 2)),
    ];
    let same = |a: &ConvolverConfig<f64>, b: &ConvolverConfig<f64>| {
        NodeConfig::<f64>::same_structure(a, b)
    };
    for (name, (a, b, other)) in &cases {
        assert!(same(a, b) && same(b, a), "{name}: one handle");
        assert!(
            !same(a, other) && !same(other, a),
            "{name}: another handle is a new node"
        );
    }
}
