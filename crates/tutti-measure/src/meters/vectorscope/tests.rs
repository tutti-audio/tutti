use super::*;
use crate::fold_uneven;

const ONE: NonZeroUsize = NonZeroUsize::MIN;

fn sine(len: usize) -> Vec<f64> {
    tutti_fixtures::signal::sine(
        tutti_units::Hz::new(440.0),
        tutti_units::Amplitude::new(1.0),
        tutti_units::SampleRate::SR_48K,
        tutti_units::Samples::new(len),
    )
}

fn scope(l: &[f64], r: &[f64], decimate: NonZeroUsize) -> Vec<ScopePoint> {
    vectorscope_points(
        &VectorscopeConfig::new(decimate),
        StereoPlanes::new(l, r).unwrap(),
    )
}

/// Mono lies on the mid axis, with the mono level times sqrt 2 as its
/// height.
#[test]
fn mono_lies_on_the_mid_axis() {
    let x = sine(480);
    for (p, &s) in scope(&x, &x, ONE).iter().zip(&x) {
        assert_eq!(p.side, 0.0);
        assert!((p.mid - s * core::f64::consts::SQRT_2).abs() < 1e-12);
    }
}

/// An inverted right channel lies on the side axis, and a signal in one
/// channel on a diagonal of slope +-1.
#[test]
fn inverted_pairs_lie_on_the_side_axis_and_one_channel_on_a_diagonal() {
    let x = sine(480);
    let inverted: Vec<f64> = x.iter().map(|v| -v).collect();
    for p in scope(&x, &inverted, ONE) {
        assert_eq!(p.mid, 0.0);
    }
    let silent = vec![0.0f64; x.len()];
    // Left only: side == mid. Right only: side == -mid.
    for p in scope(&x, &silent, ONE) {
        assert_eq!(p.side, p.mid);
    }
    for p in scope(&silent, &x, ONE) {
        assert_eq!(p.side, -p.mid);
    }
}

/// A panned sine sits at the angle its gains fix: `side / mid` is
/// `(gl - gr) / (gl + gr)` on every frame, and the radius is the frame's
/// level, which the rotation preserves.
#[test]
fn a_panned_sine_sits_at_its_gain_angle_with_its_level_as_radius() {
    let x = sine(480);
    let (gl, gr) = (0.8f64, 0.2f64);
    let left: Vec<f64> = x.iter().map(|v| v * gl).collect();
    let right: Vec<f64> = x.iter().map(|v| v * gr).collect();
    let expected = (gl - gr) / (gl + gr);
    for ((p, &l), &r) in scope(&left, &right, ONE).iter().zip(&left).zip(&right) {
        assert!((p.side * (gl + gr) - p.mid * (gl - gr)).abs() < 1e-12);
        if p.mid.abs() > 0.1 {
            assert!((p.side / p.mid - expected).abs() < 1e-9);
        }
        let radius = (p.side * p.side + p.mid * p.mid).sqrt();
        assert!((radius - (l * l + r * r).sqrt()).abs() < 1e-12);
    }
}

/// Decimation keeps frames `0, n, 2n, ...` and an empty pair gives no
/// points.
#[test]
fn decimation_keeps_every_nth_frame_from_the_first() {
    let l: Vec<f64> = (0..10).map(|i| i as f64).collect();
    let r = vec![0.0f64; 10];
    let kept = scope(&l, &r, NonZeroUsize::new(4).unwrap());
    let mids: Vec<f64> = kept.iter().map(|p| p.mid).collect();
    let want: Vec<f64> = [0.0f64, 4.0, 8.0]
        .iter()
        .map(|v| v * core::f64::consts::FRAC_1_SQRT_2)
        .collect();
    assert_eq!(mids, want);
    assert!(scope(&[], &[], ONE).is_empty());
}

/// Feeding blocks of any size, through one reused buffer, gives the batch
/// points: the stride phase carries over block edges.
#[test]
fn blocks_of_any_size_keep_one_stride_phase() {
    let l: Vec<f64> = (0..103).map(|i| i as f64).collect();
    let r: Vec<f64> = (0..103).map(|i| -(i as f64) * 0.5).collect();
    for decimate in [1usize, 3, 4, 7, 200] {
        let d = NonZeroUsize::new(decimate).unwrap();
        let batch = scope(&l, &r, d);
        for block in [1usize, 2, 5, 10, 64, 103] {
            let mut meter = VectorscopeConfig::new(d).measure(()).unwrap();
            let mut out = Vec::new();
            for (lb, rb) in l.chunks(block).zip(r.chunks(block)) {
                meter
                    .push(StereoPlanes::new(lb, rb).unwrap(), &mut out)
                    .unwrap();
            }
            assert_eq!(out, batch, "decimate {decimate}, block {block}");
        }
    }
}

/// The trait fold over uneven blocks (1, 7, 64 and the rest) yields
/// exactly the points of the batch function, as items.
#[test]
fn the_trait_fold_over_uneven_chunks_equals_vectorscope_points() {
    let l: Vec<f64> = (0..300).map(|i| i as f64).collect();
    let r: Vec<f64> = (0..300).map(|i| -(i as f64) * 0.25).collect();
    let cfg = VectorscopeConfig::new(NonZeroUsize::new(5).unwrap());
    let (items, ()) = fold_uneven(&cfg, (), l.len(), |meter, at, out| {
        meter.push(StereoPlanes::new(&l[at.clone()], &r[at]).unwrap(), out)
    })
    .unwrap();
    let batch = vectorscope_points(&cfg, StereoPlanes::new(&l, &r).unwrap());
    assert_eq!(batch.len(), 60);
    assert_eq!(items, batch);
}
