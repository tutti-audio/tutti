//! Every polysynth node's config is a `NodeConfig`: its initials are the values the built
//! node starts at, a change of values keeps the structure, and a change of structure does not.

use std::fmt::Debug;
use std::num::NonZeroUsize;

use tutti_dsp::EnvelopeStages;
use tutti_graph::{HostValues, IntoNode, NodeConfig, ParamSet};
use tutti_polysynth::{
    AdditiveVoice, InstrumentConfig, InstrumentVoice, KarplusVoice, MasterVolume, ModalVoice,
    NoteInstrument, OscillatorType, SynthConfig, UnisonConfig, UnisonVoices, WaveguideVoice,
};
use tutti_units::{Amplitude, CentsSpan, Normalized, Seconds, Spread};

use crate::support::instruments::{additive, karplus, modal, poly, waveguide};

/// One config's row: `base`, a variant `values` that moves every param it has, and variants
/// that each change one structure field.
///
/// Checks (a) the initials of `base` and `values` are what their built nodes start at;
/// (b) `values` is the same structure as `base`, both ways, and moves every initial;
/// (c) each structure variant is not, both ways.
fn check<C>(name: &str, base: &C, values: &C, structures: &[C])
where
    C: NodeConfig<f32> + Debug,
    C::Node: IntoNode<f32, Controls = ParamSet>,
{
    for config in [base, values] {
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
    }
    let (was, now) = (base.initials(), values.initials());
    assert_eq!(
        was.params().len(),
        now.params().len(),
        "{name}: the same params"
    );
    for (&(addr, a), &(other, b)) in was.params().iter().zip(now.params()) {
        assert_eq!(addr, other, "{name}: the same params");
        assert_ne!(a, b, "{name}: the values variant moves {addr:?}");
    }
    assert!(base.same_structure(values) && values.same_structure(base));
    for (k, other) in structures.iter().enumerate() {
        assert!(
            !base.same_structure(other) && !other.same_structure(base),
            "{name}: structure variant {k} is a new node"
        );
    }
}

fn voice_row<V: InstrumentVoice<Float = f32>>(name: &str, voice: V::Config, other: V::Config)
where
    InstrumentConfig<V>: NodeConfig<f32, Node = NoteInstrument<V>> + Debug,
    NoteInstrument<V>: IntoNode<f32, Controls = ParamSet>,
{
    let base = InstrumentConfig::<V>::new(voice.clone(), poly(4));
    check(
        name,
        &base,
        &base.clone().gain(Amplitude::new(0.5)),
        &[
            InstrumentConfig::<V>::new(other, poly(4)),
            InstrumentConfig::<V>::new(voice, poly(5)),
        ],
    );
}

fn gate(release: f64) -> EnvelopeStages {
    EnvelopeStages::new(
        Seconds::new(0.001),
        Seconds::new(0.1),
        Normalized::of(0.5),
        Seconds::new(release),
    )
}

#[test]
fn each_instrument_voice_config_lists_its_values_and_compares_its_structure() {
    voice_row::<ModalVoice<f32>>("modal", modal(), modal().envelope(gate(1.0)));
    voice_row::<WaveguideVoice<f32>>("waveguide", waveguide(), waveguide().envelope(gate(1.0)));
    voice_row::<KarplusVoice<f32>>("karplus", karplus(), karplus().envelope(gate(1.0)));
    voice_row::<AdditiveVoice<f32>>("additive", additive(), additive().envelope(gate(1.0)));
}

#[test]
fn a_synth_config_lists_its_params_and_compares_its_structure() {
    let unison = UnisonConfig::new().voice_count(UnisonVoices::of(3));
    let plain = SynthConfig::new();
    let stacked = plain.clone().unison(unison.clone());
    check::<SynthConfig>(
        "synth",
        &plain,
        &plain.clone().volume(MasterVolume::of(Amplitude::new(0.5))),
        &[
            plain.clone().oscillator(OscillatorType::Sine),
            plain.clone().polyphony(NonZeroUsize::new(4).unwrap()),
            stacked.clone(),
        ],
    );
    check::<SynthConfig>(
        "synth with unison",
        &stacked,
        &stacked
            .clone()
            .volume(MasterVolume::of(Amplitude::new(0.5)))
            .unison(
                unison
                    .clone()
                    .detune_cents(CentsSpan::of(12.0))
                    .stereo_spread(Spread::new(0.5)),
            ),
        &[
            stacked
                .clone()
                .unison(unison.clone().voice_count(UnisonVoices::of(5))),
            stacked.clone().unison(unison.phase_randomize(true)),
            plain,
        ],
    );
}
