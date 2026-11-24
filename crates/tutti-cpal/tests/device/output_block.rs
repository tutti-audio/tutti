//! The sample-format conversion the device driver hands the output block, tested
//! without a sound card.
//!
//! The block renders the graph's float format; the driver's callback writes the
//! device's own sample type by the rule in `tutti_buffers::Sample`
//! (`OutputBlock::render_delayed`), the one rule the codecs share. An
//! offset-binary device is the same rule on the signed format of its width with
//! the sign bit flipped, tested beside the driver (`driver_seam`'s
//! `format_tests`). The clamp, the zero-fill and the width are the block's,
//! tested in `tutti-engine`.
//!
//! Expected values are stated from each format's own arithmetic, never by
//! calling the conversion under test.

use crate::support::dc_state;
use tutti_buffers::ChannelLayout;
use tutti_buffers::Sample;
use tutti_engine::OutputBlock;

/// A constant `level` graph rendered into a device buffer of `S`.
fn render<S: Sample>(level: f32) -> [S; 64] {
    let mut block = OutputBlock::new(dc_state(level, 2), ChannelLayout::STEREO);
    let mut out = [S::SILENCE; 64];
    block.render_delayed(&mut out, None);
    out
}

/// **A constant level through each native device format.**
///
/// The integer expectations are `0.5 * 2^(bits-1)`, with the positive extreme
/// held: `i16` gives 16384, and the ends of the range are `MIN` for -1.0 and
/// `MAX` for +1.0.
#[test]
fn every_native_format_carries_the_dc_level_the_sample_rule_gives() {
    assert!(render::<f32>(0.5).iter().all(|&s| s == 0.5));
    assert!(render::<f64>(0.5).iter().all(|&s| s == 0.5));
    assert!(render::<i16>(0.5).iter().all(|&s| s == 1 << 14));
    assert!(render::<i8>(0.5).iter().all(|&s| s == 1 << 6));
    assert!(render::<i32>(0.5).iter().all(|&s| s == 1 << 30));
    assert!(render::<i16>(-1.0).iter().all(|&s| s == i16::MIN));
    assert!(render::<i16>(1.0).iter().all(|&s| s == i16::MAX));
}

/// **Silence is the format's own zero.**
#[test]
fn silence_is_zero_in_every_signed_and_float_format() {
    assert!(render::<i16>(0.0).iter().all(|&s| s == 0));
    assert!(render::<i8>(0.0).iter().all(|&s| s == 0));
    assert!(render::<f32>(0.0).iter().all(|&s| s == 0.0));
}

/// **The device edge converts an `f64` graph by the same rule.**
#[test]
fn an_f64_graph_reaches_every_native_format_by_the_same_rule() {
    fn check<S: Sample + std::fmt::Debug>(expected: S, label: &str) {
        let mut block = OutputBlock::new(dc_state(0.5f64, 2), ChannelLayout::STEREO);
        let mut out = [S::SILENCE; 64];
        block.render_delayed(&mut out, None);
        assert!(
            out.iter().all(|s| *s == expected),
            "{label}: a constant 0.5 should be {expected:?}, got {:?}",
            &out[..4]
        );
    }
    check::<f64>(0.5, "f64");
    check::<f32>(0.5, "f32");
    check::<i16>(1 << 14, "i16");
    check::<i8>(1 << 6, "i8");
    check::<i32>(1 << 30, "i32");
}
