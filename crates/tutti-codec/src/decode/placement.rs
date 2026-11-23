//! Which speaker each channel of a decoded track feeds.
//!
//! The one place that reads symphonia's `Channels`. A decoder hands its
//! planar channels in ascending bit order of a positioned set (Vorbis, whose own
//! order is `FL FC FR RL RR LFE`, is reordered by symphonia into it), so the
//! topology lists the speakers in the same order and channel `n` of the samples
//! is entry `n` of the topology.

use symphonia::core::audio::{Channels, Position};
use tutti_buffers::{
    AmbisonicFormat, AmbisonicOrder, Bed, ChannelLayout, ChannelSet, ChannelTopology, Placement,
    Speaker, Speakers,
};

/// The speaker of one symphonia position flag.
///
/// A flag the vocabulary has no speaker for (the second LFE, the wide and the
/// high side channels) is [`Speaker::Unknown`] carrying its bit index, which is
/// its slot and no more: it is a position the format named and [`Speaker`]
/// cannot, not a channel known to feed no speaker, so it is not
/// [`Speaker::Unassigned`]. A fold that meets one refuses it
/// ([`FoldError::Unnamed`](tutti_buffers::FoldError::Unnamed)) rather than guess
/// where it goes. (The sampler builds that fold when it builds a voice over the
/// wave, so a file with one is refused there.)
fn speaker(flag: Position) -> Speaker {
    let bits = flag.bits();
    u32::try_from(bits)
        .ok()
        .and_then(crate::wave_mask::speaker_of)
        .unwrap_or_else(|| Speaker::Unknown(bits.trailing_zeros() as u16))
}

/// The topology of a track's channels, in the order its decoder writes them,
/// and whether the container stated it.
///
/// A positioned set is [`Stated`](Placement::Stated), and a lone channel is the
/// engine's mono ([`Speaker::FrontCenter`]): symphonia names the only channel of
/// a mono file `FRONT_LEFT`, and the engine's mono is the centre. An ambisonic
/// set of up to third order is stated too: symphonia's is ACN with SN3D. A
/// container that states a count and no placement (discrete channels) is
/// [`ChannelTopology::default_for`] at that count and [`Default`](Placement::Default).
/// A container that states no channels at all is stereo by default; a track
/// whose decoder then reports another set is read from that set.
///
/// The channels are a [`ChannelSet`]: past
/// [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) channels no topology is held,
/// and the width is all it states.
pub(super) fn channels_of(channels: Option<&Channels>) -> (ChannelSet, Placement) {
    let unassigned = |count: usize, placement| {
        (
            ChannelSet::unassigned(ChannelLayout::clamped(count)),
            placement,
        )
    };
    let default = |count: usize| {
        (
            ChannelSet::default_for(ChannelLayout::clamped(count)),
            Placement::Default,
        )
    };
    let stated = |topology: ChannelTopology| (ChannelSet::from(topology), Placement::Stated);
    match channels {
        Some(channels) if channels.count() == 1 => stated(Bed::Mono.topology()),
        // A positioned set wider than a topology holds keeps its width and no topology.
        Some(all @ Channels::Positioned(positions)) => {
            match Speakers::from_format(positions.iter().map(speaker)) {
                Some(speakers) => stated(ChannelTopology::Speakers(speakers)),
                None => unassigned(all.count(), Placement::Stated),
            }
        }
        Some(all @ Channels::Ambisonic(order)) => match AmbisonicOrder::new(*order) {
            Some(order) => stated(AmbisonicFormat::ambix(order).into()),
            // Past third order no topology holds it; the width is kept for the caller to refuse.
            None => unassigned(all.count(), Placement::Stated),
        },
        // Labelled channels name no speaker this vocabulary reads.
        Some(Channels::Custom(labels)) => unassigned(labels.len(), Placement::Stated),
        Some(channels) if channels.count() > 0 => default(channels.count()),
        Some(_) | None => default(2),
    }
}

#[cfg(test)]
mod tests;
