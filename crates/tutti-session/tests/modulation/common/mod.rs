//! Shared test nodes.

#![allow(dead_code)]

use tutti_buffers::{ChannelLayout, InterleavedMut, MaxBlock};
use tutti_session::Session;
use tutti_units::Samples;

pub mod bands;
pub mod probe;

/// A stereo offline session with 256-frame blocks.
pub fn session() -> Session {
    Session::builder()
        .outputs(ChannelLayout::STEREO)
        .max_block(MaxBlock::of(Samples::new(256)))
        .build()
        .expect("builds")
}

/// Output channel 0 (the left of the stereo main output) for `blocks` blocks of 128 frames.
pub fn render(s: &mut Session, blocks: usize) -> Vec<f32> {
    let mut all = Vec::new();
    let mut buf = vec![0.0f32; 128 * 2];
    for _ in 0..blocks {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        all.extend(buf.as_chunks::<2>().0.iter().map(|f| f[0]));
    }
    all
}
