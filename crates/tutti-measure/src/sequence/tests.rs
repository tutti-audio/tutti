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
