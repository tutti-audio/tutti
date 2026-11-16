//! Responses cross the conversion unchanged, receiver 0 as the left ear.

use tutti_hrir_convert::convert;

use crate::support::{Sofa, TAPS, read_back, tap};

#[test]
fn responses_come_through_bit_for_bit_with_receiver_zero_as_left() {
    let written = read_back(&convert(&Sofa::octahedron().write("bits")).expect("converts"));
    assert_eq!(written.rate, 48_000);
    assert_eq!(written.len, TAPS);
    for m in 0..6 {
        for n in 0..TAPS {
            assert_eq!(
                written.left[m][n].to_bits(),
                (tap(m, 0, n) as f32).to_bits()
            );
            assert_eq!(
                written.right[m][n].to_bits(),
                (tap(m, 1, n) as f32).to_bits()
            );
        }
    }
}

#[test]
fn the_sampling_rate_is_the_files() {
    let mut sofa = Sofa::octahedron();
    sofa.rate = vec![44_100.0];
    assert_eq!(
        read_back(&convert(&sofa.write("rate")).expect("converts")).rate,
        44_100
    );
}
