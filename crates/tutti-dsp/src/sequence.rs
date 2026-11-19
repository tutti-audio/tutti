//! Order statistics and peak picking over a sequence of values: the median of a
//! set, and the local maxima of a curve.

use core::ops::Range;

/// The median of values already sorted ascending by [`f64::total_cmp`]: the
/// middle value for an odd count and the mean of the middle two for an even
/// count. `None` for no values.
///
/// A NaN sorts above every number under `total_cmp`, so a median that lands on
/// one is NaN; the caller refuses a NaN it cannot tolerate before the sort.
pub fn median_of_sorted<T: Copy + Into<f64>>(sorted: &[T]) -> Option<f64> {
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
pub fn median<T: Copy + Into<f64>>(values: &[T]) -> Option<f64> {
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
pub fn local_maxima<T: PartialOrd + Copy>(
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
mod tests;
