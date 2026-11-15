//! The device driver's conversion allocates nothing.
//!
//! `OutputBlock`'s own allocation gate is `tutti-engine`'s
//! (`tests/no_alloc/block_no_alloc.rs`); this one runs the block through each
//! sample type a device driver converts to, `T::from_sample` as the callback
//! passes it, at the `MAX_FRAMES` ceiling.

use crate::support::dc_state;
use tutti_engine::{MAX_FRAMES, OutputBlock};
use tutti_types::ChannelLayout;

/// Every device sample type, rendered after a warm-up, allocates nothing and
/// carries the signal (a DC of 0.5 is not the format's silence), so the gate
/// does not pass over an empty block.
#[test]
fn the_conversion_to_every_device_format_is_allocation_free() {
    fn gate<T: cpal::SizedSample + cpal::FromSample<f32> + PartialEq>(label: &str) {
        let mut block = OutputBlock::new(dc_state(0.5, 2), ChannelLayout::STEREO);
        let mut out = vec![T::from_sample(0.0f32); MAX_FRAMES * 2];
        block.render_with(&mut out, T::from_sample);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..8 {
                block.render_with(&mut out, T::from_sample);
            }
        });
        let silence = T::from_sample(0.0f32);
        assert!(
            out.iter().all(|s| *s != silence),
            "{label} rendered silence"
        );
    }
    gate::<f32>("f32");
    gate::<f64>("f64");
    gate::<i8>("i8");
    gate::<i16>("i16");
    gate::<i32>("i32");
    gate::<u8>("u8");
    gate::<u16>("u16");
    gate::<u32>("u32");
}
