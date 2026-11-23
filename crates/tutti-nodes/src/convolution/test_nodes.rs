//! Test constructors: one `ConvolverConfig` per layout, at the buses a test names.

use tutti_buffers::{BusTopology, ChannelLayout, ChannelTopology, Mono, Sample};
use tutti_dsp::{FftFloat, Lerp};
use tutti_units::Samples;

use super::{ConvolverConfig, ConvolverError, ConvolverNode};

/// `width` channels that feed no speaker, as a bus.
pub(crate) fn unassigned_bus(width: impl Into<ChannelLayout>) -> BusTopology {
    BusTopology::unassigned(width.into()).expect("a test width fits a bus")
}

/// A one-channel node over `ir`.
pub(crate) fn mono_node<F: FftFloat + Lerp, S: Sample>(
    ir: Mono<'_, S>,
    block_size: Samples,
) -> ConvolverNode<F> {
    ConvolverNode::try_from(ConvolverConfig::new(BusTopology::MONO, &[ir]).block_size(block_size))
        .expect("one response on a mono bus")
}

/// `width` unassigned channels sharing the response `ir`.
pub(crate) fn shared_ir_node<F: FftFloat + Lerp, S: Sample>(
    width: impl Into<ChannelLayout>,
    ir: Mono<'_, S>,
    block_size: Samples,
) -> Result<ConvolverNode<F>, ConvolverError> {
    ConvolverNode::try_from(
        ConvolverConfig::new(unassigned_bus(width), &[ir]).block_size(block_size),
    )
}

/// One response per channel, at as many unassigned channels as responses.
pub(crate) fn per_channel_node<F: FftFloat + Lerp, S: Sample>(
    irs: &[Mono<'_, S>],
    block_size: Samples,
) -> Result<ConvolverNode<F>, ConvolverError> {
    let width = ChannelLayout::try_from(irs.len()).expect("a test width fits a layout");
    ConvolverNode::try_from(ConvolverConfig::new(unassigned_bus(width), irs).block_size(block_size))
}

/// Every input folded to mono, then one response per output, at the engine's placement of
/// that many channels.
pub(crate) fn folded_node<F: FftFloat + Lerp, S: Sample>(
    irs: &[Mono<'_, S>],
    block_size: Samples,
) -> Result<ConvolverNode<F>, ConvolverError> {
    let width = ChannelLayout::try_from(irs.len()).expect("a test width fits a layout");
    let bus = BusTopology::default_for(width).expect("a test width fits a bus");
    ConvolverNode::try_from(
        ConvolverConfig::new(bus, irs)
            .folded()
            .block_size(block_size),
    )
}

/// True stereo from `[LL, LR, RL, RR]`.
pub(crate) fn true_stereo_node<F: FftFloat + Lerp, S: Sample>(
    ll: Mono<'_, S>,
    lr: Mono<'_, S>,
    rl: Mono<'_, S>,
    rr: Mono<'_, S>,
    block_size: Samples,
) -> ConvolverNode<F> {
    ConvolverNode::try_from(
        ConvolverConfig::new(BusTopology::STEREO, &[ll, lr, rl, rr])
            .true_stereo()
            .block_size(block_size),
    )
    .expect("four responses on a stereo pair")
}

/// An ambisonic set of `input` in, `output` out.
pub(crate) fn ambisonic_node<F: FftFloat + Lerp, S: Sample>(
    input: tutti_buffers::AmbisonicFormat,
    output: ChannelTopology,
    irs: &[Mono<'_, S>],
    block_size: Samples,
) -> Result<ConvolverNode<F>, ConvolverError> {
    ConvolverNode::try_from(
        ConvolverConfig::new(output, irs)
            .ambisonic(input)
            .block_size(block_size),
    )
}
