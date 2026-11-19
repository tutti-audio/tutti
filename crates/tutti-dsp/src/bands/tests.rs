use super::*;

/// Neighbouring log-band edges have one ratio, and the ends are the range's.
#[test]
fn log_edges_share_one_ratio_from_the_low_edge_to_the_high() {
    let edges = log_band_edges(Hz::new(20.0), Hz::new(20_480.0), 10);
    assert_eq!(edges.len(), 11);
    assert_eq!(edges[0], Hz::new(20.0));
    assert!((edges[10].get() - 20_480.0).abs() < 1e-6);
    // 20480 / 20 = 2^10, so each band is one octave.
    for pair in edges.windows(2) {
        assert!((pair[1].get() / pair[0].get() - 2.0).abs() < 1e-12);
    }
}

/// No bands have no edges, not a NaN one.
#[test]
fn zero_log_bands_have_no_edges() {
    assert!(log_band_edges(Hz::new(20.0), Hz::new(20_000.0), 0).is_empty());
}

/// Octave edges are `lo * 2^(j / n)`, and a band that would end past the
/// top is left off: 100..=1000 holds `floor(3 * log2(10)) = 9` third-octave
/// bands.
#[test]
fn octave_edges_count_whole_bands_only() {
    let edges = octave_band_edges(Hz::new(100.0), Hz::new(1000.0), 3);
    assert_eq!(edges.len(), 10);
    for (j, e) in edges.iter().enumerate() {
        assert!(
            (e.get() - 100.0 * 2f64.powf(j as f64 / 3.0)).abs() < 1e-9,
            "{j}"
        );
    }
    assert!(edges[9].get() <= 1000.0);
    // The next band would end above the top.
    assert!(100.0 * 2f64.powf(10.0 / 3.0) > 1000.0);
}

/// An exact whole number of bands keeps its last edge despite the rounding of
/// the logarithm: 100..=400 is two octaves exactly.
#[test]
fn an_exact_octave_multiple_keeps_its_top_band() {
    assert_eq!(
        octave_band_edges(Hz::new(100.0), Hz::new(400.0), 1).len(),
        3
    );
    assert_eq!(
        octave_band_edges(Hz::new(100.0), Hz::new(400.0), 6).len(),
        13
    );
}

/// A range narrower than one band, or no bands per octave, has no edges.
#[test]
fn a_range_narrower_than_a_band_has_no_octave_edges() {
    assert!(octave_band_edges(Hz::new(100.0), Hz::new(150.0), 1).is_empty());
    assert!(octave_band_edges(Hz::new(100.0), Hz::new(1000.0), 0).is_empty());
}
