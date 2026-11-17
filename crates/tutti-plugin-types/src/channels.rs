//! Channel tables for a plugin's process call.
//!
//! # A plugin is a node, not an edge
//!
//! In the graph a plugin is a `tutti_graph::Node`: the graph's clock calls it one
//! block at a time, planar. The block a plugin renders is a
//! [`tutti_buffers::InputOutput`]; this module holds what builds one on the
//! realtime thread.
//!
//! Audio crosses the plugin boundary one buffer *per channel* (deinterleaved),
//! and the C plugin ABIs (VST3, AU) take those channel buffers as a `void**`.
//! Each format's `-ffi` crate builds that table from the views; this module
//! owns the safe side:
//!
//! - [`ChannelTable`]: a stack table of the channel slices of one direction, the
//!   backing a `Planes` or `PlanesMut` borrows;
//! - [`silence`] and [`sanitize`]: the zeroing a block's output gets.

use tutti_buffers::{BufferError, Float, Mono, MonoMut, Planes, PlanesMut, Sample, convert_planes};

/// The most channels, per direction, that a block's stack-built channel tables
/// hold (the `&[&[T]]` and `&mut [&mut [T]]` slice tables of the plugin server's
/// audio pipeline and of the in-process node's `process`).
///
/// 16 covers every realistic plugin layout: most are mono or stereo and
/// surround tops out at 7.1 (8 channels), so 16 leaves room for 7.1.4 (12) and
/// a pair of buses. A wider plugin is refused ([`PluginTooWide`]) where its
/// width is first known: a node over it is never built, and a served slab for
/// it is never mapped. The tables are fixed arrays so a block builds them
/// without allocating on the audio thread.
pub const MAX_BLOCK_CHANNELS: usize = 16;

/// A plugin with more than [`MAX_BLOCK_CHANNELS`] channels in a direction.
///
/// A width is refused, never narrowed: a node over a plugin that is handed
/// only some of its channels would run it on another layout than it declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("the plugin has {channels} {direction} channels, at most {MAX_BLOCK_CHANNELS} are served")]
pub struct PluginTooWide {
    /// `"input"` or `"output"`.
    pub direction: &'static str,
    /// The channels the plugin has in that direction, over all its buses.
    pub channels: usize,
}

/// The channel slices of one direction of a block, in a fixed table of
/// [`MAX_BLOCK_CHANNELS`] slots on the stack: `ChannelTable<&[T]>` for inputs,
/// `ChannelTable<&mut [T]>` for outputs. Collecting one allocates nothing,
/// which is what lets the audio thread build the tables of a block.
///
/// It reads as the slice of the channels collected. A table is a plain array
/// with no `Drop`, so a table of `&mut [T]` does not force its data to outlive
/// it, which an `ArrayVec` of them would.
pub struct ChannelTable<S> {
    slots: [S; MAX_BLOCK_CHANNELS],
    len: usize,
}

impl<'a, T: Sample> ChannelTable<&'a [T]> {
    /// The channels of `planes`, read-only: the table a format's channel
    /// pointers are built from.
    ///
    /// # Errors
    ///
    /// [`PluginTooWide`] for an `"input"` past [`MAX_BLOCK_CHANNELS`] channels.
    /// A plugin's width is checked at activation, so a block of an activated
    /// plugin is never refused here.
    pub fn of_planes(planes: Planes<'a, T>) -> Result<Self, PluginTooWide> {
        Self::fitting(planes.channels(), "input")?;
        Ok(planes.iter().map(|plane| plane.samples()).collect())
    }
}

impl<'a, T: Sample> ChannelTable<&'a mut [T]> {
    /// The channels of `planes`, writable, for as long as `planes` is borrowed.
    ///
    /// # Errors
    ///
    /// [`PluginTooWide`] for an `"output"` past [`MAX_BLOCK_CHANNELS`] channels,
    /// which activation's width check keeps an activated plugin's block from.
    pub fn of_planes_mut(planes: &'a mut PlanesMut<'_, '_, T>) -> Result<Self, PluginTooWide> {
        Self::fitting(planes.channels(), "output")?;
        Ok(planes
            .iter_mut()
            .map(tutti_buffers::MonoMut::into_samples_mut)
            .collect())
    }
}

impl<S> ChannelTable<S> {
    fn fitting(channels: usize, direction: &'static str) -> Result<(), PluginTooWide> {
        if channels > MAX_BLOCK_CHANNELS {
            return Err(PluginTooWide {
                direction,
                channels,
            });
        }
        Ok(())
    }
}

impl<S> std::fmt::Debug for ChannelTable<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelTable")
            .field("channels", &self.len)
            .finish_non_exhaustive()
    }
}

impl<S: Default> FromIterator<S> for ChannelTable<S> {
    /// # Panics
    ///
    /// When more than [`MAX_BLOCK_CHANNELS`] channels are collected: a channel
    /// is never dropped, and every caller's width was refused past the
    /// maximum where it was first known ([`PluginTooWide`]).
    fn from_iter<I: IntoIterator<Item = S>>(channels: I) -> Self {
        let mut slots: [S; MAX_BLOCK_CHANNELS] = std::array::from_fn(|_| S::default());
        let mut len = 0;
        for channel in channels {
            assert!(
                len < MAX_BLOCK_CHANNELS,
                "more than {MAX_BLOCK_CHANNELS} channels in a block"
            );
            slots[len] = channel;
            len += 1;
        }
        Self { slots, len }
    }
}

impl<S> std::ops::Deref for ChannelTable<S> {
    type Target = [S];

    fn deref(&self) -> &[S] {
        &self.slots[..self.len]
    }
}

impl<S> std::ops::DerefMut for ChannelTable<S> {
    fn deref_mut(&mut self) -> &mut [S] {
        &mut self.slots[..self.len]
    }
}

/// Converts one channel into another format by [`Sample`]'s rule, through
/// [`convert_planes`]: the step between a caller's block and a format's scratch,
/// whose channel counts need not agree.
///
/// # Errors
///
/// [`BufferError::LengthMismatch`] when the two differ in length; nothing is
/// written then.
pub fn convert_channel<A: Sample, B: Sample>(
    src: Mono<'_, A>,
    dst: MonoMut<'_, B>,
) -> Result<(), BufferError> {
    let from = [src.samples()];
    let mut to = [dst.into_samples_mut()];
    convert_planes(Planes::new(&from)?, PlanesMut::new(&mut to)?)
}

/// Zeroes the first `frames` samples of every channel (all of a shorter one).
/// Allocation-free and RT-safe.
pub fn silence<T: Float, C: AsMut<[T]>>(channels: &mut [C], frames: usize) {
    for channel in channels {
        let channel = channel.as_mut();
        let n = channel.len().min(frames);
        channel[..n].fill(T::ZERO);
    }
}

/// Replaces every NaN and infinite sample with zero and says whether any was
/// replaced.
///
/// A plugin that emits one non-finite sample would poison every node
/// downstream of it, so both the in-process node and the plugin server run each
/// block of plugin output through this before it leaves the plugin's stage.
/// Allocation-free and RT-safe.
pub fn sanitize<T: Float>(samples: &mut [T]) -> bool {
    let mut replaced = false;
    for s in samples {
        if !s.is_finite() {
            *s = T::ZERO;
            replaced = true;
        }
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::{ChannelTable, MAX_BLOCK_CHANNELS, convert_channel, sanitize, silence};
    use tutti_buffers::{Mono, MonoMut};

    #[test]
    fn a_channel_table_holds_the_channels_collected_in_order_without_allocating() {
        let data = [[1.0_f32; 4], [2.0; 4], [3.0; 4]];
        let table = assert_no_alloc::assert_no_alloc(|| {
            data.iter()
                .map(|c| c.as_slice())
                .collect::<ChannelTable<&[f32]>>()
        });
        assert_eq!(table.len(), 3);
        assert_eq!(table[2], &[3.0; 4][..]);
    }

    #[test]
    #[should_panic(expected = "channels in a block")]
    fn a_channel_table_refuses_a_channel_past_the_maximum_instead_of_dropping_it() {
        let data = [0.0_f32; 1];
        let _ = (0..=MAX_BLOCK_CHANNELS)
            .map(|_| data.as_slice())
            .collect::<ChannelTable<&[f32]>>();
    }

    #[test]
    fn silence_zeroes_the_first_frames_of_every_channel_and_spares_the_rest() {
        let (mut a, mut b) = ([1.0_f64; 4], [1.0_f64; 2]);
        silence(&mut [&mut a[..], &mut b[..]], 3);
        assert_eq!(a, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(b, [0.0, 0.0]);
    }

    #[test]
    fn a_channel_converts_by_the_sample_rule_and_an_unequal_pair_is_refused() {
        let src = [0.5_f32, -1.0, 2.0];
        let mut wide = [0.0_f64; 3];
        convert_channel(Mono::new(&src), MonoMut::new(&mut wide)).expect("equal lengths");
        assert_eq!(wide, [0.5, -1.0, 2.0]);
        let mut short = [7_i16; 2];
        assert!(convert_channel(Mono::new(&src), MonoMut::new(&mut short)).is_err());
        assert_eq!(short, [7, 7], "nothing is written on a refusal");
    }

    #[test]
    fn sanitize_zeroes_nan_and_both_infinities_and_keeps_finite_samples() {
        let mut block = [
            0.5_f32,
            f32::NAN,
            -0.25,
            f32::INFINITY,
            f32::NEG_INFINITY,
            0.0,
        ];
        assert!(sanitize(&mut block));
        assert_eq!(block, [0.5, 0.0, -0.25, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn sanitize_reports_nothing_replaced_for_a_finite_f64_block() {
        let mut block = [1.0_f64, -1.0, f64::MIN_POSITIVE, f64::MAX];
        assert!(!sanitize(&mut block));
        assert_eq!(block, [1.0, -1.0, f64::MIN_POSITIVE, f64::MAX]);
        let mut bad = [f64::NAN];
        assert!(sanitize(&mut bad));
        assert_eq!(bad, [0.0]);
    }

    #[test]
    fn a_table_of_planes_past_the_maximum_is_a_typed_refusal_not_a_panic() {
        let data = [0.0_f32; 2];
        let slices = vec![&data[..]; MAX_BLOCK_CHANNELS + 1];
        let planes = tutti_buffers::Planes::new(&slices).expect("equal planes");
        let refused = ChannelTable::of_planes(planes).expect_err("17 channels");
        assert_eq!(
            refused,
            crate::PluginTooWide {
                direction: "input",
                channels: MAX_BLOCK_CHANNELS + 1
            }
        );
    }
}
