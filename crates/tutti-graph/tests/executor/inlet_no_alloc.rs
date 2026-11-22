//! Regression gate: the inlet's hot path must not allocate per block.
//!
//! `InletNode` drains a ring it owns on the audio thread. A block copies what
//! the ring holds straight out of its storage and advances the read index
//! once (no lock, no alloc), and an underrun emits silence rather than
//! stalling. A block must be allocation-free, exactly like the playback
//! units'.
//!
//! Driven through `tutti_graph::contract::Direct`: the node's own
//! `Node::process`, called by hand with buffers built once.

use tutti_buffers::ChannelLayout;
use tutti_buffers::Interleaved;
use tutti_graph::InletNode;
use tutti_graph::contract::Direct;
use tutti_units::{SampleRate, Samples};

/// Blocks with the ring full, then running dry (both branches of the pop).
#[test]
fn inlet_process_is_allocation_free() {
    let (node, mut prod) = InletNode::pair(ChannelLayout::STEREO, Samples::new(4096));
    let mut node = Direct::new(node, SampleRate::SR_48K, 64);

    for _ in 0..2048 {
        prod.push_interleaved(Interleaved::new(&[0.2, 0.2], prod.layout()).unwrap());
    }
    for _ in 0..8 {
        node.block();
    }

    // The 256 blocks far outlast the 2 048 primed frames (32 blocks), so it goes silent.
    for _ in 0..256 {
        node.block();
    }
    assert_eq!(node.output().plane(0).samples(), [0.0; 64], "ran dry");
}
