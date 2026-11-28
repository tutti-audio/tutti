//! Building a [`Voice`] from what a host has: a decoded wave, and the width a
//! file declares.

use std::sync::Arc;

use tutti_buffers::ChannelLayout;

use super::memory_source::{MemorySource, MemorySourceConfig, VoiceWindow};
use super::types::{Playback, Voice, VoiceSource};
use crate::SamplerFloat;
use crate::{PlaceError, Wave};

/// Builds an in-memory voice from a decoded wave.
///
/// The resident half of the tier decision, expressed as a function so
/// a host writes one line rather than assembling a `Voice` by hand. The disk
/// half needs a streamer round-trip and so is not a pure function: it goes
/// through a [`DiskStreamer`](crate::DiskStreamer).
///
/// # `window` is required, not defaulted
///
/// It is the clip's placement on the timeline. A voice built without one
/// would play at the default position regardless of where the clip is placed,
/// so it is taken here rather than defaulted.
///
/// # Errors
///
/// [`PlaceError`] when the wave cannot be placed into `width` channels: a file
/// with a channel no speaker names (a `Speaker::Unknown` in its topology) is
/// refused here rather than played with a channel in the wrong place or
/// dropped.
pub fn memory_voice<F: SamplerFloat>(
    wave: Arc<Wave<F>>,
    width: ChannelLayout,
    play: Playback,
    window: VoiceWindow,
) -> Result<Voice<F>, PlaceError> {
    let config = MemorySourceConfig::new(wave, width)?.window(window);
    let source = MemorySource::from(config);
    Ok(Voice {
        source: VoiceSource::Memory(source),
        play,
        // `None`: the streamer channel is a disk-tier concept. `Voice`'s own doc
        // says it is "meaningless for `Memory` (loop is primed directly on the
        // `MemorySource`)".
    })
}

/// The width a voice runs at, given what the file has: `file` itself, when the
/// sampler can carry it.
///
/// # Errors
///
/// [`PlaceError::EmptyWidth`] for no channels (a graph node that reports zero
/// outputs is one nothing can be wired to, though a layout can represent an
/// empty bus), and [`PlaceError::WidthTooWide`] past
/// [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS), the sampler's own per-read
/// stack ceiling, deliberately equal to the graph's main output's.
///
/// Deliberately not a function of the device width. The graph's main output folds
/// whatever reaches it, so a voice narrowed to the device here would be narrowed
/// *twice* on a wider project.
pub fn voice_width(file: ChannelLayout) -> Result<ChannelLayout, PlaceError> {
    match file.count() {
        0 => Err(PlaceError::EmptyWidth),
        n if n > crate::MAX_SAMPLER_WIDTH => Err(PlaceError::WidthTooWide {
            channels: n,
            max: crate::MAX_SAMPLER_WIDTH,
        }),
        _ => Ok(file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::MAX_CHANNELS;

    /// `voice_width` is the file's own width, refused outside `1..=MAX_CHANNELS`:
    /// the device is deliberately not consulted, or a voice would be narrowed
    /// twice on a wider project.
    #[test]
    fn voice_width_refuses_what_the_graph_cannot_carry() {
        let ceiling = MAX_CHANNELS as u16;
        assert_eq!(
            voice_width(ChannelLayout::STEREO),
            Ok(ChannelLayout::STEREO)
        );
        assert_eq!(
            voice_width(ChannelLayout::from(ceiling)),
            Ok(ChannelLayout::from(ceiling)),
            "the ceiling itself is carried"
        );
        assert_eq!(
            voice_width(ChannelLayout::from(ceiling + 1)),
            Err(PlaceError::WidthTooWide {
                channels: ceiling + 1,
                max: ceiling
            })
        );
        assert_eq!(
            voice_width(ChannelLayout::from(0u16)),
            Err(PlaceError::EmptyWidth)
        );
    }
}
