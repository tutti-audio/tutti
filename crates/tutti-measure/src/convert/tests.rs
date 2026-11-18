use super::*;

/// Every run is whole frames and the runs, joined, are the data converted in
/// order: for odd and even widths over a block, and for a frame wider than
/// the stack block. A run cut off a frame boundary fails the first check.
#[test]
fn runs_are_whole_frames_that_join_to_the_converted_data() {
    for channels in [1usize, 3, 6, BLOCK + 3] {
        let frames = if channels > BLOCK { 3 } else { 5000 };
        let data: Vec<i16> = (0..channels * frames)
            .map(|i| (i % 30_000) as i16)
            .collect();
        let mut joined = Vec::new();
        for_blocks(&data, channels, |run| {
            assert_eq!(run.len() % channels, 0, "{channels} channels");
            assert!(!run.is_empty());
            joined.extend_from_slice(run);
        });
        let want: Vec<f32> = data.iter().map(|s| s.convert_to::<f32>()).collect();
        assert_eq!(joined, want, "{channels} channels");
    }
}
