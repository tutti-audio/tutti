//! A whole-sample delay is zeros in front of its ear's response.

use tutti_hrir_convert::convert;

use crate::support::{Sofa, TAPS, read_back, tap};

/// One delay per receiver for every measurement: 3 samples on the right
/// ear. Every response grows to the longest, so the left ear's ends in
/// three zeros and the right ear's begins with three.
#[test]
fn a_whole_sample_delay_prepends_that_many_zeros() {
    let mut sofa = Sofa::octahedron();
    sofa.delay = Some((vec![1, 2], vec![0.0, 3.0]));
    let written = read_back(&convert(&sofa.write("delay_shared")).expect("converts"));
    assert_eq!(written.len, TAPS + 3);
    for m in 0..6 {
        let left: Vec<f32> = (0..TAPS)
            .map(|n| tap(m, 0, n) as f32)
            .chain([0.0; 3])
            .collect();
        let right: Vec<f32> = [0.0; 3]
            .into_iter()
            .chain((0..TAPS).map(|n| tap(m, 1, n) as f32))
            .collect();
        assert_eq!(written.left[m], left, "measurement {m}, left");
        assert_eq!(written.right[m], right, "measurement {m}, right");
    }
}

/// Delays per measurement: measurement `m` delays its left ear by `m` and
/// its right by 0.
#[test]
fn a_delay_per_measurement_applies_to_its_own_responses() {
    let mut sofa = Sofa::octahedron();
    sofa.delay = Some((vec![6, 2], (0..6).flat_map(|m| [m as f64, 0.0]).collect()));
    let written = read_back(&convert(&sofa.write("delay_each")).expect("converts"));
    assert_eq!(written.len, TAPS + 5);
    for m in 0..6 {
        assert!(
            written.left[m][..m].iter().all(|&s| s == 0.0),
            "measurement {m}"
        );
        assert_eq!(written.left[m][m], tap(m, 0, 0) as f32, "measurement {m}");
        assert_eq!(written.right[m][0], tap(m, 1, 0) as f32, "measurement {m}");
    }
}
