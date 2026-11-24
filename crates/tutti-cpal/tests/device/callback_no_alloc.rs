//! The device driver's conversion allocates nothing.
//!
//! `OutputBlock`'s own allocation gate is `tutti-engine`'s
//! (`tests/no_alloc/block_no_alloc.rs`); this one runs the block through each
//! native sample type a device driver converts to (`render_delayed`), at the
//! `MAX_FRAMES` ceiling. The offset-binary formats' gate is beside the driver.

use crate::support::dc_state;
use tutti_buffers::ChannelLayout;
use tutti_buffers::Sample;
use tutti_engine::{MAX_FRAMES, OutputBlock};

/// Every native device sample type, rendered after a warm-up, allocates nothing
/// and carries the signal (a DC of 0.5 is not the format's silence), so the gate
/// does not pass over an empty block.
#[test]
fn the_conversion_to_every_device_format_is_allocation_free() {
    fn gate<S: Sample>(label: &str) {
        let mut block = OutputBlock::new(dc_state(0.5f32, 2), ChannelLayout::STEREO);
        let mut out = vec![S::SILENCE; MAX_FRAMES * 2];
        block.render_delayed(&mut out, None);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..8 {
                block.render_delayed(&mut out, None);
            }
        });
        assert!(
            out.iter().all(|s| *s != S::SILENCE),
            "{label} rendered silence"
        );
    }
    gate::<f32>("f32");
    gate::<f64>("f64");
    gate::<i8>("i8");
    gate::<i16>("i16");
    gate::<i32>("i32");
}
