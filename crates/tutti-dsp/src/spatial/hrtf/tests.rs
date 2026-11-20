use super::*;
use tutti_buffers::{MonoMut, StereoPlanes};
use tutti_units::{Azimuth, Elevation};

/// A tetrahedron with delta responses: the smallest sphere that parses.
fn tetrahedron(rate: u32) -> Vec<u8> {
    let mut b = b"HRIR".to_vec();
    for word in [rate, 4, 4, 12] {
        b.extend_from_slice(&word.to_le_bytes());
    }
    for idx in [0u32, 1, 2, 0, 3, 1, 0, 2, 3, 1, 3, 2] {
        b.extend_from_slice(&idx.to_le_bytes());
    }
    for v in [
        [1.0f32, 1.0, 1.0],
        [-1.0, -1.0, 1.0],
        [-1.0, 1.0, -1.0],
        [1.0, -1.0, -1.0],
    ] {
        for x in v
            .into_iter()
            .chain([1.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0])
        {
            b.extend_from_slice(&x.to_le_bytes());
        }
    }
    b
}

/// The parsed [`tetrahedron`] sphere at `rate` Hz.
fn sphere(rate: u32) -> Arc<HrirSphere> {
    Arc::new(HrirSphere::from_bytes(&tetrahedron(rate)).expect("parses"))
}

/// A clone (the fork's template, and every fork) shares the parsed
/// sphere and its prepared spectra; only a new rate prepares a new set,
/// and a re-prepare at the same rate keeps the one it has.
#[test]
fn a_clone_shares_the_sphere_and_its_spectra() {
    let rate = SampleRate::SR_48K;
    let front = Direction::new(Azimuth::FRONT, Elevation::LEVEL);
    let mut a = HrtfBinaural::<f32>::new(sphere(48_000), rate, front);
    let b = a.fresh(front);
    assert!(Arc::ptr_eq(&a.sphere, &b.sphere));
    assert!(Arc::ptr_eq(&a.set, &b.set));

    a.prepare(rate, || front);
    assert!(Arc::ptr_eq(&a.set, &b.set), "a same-rate prepare rebuilt");
    a.prepare(SampleRate::SR_96K, || front);
    assert!(Arc::ptr_eq(&a.sphere, &b.sphere));
    assert!(!Arc::ptr_eq(&a.set, &b.set));
    assert_eq!(a.set.rate, 96_000);
}

/// Two graph rates above [`MAX_PREPARED_RATE`] share one prepared set, keyed
/// at the ceiling; a set above it holds at most 50 ms at the ceiling.
#[test]
fn a_prepare_above_the_ceiling_keeps_one_set() {
    let front = Direction::new(Azimuth::FRONT, Elevation::LEVEL);
    let mut a = HrtfBinaural::<f32>::new(sphere(48_000), SampleRate::of(1_536_000.0), front);
    let set = a.set.clone();
    a.prepare(SampleRate::of(1_024_000.0), || front);
    assert!(Arc::ptr_eq(&a.set, &set), "both rates key to one set");
    assert_eq!(a.set.rate, MAX_PREPARED_RATE);
}

/// A direction that is NaN, infinite or zero finds no face and takes a
/// real vertex: the weights name vertices the sphere has and sum to one.
/// The renderer is then fed NaN and infinite targets (the node holds the
/// last finite one, so only the panner sees them; a zero direction
/// cannot come from angles, so only the weights are checked with it) and
/// must keep producing finite, non-silent audio: the tetrahedron's
/// deltas pass the constant input through at every vertex.
#[test]
fn a_degenerate_direction_takes_a_real_vertex_and_renders_finite_audio() {
    let rate = SampleRate::SR_48K;
    let mut p = HrtfBinaural::<f32>::new(
        sphere(48_000),
        rate,
        Direction::new(Azimuth::FRONT, Elevation::LEVEL),
    );
    let mut hint = 0;
    for dir in [
        [f64::NAN; 3],
        [f64::INFINITY, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [f64::NAN, 1.0, 0.0],
    ] {
        let w = p.set.weights(dir, &mut hint);
        assert!(w.verts.iter().all(|&v| (v as usize) < 4), "{dir:?}: {w:?}");
        assert!(w.w.iter().all(|x| x.is_finite()) && (w.w.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }
    let input = [0.5f32; 64];
    let (mut left, mut right) = ([0.0f32; 64], [0.0f32; 64]);
    for target in [
        Direction::new(Azimuth::new(f64::NAN), Elevation::new(f64::NAN)),
        Direction::new(Azimuth::new(f64::INFINITY), Elevation::new(0.0)),
    ] {
        let mut peak = 0.0f32;
        for _ in 0..20 {
            p.process(
                Mix::WET,
                StereoPlanes::new(&input, &input).unwrap(),
                (MonoMut::new(&mut left), MonoMut::new(&mut right)),
                || target,
            );
            assert!(left.iter().chain(&right).all(|s| s.is_finite()));
            peak = left.iter().chain(&right).fold(peak, |m, s| m.max(s.abs()));
        }
        assert!(peak > 0.1, "{target:?} rendered silence (peak {peak})");
    }
}

/// In `f64`, a sphere of delta responses (left 1, right 0.5) renders the mono input delayed by
/// one step, scaled by each ear's gain: every blend of the vertices' deltas is that delta.
#[test]
fn an_f64_render_is_the_input_delayed_one_step_and_scaled_per_ear() {
    let front = Direction::new(Azimuth::FRONT, Elevation::LEVEL);
    let mut p = HrtfBinaural::<f64>::new(sphere(48_000), SampleRate::SR_48K, front);
    let input: Vec<f64> = (0..2 * HRTF_STEP).map(|n| 0.01 * n as f64 - 0.7).collect();
    let (mut left, mut right) = (vec![0.0f64; input.len()], vec![0.0f64; input.len()]);
    p.process(
        Mix::WET,
        StereoPlanes::new(&input, &input).unwrap(),
        (MonoMut::new(&mut left), MonoMut::new(&mut right)),
        || front,
    );
    for n in 0..HRTF_STEP {
        let want = input[n];
        assert!((left[HRTF_LATENCY + n] - want).abs() < 1e-12, "left {n}");
        assert!(
            (right[HRTF_LATENCY + n] - 0.5 * want).abs() < 1e-12,
            "right {n}"
        );
    }
}
