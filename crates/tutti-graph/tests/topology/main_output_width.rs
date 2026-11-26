//! What a refused main output says.

use tutti_buffers::{FoldError, Speaker};
use tutti_graph::MainOutputError;

#[test]
fn a_main_output_error_says_what_was_refused() {
    let folded = MainOutputError::from(FoldError::Unplaced {
        channel: 2,
        speaker: Speaker::Unassigned,
    });
    assert_eq!(
        folded.to_string(),
        "source channel 2 (Unassigned) has no place in the destination"
    );
}
