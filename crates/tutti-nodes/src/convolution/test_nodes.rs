//! Test constructors: one `ConvolverConfig` per layout, at the buses a test names.

use tutti_buffers::{ChannelLayout, ChannelTopology, Mono, Sample};
use tutti_dsp::{FftFloat, Lerp};
use tutti_units::Samples;

use tutti_graph::Shared;

use super::{ConvolverConfig, ConvolverError, ConvolverNode, ImpulseResponses, IrLayout};

/// A config of `layout` over `irs` at `output`, for `block_size`.
fn config<S: Sample>(
    output: ChannelTopology,
    layout: IrLayout,
    irs: &[Mono<'_, S>],
    block_size: Samples,
) -> Result<ConvolverConfig<S>, ConvolverError> {
    ConvolverConfig::new(output, layout, Shared::new(ImpulseResponses::new(irs)))
        .map(|config| config.block_size(block_size))
}

/// `width` channels that feed no speaker, as a bus.
pub(crate) fn unassigned_bus(width: impl Into<ChannelLayout>) -> ChannelTopology {
    ChannelTopology::unassigned(width.into()).expect("a test width fits a bus")
}

/// A one-channel node over `ir`.
pub(crate) fn mono_node<F: FftFloat + Lerp, S: Sample>(
    ir: Mono<'_, S>,
    block_size: Samples,
) -> ConvolverNode<F> {
    ConvolverNode::from(
        config(
            ChannelTopology::MONO,
            IrLayout::PerChannel,
            &[ir],
            block_size,
        )
        .expect("one response on a mono bus"),
    )
}

/// `width` unassigned channels sharing the response `ir`.
pub(crate) fn shared_ir_node<F: FftFloat + Lerp, S: Sample>(
    width: impl Into<ChannelLayout>,
    ir: Mono<'_, S>,
    block_size: Samples,
) -> Result<ConvolverNode<F>, ConvolverError> {
    config(
        unassigned_bus(width),
        IrLayout::PerChannel,
        &[ir],
        block_size,
    )
    .map(ConvolverNode::from)
}

/// One response per channel, at as many unassigned channels as responses.
pub(crate) fn per_channel_node<F: FftFloat + Lerp, S: Sample>(
    irs: &[Mono<'_, S>],
    block_size: Samples,
) -> Result<ConvolverNode<F>, ConvolverError> {
    let width = ChannelLayout::try_from(irs.len()).expect("a test width fits a layout");
    config(unassigned_bus(width), IrLayout::PerChannel, irs, block_size).map(ConvolverNode::from)
}

/// Every input folded to mono, then one response per output, at the engine's placement of
/// that many channels.
pub(crate) fn folded_node<F: FftFloat + Lerp, S: Sample>(
    irs: &[Mono<'_, S>],
    block_size: Samples,
) -> Result<ConvolverNode<F>, ConvolverError> {
    let width = ChannelLayout::try_from(irs.len()).expect("a test width fits a layout");
    let bus = ChannelTopology::default_for(width).expect("a test width fits a bus");
    config(bus, IrLayout::Folded, irs, block_size).map(ConvolverNode::from)
}

/// True stereo from `[LL, LR, RL, RR]`.
pub(crate) fn true_stereo_node<F: FftFloat + Lerp, S: Sample>(
    ll: Mono<'_, S>,
    lr: Mono<'_, S>,
    rl: Mono<'_, S>,
    rr: Mono<'_, S>,
    block_size: Samples,
) -> ConvolverNode<F> {
    ConvolverNode::from(
        config(
            ChannelTopology::STEREO,
            IrLayout::TrueStereo,
            &[ll, lr, rl, rr],
            block_size,
        )
        .expect("four responses on a stereo pair"),
    )
}

/// An ambisonic set of `input` in, `output` out.
pub(crate) fn ambisonic_node<F: FftFloat + Lerp, S: Sample>(
    input: tutti_buffers::AmbisonicFormat,
    output: ChannelTopology,
    irs: &[Mono<'_, S>],
    block_size: Samples,
) -> Result<ConvolverNode<F>, ConvolverError> {
    config(output, IrLayout::Ambisonic(input), irs, block_size).map(ConvolverNode::from)
}
