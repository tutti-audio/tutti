//! Level-dependent gain processors: compressor, gate, limiter, the dynamic EQ band (and its
//! de-esser) and the multiband compressor.
//!
//! All of them share one shape — measure a level, decide a gain, apply it — and the dB
//! conversions and gain curves of `tutti-dsp`. What differs is only the gain decision, which is
//! why each keeps its own file and nothing here is a trait. The compressor, gate, limiter
//! and brickwall are [`KernelNode`](crate::KernelNode)s over the banks of `tutti-dsp`
//! (`CompressorBank`, `GateBank`, `LimiterBank` and `BrickwallBank`); the multiband is a
//! hand-written node over `MultibandBank` (the crossover and a `CompressorBank` a band), see
//! its file.
//!
//! [`CompressorNode`] and [`GateNode`] take a typed [`KeySource`]: an
//! **external sidechain** (their level detector reads separate inputs, so the signal
//! being measured need not be the signal being shaped) or their own signal. [`MultibandCompressorNode`] runs one compressor per
//! band of a Linkwitz-Riley crossover, each detecting on its own band.

use tutti_buffers::ChannelTopology;
use tutti_dsp::KeySource;
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};

mod brickwall;
mod compressor;
mod dynamic_eq;
mod gate;
mod leveller;
mod limiter;
mod multiband;
mod transient;

pub use brickwall::{BrickwallKernel, BrickwallLimiterConfig, BrickwallLimiterNode, BrickwallSet};
pub use compressor::{CompressorConfig, CompressorKernel, CompressorNode, CompressorSet};
pub use dynamic_eq::{DynamicEqConfig, DynamicEqKernel, DynamicEqNode, DynamicEqSet};
pub use gate::{GateConfig, GateKernel, GateNode, GateSet};
pub use leveller::{LevellerConfig, LevellerKernel, LevellerNode, LevellerSet};
pub use limiter::{LimiterConfig, LimiterKernel, LimiterNode, LimiterSet};
pub use multiband::{
    BAND_PARAMS, MultibandCompressorConfig, MultibandCompressorNode, MultibandControls,
};
pub use transient::{
    TransientShaperConfig, TransientShaperKernel, TransientShaperNode, TransientShaperSet,
};

/// The shape of a keyed dynamics node: the `input` bus, then, for a
/// [`KeySource::Sidechain`], the key bus as wide; one output bus where the input is.
fn keyed_shape(input: ChannelTopology, key: KeySource) -> Shape {
    let main = Buses::main(input);
    let inputs = match key {
        KeySource::Own => main,
        KeySource::Sidechain => main
            .with(Bus::new(input, BusRole::Sidechain))
            .expect("a main bus and a key are two buses"),
    };
    Shape::buses(inputs, Buses::main(input)).cost(Cost::Moderate)
}
