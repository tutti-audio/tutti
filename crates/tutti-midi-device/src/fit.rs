//! [`slope`]: the least-squares line through arrival times.

use tutti_units::Seconds;

/// The slope of the least-squares line through `points`, a time `y` against
/// `x`: `sum((x - mean x)(y - mean y)) / sum((x - mean x)^2)`, in seconds per
/// unit of `x` (a tick count, or seconds for a clock's drift). It is a plain
/// ratio because `x`'s unit is the caller's.
///
/// `None` for fewer than two points and for points with one `x` (no line
/// has a slope there).
///
/// # Examples
///
/// ```
/// use tutti_midi_device::slope;
/// use tutti_units::Seconds;
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
pub fn slope(points: impl Iterator<Item = (f64, Seconds)> + Clone) -> Option<f64> {
    let (n, sum_x, sum_y) = points.clone().fold((0.0, 0.0, 0.0), |(n, sx, sy), (x, y)| {
        (n + 1.0, sx + x, sy + y.get())
    });
    if n < 2.0 {
        return None;
    }
    let (mean_x, mean_y) = (sum_x / n, sum_y / n);
    let (cross, spread) = points.fold((0.0, 0.0), |(c, s), (x, y)| {
        let dx = x - mean_x;
        (c + dx * (y.get() - mean_y), s + dx * dx)
    });
    (spread > 0.0).then(|| cross / spread)
}
