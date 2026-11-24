//! The block schedules a test renders under: a total cut into blocks of frames.

/// `total` frames in blocks of `size`, the remainder (when `size` does not divide `total`)
/// as the last block. Every block is non-zero and they sum to `total`.
///
/// # Panics
///
/// If `size` is zero.
pub fn fixed_blocks(size: usize, total: usize) -> Vec<usize> {
    assert!(size > 0, "a block has at least one frame");
    let mut blocks = vec![size; total / size];
    if !total.is_multiple_of(size) {
        blocks.push(total % size);
    }
    blocks
}

/// `total` frames in blocks that take `pattern`'s sizes in turn, over and over, each cut
/// down to the frames still left, so the last block may be shorter than its turn. Every
/// block is non-zero and they sum to `total`.
///
/// # Panics
///
/// If `pattern` is empty or holds a zero.
pub fn cycle_blocks(pattern: &[usize], total: usize) -> Vec<usize> {
    assert!(
        !pattern.is_empty() && pattern.iter().all(|&n| n > 0),
        "a pattern is non-empty and each of its blocks has at least one frame"
    );
    let mut blocks = Vec::new();
    let mut left = total;
    for &n in pattern.iter().cycle() {
        if left == 0 {
            break;
        }
        let n = n.min(left);
        blocks.push(n);
        left -= n;
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_cut_ends_on_the_remainder() {
        assert_eq!(fixed_blocks(3, 10), [3, 3, 3, 1]);
        assert_eq!(fixed_blocks(5, 10), [5, 5]);
        assert_eq!(fixed_blocks(1, 4), [1, 1, 1, 1]);
    }

    #[test]
    fn a_fixed_block_longer_than_the_total_is_the_total_alone() {
        assert_eq!(fixed_blocks(8, 5), [5]);
    }

    #[test]
    fn nothing_cuts_into_no_blocks() {
        assert!(fixed_blocks(4, 0).is_empty());
        assert!(cycle_blocks(&[3, 1], 0).is_empty());
    }

    #[test]
    fn a_cycle_repeats_its_pattern_and_clamps_the_last_block() {
        assert_eq!(cycle_blocks(&[3, 1, 2], 14), [3, 1, 2, 3, 1, 2, 2]);
        assert_eq!(cycle_blocks(&[3, 1, 2], 12), [3, 1, 2, 3, 1, 2]);
    }

    #[test]
    fn a_cycle_clamps_a_turn_to_what_is_left_mid_pattern() {
        assert_eq!(cycle_blocks(&[4, 1], 6), [4, 1, 1]);
        assert_eq!(cycle_blocks(&[9, 1], 5), [5]);
    }

    #[test]
    fn every_cut_sums_to_its_total_with_no_empty_block() {
        for total in 0..60 {
            for blocks in [
                fixed_blocks(1, total),
                fixed_blocks(7, total),
                fixed_blocks(64, total),
                cycle_blocks(&[1, 128, 7, 64, 127, 33, 2, 100], total),
            ] {
                assert_eq!(blocks.iter().sum::<usize>(), total);
                assert!(blocks.iter().all(|&n| n > 0));
            }
        }
    }

    #[test]
    #[should_panic(expected = "at least one frame")]
    fn a_zero_block_size_is_refused() {
        fixed_blocks(0, 4);
    }

    #[test]
    #[should_panic(expected = "non-empty")]
    fn a_pattern_with_a_zero_is_refused() {
        cycle_blocks(&[2, 0], 4);
    }
}
