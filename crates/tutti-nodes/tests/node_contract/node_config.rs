//! Every node config's `NodeConfig`: its initials are the values the built node starts
//! at, a change of values keeps the structure, and a change of structure does not.

use std::fmt::Debug;
use std::sync::Arc;

use tutti_buffers::{
    AmbisonicFormat, AmbisonicOrder, ChannelIndex, ChannelLayout, ChannelTopology, Interleaved,
    Mono, Wave,
};
use tutti_dsp::MaxGrains;
use tutti_dsp::{
    CrossoverControls, HrirSphere, Inline, PhaserControls, Stages, Tap, TapSet, Wavetable,
};
use tutti_graph::{HostValues, IntoNode, NodeConfig, ParamSet};
use tutti_nodes::{
    AmbisonicEncoderConfig, AmbisonicRotatorConfig, ClickNodeConfig, ClickSettings,
    CrossoverConfig, DelayConfig, DelayControls, DelayLayout, DelaySync, EarlyReflectionsConfig,
    EqBandConfig, GranularConfig, GranularSource, HrtfBinauralConfig, InterpolationMode,
    LevellerConfig, MaxDelay, MidSideConfig, PhaserConfig, PolarityConfig, Stagger,
    SvfFilterConfig, SvfType, SweepRange, WavetableConfig,
};
use tutti_units::{
    Amplitude, ArcDegrees, Azimuth, Bipolar, Cents, Db, Direction, Elevation, Feedback, Gain, Hz,
    Mix, Normalized, Q, SampleRate, SampleSpan, Seconds,
};

use tutti_fixtures::hrir::synthetic_hrir_sphere;

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
    C::Node: IntoNode<f64, Controls = ParamSet>,
{
    for config in std::iter::once(base).chain(values) {
        let initials = config.initials();
        let (_, controls) = config.build(&HostValues::new()).into_node();
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

    // Ambisonics.
    let ambix = || ChannelTopology::Ambisonic(AmbisonicFormat::ambix(AmbisonicOrder::of(1)));
    let ambix2 = || ChannelTopology::Ambisonic(AmbisonicFormat::ambix(AmbisonicOrder::of(2)));
    let side = Direction::new(Azimuth::new(90.0), Elevation::new(30.0));
    let encoder = AmbisonicEncoderConfig::new(ambix()).unwrap();
    check(
        "ambisonic encoder",
        &encoder,
        Some(&encoder.direction(side)),
        &[AmbisonicEncoderConfig::new(ambix2()).unwrap()],
    );
    let rotator = AmbisonicRotatorConfig::new(ambix()).unwrap();
    check(
        "ambisonic rotator",
        &rotator,
        Some(&rotator.front(side)),
        &[
            AmbisonicRotatorConfig::new(ambix2()).unwrap(),
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
}
