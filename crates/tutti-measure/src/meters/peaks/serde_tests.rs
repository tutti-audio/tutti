use tutti_buffers::ChannelLayout;
use tutti_units::Amplitude;

use super::{PeakBlock, PeakBlocks};

fn block(v: f64) -> PeakBlock {
    PeakBlock {
        min: -v,
        max: v,
        rms: Amplitude::new(v / 2.0),
    }
}

/// A stereo summary of three blocks per channel reads back equal, each channel in its
/// place.
#[test]
fn peak_blocks_round_trip_through_serde() {
    let blocks = PeakBlocks {
        blocks: (1..=6).map(|i| block(i as f64 / 8.0)).collect(),
        layout: ChannelLayout::STEREO,
        per_channel: 3,
    };
    let text = serde_json::to_string(&blocks).expect("serializes");
    let back: PeakBlocks = serde_json::from_str(&text).expect("reads back");
    assert_eq!(back, blocks);
    assert_eq!(back.channel(1).map(|c| c[0]), Some(block(0.5)));
}

/// Stored blocks that are not channels times blocks per channel long are refused: five
/// blocks cannot be two channels of three.
#[test]
fn stored_blocks_of_the_wrong_length_are_refused() {
    let blocks = PeakBlocks {
        blocks: (1..=6).map(|i| block(i as f64 / 8.0)).collect(),
        layout: ChannelLayout::STEREO,
        per_channel: 3,
    };
    let mut value = serde_json::to_value(&blocks).expect("serializes");
    value["blocks"]
        .as_array_mut()
        .expect("blocks are a list")
        .pop();
    assert!(serde_json::from_value::<PeakBlocks>(value).is_err());
}

/// No channels with blocks per channel is refused (the empty summary has none of
/// either), and too many blocks for the layout is refused as too few is.
#[test]
fn stored_blocks_over_no_channels_or_too_many_are_refused() {
    let over_none = r#"{"blocks":[],"layout":0,"per_channel":5}"#;
    assert!(serde_json::from_str::<PeakBlocks>(over_none).is_err());
    let empty = r#"{"blocks":[],"layout":0,"per_channel":0}"#;
    assert_eq!(
        serde_json::from_str::<PeakBlocks>(empty).expect("the empty summary"),
        PeakBlocks::default()
    );
    let blocks = PeakBlocks {
        blocks: vec![block(0.5); 2],
        layout: ChannelLayout::MONO,
        per_channel: 2,
    };
    let mut value = serde_json::to_value(&blocks).expect("serializes");
    value["blocks"]
        .as_array_mut()
        .expect("blocks are a list")
        .push(serde_json::to_value(block(0.25)).expect("a block"));
    assert!(serde_json::from_value::<PeakBlocks>(value).is_err());
}
