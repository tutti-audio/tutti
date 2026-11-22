//! The device edge's conversion, run through the driver's own render functions:
//! every device format is the rule in `Sample`, an offset-binary one through the
//! signed format of its width with the sign bit flipped.

use super::*;
use std::sync::Arc;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_engine::{AudioCallbackState, Engine, MAX_FRAMES};
use tutti_graph::{Editor, NodeKey, OutputPort, PortSource, Prepare};
use tutti_nodes::testing::Const;
use tutti_timeline::Transport;
use tutti_units::Samples;

/// A block over a graph holding `level` on both channels of a stereo device.
fn block(level: f32) -> OutputBlock<f32> {
    let rate = SampleRate::SR_48K;
    let transport = Transport::new(rate);
    let (mut ed, exec) = Editor::new(Prepare::new(rate, MaxBlock::of(Samples::new(512))));
    ed.insert(NodeKey(1), "dc", Const::mono(level));
    ed.spec_mut().topology.set_outputs(vec![
        PortSource::Node(OutputPort {
            node: NodeKey(1),
            port: 0
        });
        2
    ]);
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");
    Box::leak(Box::new(ed));
    OutputBlock::new(
        Arc::new(AudioCallbackState::new(engine)),
        ChannelLayout::STEREO,
    )
}

fn offset_of<U: OffsetBinary>(level: f32) -> [U; 8] {
    let mut out = [U::from_signed(<U::Native as Sample>::SILENCE); 8];
    offset(&mut block(level), &mut out, None);
    out
}

/// **An offset-binary device is mid-scale silence, and `Sample`'s rule above
/// and below it.** `u16`: 0.0 is 32768, 0.5 is `32768 + 2^14`, -1.0 is 0 and
/// +1.0 holds at `u16::MAX`; `u8` and `u32` likewise.
#[test]
fn an_offset_binary_device_is_the_sample_rule_with_the_sign_bit_flipped() {
    assert!(offset_of::<u16>(0.0).iter().all(|&s| s == 32768));
    assert!(offset_of::<u16>(0.5).iter().all(|&s| s == 49152));
    assert!(offset_of::<u16>(-1.0).iter().all(|&s| s == 0));
    assert!(offset_of::<u16>(1.0).iter().all(|&s| s == u16::MAX));
    assert!(offset_of::<u8>(0.0).iter().all(|&s| s == 128));
    assert!(offset_of::<u8>(0.5).iter().all(|&s| s == 128 + (1 << 6)));
    assert!(offset_of::<u32>(0.0).iter().all(|&s| s == 1 << 31));
    assert!(
        offset_of::<u32>(0.5)
            .iter()
            .all(|&s| s == (1 << 31) + (1 << 30))
    );
}

/// The flip is its own inverse over the whole range of each width.
#[test]
fn the_sign_bit_flip_round_trips() {
    for x in [0u8, 1, 127, 128, 255] {
        assert_eq!(u8::from_signed(x.to_signed()), x);
    }
    for x in [0u16, 1, 32767, 32768, u16::MAX] {
        assert_eq!(u16::from_signed(x.to_signed()), x);
    }
    for x in [0u32, 1, (1 << 31) - 1, 1 << 31, u32::MAX] {
        assert_eq!(u32::from_signed(x.to_signed()), x);
    }
}

/// The native path writes `Sample`'s figures and the offset one allocates
/// nothing at the callback ceiling.
#[test]
fn the_offset_binary_conversion_is_allocation_free_and_carries_the_signal() {
    let mut b = block(0.5);
    let mut out = vec![0u16; MAX_FRAMES * 2];
    offset(&mut b, &mut out, None);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..8 {
            offset(&mut b, &mut out, None);
        }
    });
    assert!(out.iter().all(|&s| s == 49152));
    let mut native_out = [0i16; 8];
    native(&mut block(0.5), &mut native_out, None);
    assert_eq!(native_out, [1 << 14; 8]);
}
