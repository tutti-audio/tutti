//! [`Handled`]: the sampler nodes whose controls [`Graph::handle`](crate::Graph::handle) reaches.

#[cfg(feature = "sampler")]
use tutti_buffers::Float;
#[cfg(feature = "sampler")]
use tutti_graph::IntoNode;

/// The sampler nodes whose controls are a handle with commands:
/// [`Graph::handle`](crate::Graph::handle) reaches it by key. Sealed to the
/// three nodes (`VoiceNode`, `Clip` and `DiskVoice`) whose commands are not yet targets.
/// A `VoicePool` is not one: its table is a target, and its readings are
/// [`Graph::stream_hold`](crate::Graph::stream_hold),
/// [`Graph::pool_drops`](crate::Graph::pool_drops) and
/// [`Graph::pool_spares`](crate::Graph::pool_spares), which hand out no controls (they
/// would be a second writer of the table).
#[cfg(feature = "sampler")]
pub trait Handled<F: Float = f32>:
    IntoNode<F, Controls: Send + 'static> + Send + 'static + sealed::Sealed
{
}

#[cfg(feature = "sampler")]
mod sealed {
    use tutti_sampler::{Clip, DiskVoice, SamplerFloat, VoiceNode};

    pub trait Sealed {}
    impl<F: SamplerFloat> Sealed for VoiceNode<F> {}
    impl<F: SamplerFloat> Sealed for Clip<F> {}
    impl<F: SamplerFloat> Sealed for DiskVoice<F> {}
}

#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> Handled<F> for tutti_sampler::VoiceNode<F> {}
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> Handled<F> for tutti_sampler::Clip<F> {}
#[cfg(feature = "sampler")]
impl<F: tutti_sampler::SamplerFloat> Handled<F> for tutti_sampler::DiskVoice<F> {}
