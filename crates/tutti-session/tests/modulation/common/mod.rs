//! Shared test nodes.

#![allow(dead_code)]

use tutti_buffers::{ChannelLayout, InterleavedMut, MaxBlock};
use tutti_modulation::{Adsr, NoteEnvelope};
use tutti_session::Session;
use tutti_units::{Ballistics, Samples, Seconds};

pub(crate) mod bands;
pub(crate) mod probe;

/// A stereo offline session with 256-frame blocks.
pub(crate) fn session() -> Session {
    Session::builder()
        .outputs(ChannelLayout::STEREO)
        .max_block(MaxBlock::of(Samples::new(256)))
        .build()
        .expect("builds")
}

/// Output channel 0 (the left of the stereo main output) for `blocks` blocks of `block` frames.
pub(crate) fn render(s: &mut Session, blocks: usize, block: usize) -> Vec<f32> {
    let mut all = Vec::new();
    let mut buf = vec![0.0f32; block * 2];
    for _ in 0..blocks {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        all.extend(buf.as_chunks::<2>().0.iter().map(|f| f[0]));
    }
    all
}

/// A follower's usual times: a 5 ms attack and a 100 ms release.
pub(crate) fn meter() -> Ballistics {
    Ballistics::new(Seconds::new(0.005), Seconds::new(0.1)).expect("valid times")
}

/// The note envelope of `adsr`, whose stages the test has made valid.
pub(crate) fn envelope(adsr: Adsr) -> NoteEnvelope {
    NoteEnvelope::new(adsr).expect("valid stages")
}
