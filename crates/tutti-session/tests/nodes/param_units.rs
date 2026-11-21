//! The unit of every engine node's params, as a [`Key`](tutti_session::Key)'s
//! methods hand it out (doc 030, N1): `key.cutoff()` is a `ParamTarget<U>`
//! with `U` the node's [`HasParam`] unit, which the node's own declaration
//! writes from its cell's type. The table is the spec's: each param is in
//! the unit a reader of the node's docs expects (the ladder's resonance is
//! addressed as `Q` and is in `Resonance`; the chorus's depth is a swing in
//! `Seconds`, the phaser's a fraction).

use std::any::TypeId;

use tutti_session::HasParam;
use tutti_units::{
    Amplitude, Azimuth, Balance, BitDepth, Cents, CompressionRatio, Db, Depth, Drive, Elevation,
    Feedback, HoldFactor, Hz, Metres, Mix, ModulationIndex, Q, Resonance, Seconds, Semitones,
    Spread, StereoWidth, UnitParam,
};

/// Whether `N`'s param of kind `P` is in `U`: the unit a key's method for it
/// returns.
fn is_in<N: HasParam<P>, const P: u16, U: 'static>(node: &str, kind: UnitParam)
where
    N::Unit: 'static,
{
    assert_eq!(
        TypeId::of::<N::Unit>(),
        TypeId::of::<U>(),
        "{node}: {kind:?} is not in {}",
        std::any::type_name::<U>()
    );
}

macro_rules! units {
    ($($test:ident $(#[$cfg:meta])* { $($node:ty { $($kind:ident => $unit:ty),* $(,)? })* })*) => {$(
        $(#[$cfg])*
        #[test]
        fn $test() {
            $($(
                is_in::<$node, { UnitParam::$kind as u16 }, $unit>(
                    stringify!($node),
                    UnitParam::$kind,
                );
            )*)*
        }
    )*};
}

units! {
    every_engine_nodes_params_are_in_their_declared_unit {
        tutti_nodes::SvfFilterNode<f32> { Cutoff => Hz, Q => Q, GainDb => Db }
        tutti_nodes::SvfFilterNode<f64> { Cutoff => Hz, Q => Q, GainDb => Db }
        tutti_nodes::EqBandNode<f32> { Cutoff => Hz, Q => Q, GainDb => Db }
        tutti_nodes::EqBandNode<f64> { Cutoff => Hz, Q => Q, GainDb => Db }
        tutti_nodes::LadderFilterNode<f32> { Cutoff => Hz, Q => Resonance, Drive => Drive }
        tutti_nodes::LadderFilterNode<f64> { Cutoff => Hz, Q => Resonance, Drive => Drive }
        tutti_nodes::CompressorNode<f32> {
            Threshold => Db, Ratio => CompressionRatio, Attack => Seconds, Release => Seconds,
            GainDb => Db
        }
        tutti_nodes::CompressorNode<f64> {
            Threshold => Db, Ratio => CompressionRatio, Attack => Seconds, Release => Seconds,
            GainDb => Db
        }
        tutti_nodes::DynamicEqNode<f32> {
            Threshold => Db, Knee => Db, Ratio => CompressionRatio, Attack => Seconds,
            Release => Seconds, Range => Db, Cutoff => Hz, Q => Q
        }
        tutti_nodes::GateNode<f32> {
            Threshold => Db, Attack => Seconds, Hold => Seconds, Release => Seconds, Range => Db,
            Ratio => CompressionRatio
        }
        tutti_nodes::GateNode<f64> {
            Threshold => Db, Attack => Seconds, Hold => Seconds, Release => Seconds, Range => Db,
            Ratio => CompressionRatio
        }
        tutti_nodes::LimiterNode<f32> { Threshold => Db, Ceiling => Db, Release => Seconds }
        tutti_nodes::LimiterNode<f64> { Threshold => Db, Ceiling => Db, Release => Seconds }
        tutti_nodes::BrickwallLimiterNode<f32> { Ceiling => Db }
        tutti_nodes::BrickwallLimiterNode<f64> { Ceiling => Db }
        tutti_nodes::DistortionNode<f32> { Drive => Drive }
        tutti_nodes::BitCrusherNode<f32> { BitDepth => BitDepth, HoldFactor => HoldFactor }
        tutti_nodes::EnvelopeFilterNode<f32> {
            Cutoff => Hz, Q => Q, Attack => Seconds, Release => Seconds, Semitones => Semitones
        }
        tutti_nodes::PitchShifterNode<f32> { Pitch => Cents }
        tutti_nodes::DistanceNode<f32> { Distance => Metres }
        tutti_nodes::TransientShaperNode<f32> { GainDb => Db }
        tutti_nodes::FmNode<f32> {
            Frequency => Hz, Volume => Amplitude, Index => ModulationIndex
        }
        tutti_nodes::AdditiveNode<f32> { Frequency => Hz, Volume => Amplitude }
        tutti_nodes::BusStripNode { Volume => Amplitude, Balance => Balance }
        tutti_nodes::CrossoverNode<f32> { Cutoff => Hz }
        tutti_nodes::MultibandCompressorNode<f32> {
            Cutoff => Hz, Threshold => Db, Knee => Db, Ratio => CompressionRatio,
            Attack => Seconds, Release => Seconds, GainDb => Db, Bypass => bool, Solo => bool
        }
        tutti_nodes::DelayNode<f32> {
            DelayTime => Seconds, Feedback => Feedback, CrossFeedback => Feedback
        }
        tutti_nodes::ModDelayNode<f32> { Rate => Hz, Depth => Seconds, Feedback => Feedback }
        tutti_nodes::PhaserNode<f32> { Rate => Hz, Depth => Depth, Feedback => Feedback }
        tutti_nodes::LfoNode { Depth => Depth, Rate => Hz }
        tutti_nodes::OscillatorNode { Frequency => Hz, Volume => Amplitude }
        tutti_nodes::NoiseNode<f32> { Volume => Amplitude }
        tutti_nodes::FreeverbNode<f32> { RoomSize => Mix, Damping => Mix }
        tutti_nodes::ConvolverNode<f32> { Volume => Amplitude }
        tutti_nodes::PlateReverbNode<f32> {
            Decay => Seconds, Damping => Mix, DelayTime => Seconds, Cutoff => Hz, Depth => Depth,
            Diffusion => Mix
        }
        tutti_nodes::FdnNode<f32> {
            Decay => Seconds, RoomSize => Mix, Diffusion => Mix, DelayTime => Seconds,
            Depth => Depth, Rate => Hz, StereoSpread => StereoWidth, GainDb => Db, Freeze => bool
        }
        tutti_nodes::ShimmerNode<f32> {
            Decay => Seconds, RoomSize => Mix, Diffusion => Mix, DelayTime => Seconds,
            Depth => Depth, Rate => Hz, StereoSpread => StereoWidth, GainDb => Db,
            Pitch => Cents, Feedback => Feedback
        }
        tutti_nodes::SpringReverbNode<f32> { Decay => Seconds, Cutoff => Hz, Diffusion => Mix }
        tutti_nodes::VbapPannerNode<f32> {
            Azimuth => Azimuth, Elevation => Elevation, Spread => Spread,
            StereoWidth => StereoWidth
        }
        tutti_nodes::HrtfBinauralNode<f32> { Azimuth => Azimuth, Elevation => Elevation, Wet => Mix }
    }
    the_synths_params_are_in_their_declared_unit #[cfg(feature = "synth")] {
        tutti_polysynth::PolySynth<f32> { Volume => Amplitude, Detune => tutti_units::CentsSpan, StereoSpread => Spread }
    }
    the_samplers_params_are_in_their_declared_unit #[cfg(feature = "sampler")] {
        tutti_sampler::VoiceNode<f32> { Volume => Amplitude }
        tutti_sampler::MemorySource<f32> { Volume => Amplitude }
        tutti_sampler::Clip<f32> { Volume => Amplitude }
    }
}
