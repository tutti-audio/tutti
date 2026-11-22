//! [`slope`]: the least-squares line through points.

use crate::Quantity;

/// The slope of the least-squares line through `points`, `y` against `x`:
/// `sum((x - mean x)(y - mean y)) / sum((x - mean x)^2)`, the ratio of a `y`
/// quantity to the unit `x` is counted in (a count, or a quantity's
/// [`get`](crate::Seconds::get)). It is a plain ratio: no unit covers a
/// quotient of two arbitrary quantities.
///
/// `None` for fewer than two points and for points with one `x` (no line
/// has a slope there).
///
/// # Examples
///
/// ```
/// use tutti_units::{Seconds, slope};
///
/// // Arrivals every 2 ms, from tick 0.
/// let ticks = (0..4).map(|i| (f64::from(i), Seconds::new(0.002 * f64::from(i))));
/// assert!((slope(ticks).unwrap() - 0.002).abs() < 1e-15);
///
/// // One point, or every point at one x, has no slope.
/// assert_eq!(slope([(1.0, Seconds::new(0.5))].into_iter()), None);
/// assert_eq!(slope([(1.0, Seconds::new(0.5)), (1.0, Seconds::new(0.7))].into_iter()), None);
/// ```
#[must_use]
pub fn slope<Y: Quantity>(points: impl Iterator<Item = (f64, Y)> + Clone) -> Option<f64> {
    let (n, sum_x, sum_y) = points.clone().fold((0.0, 0.0, 0.0), |(n, sx, sy), (x, y)| {
        (n + 1.0, sx + x, sy + y.value())
    });
    if n < 2.0 {
        return None;
    }
    let (mean_x, mean_y) = (sum_x / n, sum_y / n);
    let (cross, spread) = points.fold((0.0, 0.0), |(c, s), (x, y)| {
        let dx = x - mean_x;
        (c + dx * (y.value() - mean_y), s + dx * dx)
    });
    (spread > 0.0).then(|| cross / spread)
}
