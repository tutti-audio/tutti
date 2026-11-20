use super::*;
use tutti_fixtures::RangeContract;

/// The HTK mel of 700 Hz is `2595 log10(2)`: the argument of the logarithm is exactly 2.
#[test]
fn htk_mel_of_700_hz_is_2595_log10_2() {
    let m = MelScale::Htk.mel(Hz::new(700.0));
    assert!((m - 2595.0 * 2f64.log10()).abs() < 1e-12, "{m}");
}

/// Both scales pass through the origin.
#[test]
fn mel_of_zero_hz_is_zero_on_both_scales() {
    assert_eq!(MelScale::Htk.mel(Hz::ZERO), 0.0);
    assert_eq!(MelScale::Slaney.mel(Hz::ZERO), 0.0);
}

/// Slaney's scale is `f / (200/3)` below 1 kHz and `15 + 27 log_6.4(f / 1000)` above, so
/// 500 Hz is 7.5 mel, 1 kHz is 15 and 6.4 kHz is `15 + 27 = 42`.
#[test]
fn slaney_mel_hits_its_closed_form_at_the_knee_and_a_factor_6_4_above() {
    let at = |hz: f64| MelScale::Slaney.mel(Hz::new(hz));
    assert!((at(500.0) - 7.5).abs() < 1e-12);
    assert!((at(1000.0) - 15.0).abs() < 1e-12);
    assert!((at(6400.0) - 42.0).abs() < 1e-9, "{}", at(6400.0));
}

/// `frequency` inverts `mel` on both scales, across and either side of Slaney's knee.
#[test]
fn the_inverse_mel_round_trips_on_both_scales() {
    for scale in [MelScale::Htk, MelScale::Slaney] {
        for hz in [0.0, 50.0, 700.0, 999.0, 1000.0, 1001.0, 4000.0, 22_050.0] {
            let back = scale.frequency(scale.mel(Hz::new(hz))).get();
            assert!(
                (back - hz).abs() < 1e-9 * (1.0 + hz),
                "{scale:?} {hz}: {back}"
            );
        }
    }
}

/// A band's first bin and weights.
fn weights(bank: &MelFilterbank, band: usize) -> (BinIndex, Vec<f64>) {
    let (first, w) = bank.weights::<f64>(band).unwrap();
    (first, w.collect())
}

const WIDTH: f64 = 10.0;
const BINS: usize = 1601;

fn bank(bands: u16, scale: MelScale, norm: MelNorm) -> MelFilterbank {
    MelFilterbank::try_from(
        MelConfig::new(MelBandCount::of(bands), BinCount(BINS), Hz::new(WIDTH))
            .range(Hz::new(100.0), Hz::new(8000.0))
            .scale(scale)
            .norm(norm),
    )
    .unwrap()
}

/// The `bands + 2` edges are the inverse mel of equally spaced mels from `low` to `high`:
/// HTK's `700 (10^(m / 2595) - 1)`, computed here from the closed form.
#[test]
fn the_edges_are_the_inverse_mel_of_equally_spaced_mels() {
    let b = bank(20, MelScale::Htk, MelNorm::Peak);
    let mel = |f: f64| 2595.0 * (1.0 + f / 700.0).log10();
    let (lo, hi) = (mel(100.0), mel(8000.0));
    assert_eq!(b.edges().len(), 22);
    for (j, edge) in b.edges().iter().enumerate() {
        let m = lo + (hi - lo) * j as f64 / 21.0;
        let want = 700.0 * (10f64.powf(m / 2595.0) - 1.0);
        assert!(
            (edge.get() - want).abs() < 1e-9 * want.max(1.0),
            "{j}: {edge}"
        );
    }
}

/// The same on Slaney's scale at its two ends of the knee: edges equally spaced in
/// `f / (200/3)` below 1 kHz and in `ln f` above, so the mel gaps are all equal.
#[test]
fn slaney_edges_are_equally_spaced_in_slaney_mel() {
    let b = bank(24, MelScale::Slaney, MelNorm::Peak);
    let mels: Vec<f64> = b.edges().iter().map(|&e| MelScale::Slaney.mel(e)).collect();
    let step = (mels[25] - mels[0]) / 25.0;
    for w in mels.windows(2) {
        assert!((w[1] - w[0] - step).abs() < 1e-9, "{w:?} step {step}");
    }
    assert!((b.edges()[0].get() - 100.0).abs() < 1e-12);
    assert!((b.edges()[25].get() - 8000.0).abs() < 1e-12);
}

/// With a bin exactly on a centre frequency a Peak triangle is exactly 1 there. The bin
/// width is the centre of band 1 over 40, so bin 40 is that centre.
#[test]
fn a_peak_triangle_is_one_at_a_bin_on_its_centre() {
    let probe = bank(12, MelScale::Htk, MelNorm::Peak);
    let centre = probe.edges()[2].get();
    let width = centre / 40.0;
    let b = MelFilterbank::try_from(
        MelConfig::new(MelBandCount::of(12), BinCount(1601), Hz::new(width))
            .range(Hz::new(100.0), Hz::new(8000.0)),
    )
    .unwrap();
    let (first, weights) = weights(&b, 1);
    assert!((weights[40 - first.get()] - 1.0).abs() < 1e-9);
    assert!(weights.iter().all(|&w| w <= 1.0 + 1e-9));
}

/// Each triangle peaks at the bin nearest its centre, at a height within `h / 2` of the
/// centre of 1: a bin `d <= h/2` from the centre sits `d / half_width` below the peak, and
/// the narrower half is the steeper.
#[test]
fn each_triangle_peaks_at_its_centre_bin() {
    let b = bank(26, MelScale::Htk, MelNorm::Peak);
    for band in 0..26 {
        let (e0, c, e2) = (
            b.edges()[band].get(),
            b.edges()[band + 1].get(),
            b.edges()[band + 2].get(),
        );
        let (first, weights) = weights(&b, band);
        let (peak_at, peak) = weights
            .iter()
            .copied()
            .enumerate()
            .fold((0, 0.0), |a, (i, w)| if w > a.1 { (i, w) } else { a });
        let nearest = (c / WIDTH).round() as usize;
        assert!(first.get() + peak_at == nearest, "band {band}");
        let floor = 1.0 - (WIDTH / 2.0) / (c - e0).min(e2 - c);
        assert!(
            peak <= 1.0 && peak >= floor,
            "band {band}: {peak} < {floor}"
        );
    }
}

/// Filter `i` falls over the span filter `i + 1` rises over, from `i`'s centre to
/// `i + 1`'s: the two weights sum to exactly 1 at every bin in it, so they cross at half
/// height, at the span's midpoint.
#[test]
fn adjacent_triangles_sum_to_one_and_cross_at_half_height() {
    let b = bank(20, MelScale::Htk, MelNorm::Peak);
    for band in 0..19 {
        let (lo, hi) = (b.edges()[band + 1].get(), b.edges()[band + 2].get());
        let at = |band: usize, k: usize| {
            let (first, w) = weights(&b, band);
            k.checked_sub(first.get())
                .and_then(|i| w.get(i))
                .copied()
                .unwrap_or(0.0)
        };
        let mut crossing = (f64::MAX, 0.0);
        for k in (lo / WIDTH).ceil() as usize..=(hi / WIDTH).floor() as usize {
            let (a, c) = (at(band, k), at(band + 1, k));
            assert!((a + c - 1.0).abs() < 1e-12, "band {band} bin {k}");
            let mid = k as f64 * WIDTH - (lo + hi) / 2.0;
            if mid.abs() < crossing.0 {
                crossing = (mid.abs(), a);
            }
        }
        // The bin nearest the midpoint is within h / 2 of it, so its weights are within
        // (h / 2) / span of a half.
        let tolerance = (WIDTH / 2.0) / (hi - lo);
        assert!(
            (crossing.1 - 0.5).abs() <= tolerance + 1e-12,
            "band {band}: {}",
            crossing.1
        );
    }
}

/// `Sum` weights sum to 1 exactly. `Area` weights times the bin width sum to the triangle's
/// unit area to its sampling: the triangle is zero outside its base, so the sum is the
/// trapezoid rule, which a kink of slope change `d` leaves `d h^2 / 8` short at most. The
/// three kinks change the slope by `s1`, `s1 + s2` and `s2` (`s1 = peak / (c - l)`,
/// `s2 = peak / (u - c)`, `peak = 2 / (u - l)`), so the area is within
/// `(h^2 / 4) peak (1 / (c - l) + 1 / (u - c))` of 1.
#[test]
fn a_bands_weights_sum_to_its_stated_area() {
    let sum = bank(20, MelScale::Htk, MelNorm::Sum);
    let area = bank(20, MelScale::Htk, MelNorm::Area);
    for band in 0..20 {
        let total: f64 = weights(&sum, band).1.iter().sum();
        assert!((total - 1.0).abs() < 1e-12, "sum band {band}: {total}");
        let (l, c, u) = (
            area.edges()[band].get(),
            area.edges()[band + 1].get(),
            area.edges()[band + 2].get(),
        );
        let integral: f64 = weights(&area, band).1.iter().sum::<f64>() * WIDTH;
        let bound =
            WIDTH * WIDTH / 4.0 * (1.0 / (c - l) + 1.0 / (u - c)) * 2.0 / (u - l) / WIDTH * WIDTH;
        assert!(
            (integral - 1.0).abs() <= bound,
            "area band {band}: {integral} vs {bound}"
        );
    }
}

/// A Peak triangle's area is half its base: its weights sum to `(u - l) / 2h` to the same
/// sampling bound, so the `Area` scale of `2 / (u - l)` is what makes it unit.
#[test]
fn peak_and_area_weights_differ_by_two_over_the_base() {
    let peak = bank(10, MelScale::Slaney, MelNorm::Peak);
    let area = bank(10, MelScale::Slaney, MelNorm::Area);
    for band in 0..10 {
        let ratio = 2.0 / (peak.edges()[band + 2].get() - peak.edges()[band].get());
        for (p, a) in weights(&peak, band).1.iter().zip(weights(&area, band).1) {
            assert!((p * ratio - a).abs() < 1e-15);
        }
    }
}

/// A flat magnitude of `a` gives `a^2` in every band of a `Sum` bank: the weights sum to 1.
#[test]
fn a_sum_bank_reads_a_flat_spectrum_as_its_own_power() {
    let b = bank(30, MelScale::Htk, MelNorm::Sum);
    let mut energies = [0.0f64; 30];
    b.process(&vec![3.0f64; BINS], &mut energies);
    for e in energies {
        assert!((e - 9.0).abs() < 1e-12, "{e}");
    }
}

/// A band whose triangle misses every bin is refused by name: 400 bands in 100 Hz of a
/// 10 Hz grid cannot each hold a bin.
#[test]
fn a_band_with_no_bin_is_refused() {
    let err = MelFilterbank::try_from(
        MelConfig::new(MelBandCount::of(512), BinCount(BINS), Hz::new(WIDTH))
            .range(Hz::new(0.0), Hz::new(200.0)),
    )
    .unwrap_err();
    assert!(matches!(err, MelError::EmptyBand { .. }), "{err:?}");
}

/// A range that is empty, above the last bin, or on a bad grid is refused with its own error.
#[test]
fn bad_ranges_and_grids_are_refused() {
    let cfg = || MelConfig::new(MelBandCount::of(8), BinCount(BINS), Hz::new(WIDTH));
    let refused = |c: MelConfig| MelFilterbank::try_from(c).unwrap_err();
    assert!(matches!(
        refused(cfg().range(Hz::new(500.0), Hz::new(500.0))),
        MelError::EmptyRange { .. }
    ));
    assert!(matches!(
        refused(cfg().range(Hz::new(-1.0), Hz::new(500.0))),
        MelError::EmptyRange { .. }
    ));
    assert!(matches!(
        refused(cfg().range(Hz::new(0.0), Hz::new(f64::NAN))),
        MelError::EmptyRange { .. }
    ));
    assert!(matches!(
        refused(cfg().range(Hz::new(0.0), Hz::new(16_001.0))),
        MelError::AboveNyquist { .. }
    ));
    let bad = MelConfig::new(MelBandCount::of(8), BinCount(BINS), Hz::new(0.0));
    assert!(matches!(refused(bad), MelError::BinWidth(_)));
}

/// `MelBandCount` is the `MidiChannel` shape over `1..=512`.
#[test]
fn a_mel_band_count_keeps_the_range_checked_shape() {
    let c = RangeContract {
        new: MelBandCount::new,
        get: MelBandCount::get,
        of: Some(MelBandCount::of),
        valid: &[1, 40, 512],
        invalid: &[0, 513, u16::MAX],
    };
    tutti_fixtures::check_range_contract(&c);
}
