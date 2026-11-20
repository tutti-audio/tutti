use super::*;

fn geo(window: usize, hop: usize) -> StftGeometry {
    StftGeometry::new(
        SampleRate::SR_44K1,
        WindowLength::new(Samples::new(window)).unwrap(),
        HopSize::new(Samples::new(hop)).unwrap(),
    )
}

#[test]
fn a_zero_window_and_a_zero_hop_are_not_values() {
    assert_eq!(WindowLength::new(Samples::new(0)), None);
    assert_eq!(HopSize::new(Samples::new(0)), None);
    assert_eq!(
        WindowLength::try_from(Samples::new(0)),
        Err(WindowLengthError(Samples::new(0)))
    );
    assert_eq!(
        HopSize::try_from(Samples::new(0)),
        Err(HopSizeError(Samples::new(0)))
    );
}

/// A hop wider than the window is legal for analysis and illegal for
/// reconstruction — decimated display grids sample sparsely on purpose.
#[test]
fn a_hop_wider_than_the_window_analyses_but_does_not_invert() {
    let sparse = geo(2048, 8192);
    assert!(!sparse.frames_overlap());

    assert_eq!(
        ColaGeometry::try_from(sparse),
        Err(StftError::HopExceedsWindow {
            window: Samples::new(2048),
            hop: Samples::new(8192),
        })
    );
}

/// The bug this split exists to prevent: a decimated hop is accepted for
/// analysis but refused for anything that has to invert.
#[test]
fn a_cola_conversion_refuses_what_new_accepts() {
    // 4x overlap: fine for both.
    assert!(ColaGeometry::try_from(geo(2048, 512)).is_ok());

    // 2x overlap: analysable, not invertible.
    assert_eq!(
        ColaGeometry::try_from(geo(2048, 1024)),
        Err(StftError::NotColaCompliant {
            window: Samples::new(2048),
            hop: Samples::new(1024),
            window_fn: CosineWindow::HANN,
        })
    );

    // Hop does not divide the window: analysable, not invertible.
    assert!(ColaGeometry::try_from(geo(2048, 500)).is_err());
}

/// **A grid that is COLA for one window is not automatically COLA for
/// another**, and this is the case that makes the conversion check the shape.
///
/// 2048/512 is 4x overlap: exact for Hann, and a 2.1% amplitude ripple for
/// Blackman, which needs 8x. Nothing about the grid changed — only the
/// shape laid over it.
#[test]
fn a_hann_cola_grid_may_not_be_cola_for_another_window() {
    assert!(ColaGeometry::try_from(geo(2048, 512)).is_ok());

    assert_eq!(
        ColaGeometry::try_from(geo(2048, 512).shape(CosineWindow::BLACKMAN)),
        Err(StftError::NotColaCompliant {
            window: Samples::new(2048),
            hop: Samples::new(512),
            window_fn: CosineWindow::BLACKMAN,
        }),
        "Blackman needs 8x overlap; 2048/512 is 4x"
    );

    // And it is accepted at the overlap it actually asks for.
    assert!(ColaGeometry::try_from(geo(2048, 256).shape(CosineWindow::BLACKMAN)).is_ok());
}

/// `shape` does **not** re-check COLA, and the doc says so — this pins
/// that, because a silently-invalidated grid is the hazard the
/// `ColaGeometry` conversion exists to give callers a way to avoid.
#[test]
fn the_shape_option_can_produce_a_non_cola_geo() {
    let hann = StftGeometry::from(ColaGeometry::try_from(geo(2048, 512)).expect("hann 4x"));
    assert!(hann.is_cola());

    let blackman = hann.shape(CosineWindow::BLACKMAN);
    assert!(
        !blackman.is_cola(),
        "shape is infallible and does not re-validate"
    );
}

/// The two window nouns do not collide: one is a length, one is a shape.
#[test]
fn a_grid_carries_both_a_window_length_and_a_window_shape() {
    let g = geo(2048, 512);
    assert_eq!(g.window(), Samples::new(2048));
    assert_eq!(g.window_fn(), CosineWindow::HANN);
    assert_eq!(g.window_coefficients::<f32>().len(), 2048);
}

/// **The magnitude gain follows the window shape, not just its length.**
///
/// This is what lets a display divide out the forward transform's window
/// gain without knowing which window produced it.
#[test]
fn magnitude_gain_tracks_the_window_shape() {
    let hann = geo(1024, 256);
    assert!(
        (hann.magnitude_gain::<f64>() - 512.0).abs() < 1e-9,
        "0.5 * 1024"
    );

    let blackman = hann.shape(CosineWindow::BLACKMAN);
    assert!(
        (blackman.magnitude_gain::<f64>() - 430.08).abs() < 1e-9,
        "0.42 * 1024, got {}",
        blackman.magnitude_gain::<f64>()
    );

    let rect = hann.shape(CosineWindow::RECTANGULAR);
    assert!(
        (rect.magnitude_gain::<f64>() - 1024.0).abs() < 1e-9,
        "1.0 * 1024"
    );
}

#[test]
fn bins_per_frame_counts_dc_through_nyquist() {
    let g = geo(2048, 512);
    assert_eq!(g.bins_per_frame(), BinCount(1025));
}

#[test]
fn bin_frequency_spans_dc_to_nyquist() {
    let g = geo(2048, 512);
    assert_eq!(g.bin_frequency(BinIndex(0)), Hz::ZERO);
    // Bin `window/2` is Nyquist.
    assert!((g.bin_frequency(BinIndex(1024)).get() - 22050.0).abs() < 1e-3);
    // Resolution is rate / window.
    assert!((g.bin_frequency(BinIndex(1)).get() - 44100.0 / 2048.0).abs() < 1e-3);
}

#[test]
fn frames_and_output_length_round_trip() {
    let g = geo(2048, 512);

    // Shorter than one window yields no frames at all.
    assert_eq!(g.frames_for(Samples::new(2047)), FrameCount(0));
    assert_eq!(g.output_len(FrameCount(0)), Samples::new(0));

    assert_eq!(g.frames_for(Samples::new(2048)), FrameCount(1));
    assert_eq!(g.output_len(FrameCount(1)), Samples::new(2048));

    // Each additional frame adds exactly one hop.
    assert_eq!(g.frames_for(Samples::new(2048 + 512)), FrameCount(2));
    assert_eq!(g.output_len(FrameCount(2)), Samples::new(2048 + 512));
}

#[test]
fn latency_is_one_window() {
    let g = geo(4410, 1102);
    assert!((g.latency().get() - 0.1).abs() < 1e-6);
}

/// The latency is the window over the rate in `f64`, with no `f32` on the
/// way: 2048 / 44100 = 0.046439909297052155 s, where an `f32` round trip
/// reads 0.04643991.
#[test]
fn latency_is_exact() {
    let g = geo(2048, 512);
    assert_eq!(g.latency(), Seconds::new(2048.0 / 44_100.0));
}

/// 441 Hz at 44100 Hz on 1000 points is exactly bin 10; the frequency of bin 10.5 is 463.05 Hz.
#[test]
fn a_frequency_on_a_bin_boundary_is_a_whole_bin() {
    let g = StftGeometry::new(
        SampleRate::SR_44K1,
        WindowLength::of(Samples::new(1000)),
        HopSize::of(Samples::new(250)),
    );
    assert_eq!(g.bin_of(Hz::new(441.0)), 10.0);
    assert_eq!(g.bin_of(Hz::new(441.0)).ceil(), 10.0);
    assert_eq!(g.frequency_of(10.5), Hz::new(463.05));
    let g = StftGeometry::new(
        SampleRate::SR_48K,
        WindowLength::of(Samples::new(1024)),
        HopSize::of(Samples::new(256)),
    );
    assert_eq!(g.bin_of(Hz::new(46.875)), 1.0);
    assert_eq!(g.frequency_of(3.0), Hz::new(140.625));
}

/// An edge a hair above a bin boundary lands past it: 441.0000001 Hz at 44.1 kHz over
/// 1000 points is bin 10.0000000023, whose ceiling is 11.
#[test]
fn an_edge_just_past_a_boundary_keeps_its_side() {
    let g = StftGeometry::new(
        SampleRate::SR_44K1,
        WindowLength::of(Samples::new(1000)),
        HopSize::of(Samples::new(250)),
    );
    assert_eq!(g.bin_of(Hz::new(441.000_000_1)).ceil(), 11.0);
}

/// A frame advances `hop / rate` seconds: 480 / 48000 = 0.01 s.
#[test]
fn the_frame_interval_is_the_hop_over_the_rate() {
    let g = StftGeometry::new(
        SampleRate::SR_48K,
        WindowLength::of(Samples::new(2048)),
        HopSize::of(Samples::new(480)),
    );
    assert!((g.frame_interval().get() - 0.01).abs() < 1e-15);
}
