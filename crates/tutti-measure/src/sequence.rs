//! Order statistics and peak picking over a sequence of values, shared by the
//! analyses that read a curve: the median of a set, and the local maxima of a
//! curve.

use core::ops::Range;

/// The median of values already sorted ascending by [`f64::total_cmp`]: the
/// middle value for an odd count and the mean of the middle two for an even
/// count. `None` for no values.
///
/// A NaN sorts above every number under `total_cmp`, so a median that lands on
/// one is NaN; the caller refuses a NaN it cannot tolerate before the sort.
pub(crate) fn median_of_sorted<T: Copy + Into<f64>>(sorted: &[T]) -> Option<f64> {
    let mid = sorted.len() / 2;
    let upper: f64 = (*sorted.get(mid)?).into();
    if sorted.len() % 2 == 1 {
        return Some(upper);
    }
    let lower: f64 = sorted[mid - 1].into();
    Some((lower + upper) / 2.0)
}

/// The median of `values` in any order, as [`median_of_sorted`]; `None` for no
/// values.
pub(crate) fn median<T: Copy + Into<f64>>(values: &[T]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable_by(|a, b| (*a).into().total_cmp(&(*b).into()));
    median_of_sorted(&sorted)
}

/// The indices in `within` that are local maxima of `values`: strictly above
/// the value before and not below the value after, so a plateau reports its
/// first index. The first value has no value before it, so it needs only
/// not to be below the next; the last has none after, so it needs only to be
/// above the one before. A value that compares false with a neighbour (a NaN) is
/// never a maximum. Indices at or past the end of `values` are skipped, and
/// neighbours outside `within` are still read.
pub(crate) fn local_maxima<T: PartialOrd + Copy>(
    values: &[T],
    within: Range<usize>,
) -> impl Iterator<Item = usize> + '_ {
    let end = within.end.min(values.len());
    (within.start..end.max(within.start)).filter(move |&i| {
        (i == 0 || values[i] > values[i - 1])
            && (i + 1 == values.len() || values[i] >= values[i + 1])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The median of a known odd set is its middle value, in any order.
    #[test]
    fn the_median_of_an_odd_set_is_its_middle_value() {
        assert_eq!(median(&[9.0f32, 1.0, 5.0, 3.0, 7.0]), Some(5.0));
        assert_eq!(median(&[4.0f64]), Some(4.0));
    }

    /// The median of a known even set is the mean of its middle two.
    #[test]
    fn the_median_of_an_even_set_is_the_mean_of_the_middle_two() {
        assert_eq!(median(&[4.0f32, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[10.0f64, 20.0]), Some(15.0));
        assert_eq!(median(&[-1.0f32, 1.0, 1.0, 5.0, 100.0, 7.0]), Some(3.0));
    }

    /// No values have no median.
    #[test]
    fn the_median_of_nothing_is_none() {
        assert_eq!(median::<f32>(&[]), None);
        assert_eq!(median_of_sorted::<f64>(&[]), None);
    }

    /// A NaN sorts above every number, so it moves the median only as the top
    /// value does.
    #[test]
    fn a_nan_sorts_last_and_moves_the_median_as_the_top_value() {
        assert_eq!(median(&[1.0f64, f64::NAN, 2.0, 3.0, 4.0]), Some(3.0));
        assert!(median(&[1.0f64, f64::NAN]).unwrap().is_nan());
    }

    /// A strict rise and a non-strict fall pick each peak once, and a plateau at
    /// its first index.
    #[test]
    fn a_plateau_reports_its_first_index() {
        let v = [0.0f32, 2.0, 1.0, 3.0, 3.0, 1.0, 0.5, 0.5, 2.0, 0.0];
        let all: Vec<usize> = local_maxima(&v, 0..v.len()).collect();
        assert_eq!(all, [1, 3, 8]);
    }

    /// An end needs only its one neighbour: the first not below it, the last above it.
    #[test]
    fn an_end_above_its_neighbour_is_a_maximum() {
        let v = [5.0f32, 1.0, 2.0, 4.0];
        assert_eq!(local_maxima(&v, 0..4).collect::<Vec<_>>(), [0, 3]);
        let flat = [1.0f32, 1.0];
        assert_eq!(local_maxima(&flat, 0..2).collect::<Vec<_>>(), [0]);
        assert_eq!(local_maxima(&[1.0f32], 0..1).collect::<Vec<_>>(), [0]);
    }

    /// The range limits what is reported, not what is compared: the neighbour
    /// outside the range still decides.
    #[test]
    fn the_range_bounds_the_report_not_the_neighbours() {
        let v = [9.0f32, 1.0, 5.0, 2.0, 5.0, 4.0];
        assert_eq!(local_maxima(&v, 1..5).collect::<Vec<_>>(), [2, 4]);
        assert_eq!(local_maxima(&v, 1..3).collect::<Vec<_>>(), [2]);
        assert_eq!(local_maxima(&v, 4..99).collect::<Vec<_>>(), [4]);
    }

    /// A NaN is no maximum and blocks its neighbours' rise or fall.
    #[test]
    fn a_nan_is_never_a_maximum() {
        let v = [0.0f32, f32::NAN, 1.0, 0.0];
        assert_eq!(
            local_maxima(&v, 0..4).collect::<Vec<_>>(),
            Vec::<usize>::new()
        );
    }
}
