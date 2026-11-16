use super::*;
use tutti_types::{Azimuth, Elevation};

/// The resampler reads a response's first and last samples. One vertex
/// whose left response is a unit delta on its first sample and right a
/// unit delta on its last, neither decayed, is doubled from 8 to 16 kHz.
/// Each response is resampled alone, so an output tap at `t` is
/// `step · kernel(t − m)` for its delta at `m`, and the tap that lands on
/// the delta (`n = 2m`) takes the kernel's centre: sinc and window are 1
/// there, so it is `step · cutoff = 0.5 · 0.95 = 0.475`, the delta's DC
/// sum of 1 at the kernel's peak. No libm call is involved but `cos(0)`,
/// which is exact; the tolerance covers only the `f32` rounding.
#[test]
fn resampling_reads_a_responses_edge_taps() {
    const LEN: usize = 8;
    let mut responses = vec![0.0f32; 2 * LEN];
    responses[0] = 1.0;
    responses[2 * LEN - 1] = 1.0;
    let out_len = 2 * LEN;
    let mut got = Vec::new();
    resample_into(&responses, LEN, 8_000, 16_000, out_len, |r, response| {
        assert_eq!(r, got.len(), "emitted in index order");
        got.push(response.to_vec());
    });
    assert_eq!(got.len(), 2);

    let first = got[0][0];
    let last = got[1][2 * (LEN - 1)];
    for (ear, tap) in [("left, first sample", first), ("right, last sample", last)] {
        assert!(
            (tap - 0.475).abs() <= 1e-6,
            "{ear}: the tap on the delta is {tap}, expected 0.475"
        );
    }
}

/// Resampling in small batches gives every response the same samples as
/// one batch, and the right index. Fifteen responses of 8 taps go from 8 to
/// 16 kHz in batches of 3, so four boundaries fall, some between a
/// vertex's two ears. Response `r` is a delta of
/// amplitude `r + 1` on tap `r % 8`, so its output tap `2 (r % 8)` is
/// `0.475 (r + 1)` (see `resampling_reads_a_responses_edge_taps`), which
/// pins each emitted index to its own response, and the whole output is
/// bit-identical to the single-batch run.
#[test]
fn resampling_in_batches_matches_one_batch() {
    const LEN: usize = 8;
    const OUT_LEN: usize = 16;
    const RESPONSES: usize = 15;
    let mut responses = vec![0.0f32; RESPONSES * LEN];
    for r in 0..RESPONSES {
        responses[r * LEN + r % LEN] = (r + 1) as f32;
    }
    let run = |batch_samples: usize| {
        let mut got = Vec::new();
        resample_batched(
            &responses,
            LEN,
            8_000,
            16_000,
            OUT_LEN,
            batch_samples,
            |r, response| {
                assert_eq!(r, got.len(), "emitted in index order");
                got.push(response.to_vec());
            },
        );
        got
    };
    let whole = run(RESAMPLE_BATCH);
    let small = run(3 * OUT_LEN);
    assert_eq!(whole.len(), RESPONSES);
    assert_eq!(small, whole, "batching changes no sample");
    for (r, response) in small.iter().enumerate() {
        let tap = response[2 * (r % LEN)];
        let want = 0.475 * (r + 1) as f32;
        assert!(
            (tap - want).abs() <= 1e-5 * want,
            "response {r}: tap {tap}, expected {want}"
        );
    }
}

/// Resample one response of `len` taps from `from` to `to` Hz into `out_len`.
fn resample_one(response: &[f32], from: u32, to: u32, out_len: usize) -> Vec<f32> {
    let mut got = Vec::new();
    resample_into(response, response.len(), from, to, out_len, |_, r| {
        got = r.to_vec();
    });
    got
}

/// A constant response keeps its level through the kernel's unity DC gain,
/// then takes the `from / to` weight: interior taps (the kernel fully inside
/// the response) of a response of 1.0 are `from / to`, 0.5 doubling the rate
/// and 2.0 halving it. Within 1e-3: the kernel's truncation to 32 crossings.
#[test]
fn a_constant_response_keeps_its_level_scaled_by_the_rate_ratio() {
    for (from, to, len, out_len, interior, want) in [
        (8_000, 16_000, 200, 400, 100..300, 0.5f32),
        (16_000, 8_000, 400, 200, 60..140, 2.0),
    ] {
        let got = resample_one(&vec![1.0; len], from, to, out_len);
        for n in interior {
            assert!(
                (got[n] - want).abs() < 1e-3,
                "{from}->{to}, tap {n}: {} vs {want}",
                got[n]
            );
        }
    }
}

/// An impulse on tap `m` comes out as the kernel itself, sampled at the
/// output taps: `step * cutoff * sinc(pi * cutoff * (n * step - m)) *
/// blackman`, written out here from the formulas (cutoff `0.95` of Nyquist,
/// 32 crossings, so the half-width is `32 / 0.95`), 8 to 16 kHz. The
/// Blackman is `0.42 + 0.5 cos u + 0.08 cos 2u`, `u = pi t / half`.
#[test]
fn an_impulse_resamples_to_the_analytic_kernel() {
    const LEN: usize = 200;
    const M: usize = 100;
    let mut response = vec![0.0f32; LEN];
    response[M] = 1.0;
    let got = resample_one(&response, 8_000, 16_000, 400);
    let (step, cutoff) = (0.5f64, 0.95f64);
    let half = 32.0 / cutoff;
    for (n, &g) in got.iter().enumerate() {
        let t = n as f64 * step - M as f64;
        let x = std::f64::consts::PI * cutoff * t;
        let sinc = if x == 0.0 { 1.0 } else { x.sin() / x };
        let u = std::f64::consts::PI * t / half;
        let blackman = if t.abs() > half {
            0.0
        } else {
            0.42 + 0.5 * u.cos() + 0.08 * (2.0 * u).cos()
        };
        let want = step * cutoff * sinc * blackman;
        assert!((f64::from(g) - want).abs() < 1e-6, "tap {n}: {g} vs {want}");
    }
}

const LEN: usize = 4;

/// The six directions of the octahedron: front, left, back, right, up, down.
fn octahedron() -> Vec<Direction> {
    let level = |az: f32| Direction::new(Azimuth(az), Elevation::LEVEL);
    vec![
        level(0.0),
        level(90.0),
        level(180.0),
        level(-90.0),
        Direction::new(Azimuth::FRONT, Elevation::UP),
        Direction::new(Azimuth::FRONT, Elevation::DOWN),
    ]
}

/// Measurement `i`'s left response is `[i + 1; LEN]` and its right
/// `[-(i + 1); LEN]`.
fn responses(count: usize) -> Vec<(Vec<f32>, Vec<f32>)> {
    (0..count)
        .map(|i| (vec![(i + 1) as f32; LEN], vec![-((i + 1) as f32); LEN]))
        .collect()
}

fn build(
    directions: &[Direction],
    data: &[(Vec<f32>, Vec<f32>)],
) -> Result<HrirSphere, HrtfBinauralError> {
    let measurements: Vec<_> = directions
        .iter()
        .zip(data)
        .map(|(&d, (l, r))| HrirMeasurement::new(d, Mono::new(l), Mono::new(r)))
        .collect();
    HrirSphere::new(SampleRate::SR_48K, &measurements)
}

/// The octahedron has eight faces, each wound counter-clockwise seen from
/// outside: the determinant of a face's vertices (a triple product) is
/// positive when the origin is on its inner side. The responses sit with
/// their vertex.
#[test]
fn an_octahedron_of_measurements_has_eight_outward_faces_and_keeps_its_responses() {
    let sphere = build(&octahedron(), &responses(6)).expect("an octahedron encloses the listener");
    assert_eq!(sphere.faces.len(), 8);
    for face in &sphere.faces {
        let [a, b, c] = face.map(|v| sphere.positions[v as usize]);
        let det = a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
            + a[2] * (b[0] * c[1] - b[1] * c[0]);
        assert!(det > 0.5, "face {face:?} winds inward (det {det})");
    }
    for v in 0..6 {
        let at = 2 * v * LEN;
        assert_eq!(sphere.responses[at..at + LEN], [(v + 1) as f32; LEN]);
        assert_eq!(
            sphere.responses[at + LEN..at + 2 * LEN],
            [-((v + 1) as f32); LEN]
        );
    }
}

#[test]
fn bytes_round_trip_to_an_equal_sphere() {
    let sphere = build(&octahedron(), &responses(6)).expect("built");
    let bytes = sphere.to_bytes();
    assert_eq!(bytes[..4], *b"HRIR");
    let parsed = HrirSphere::from_bytes(&bytes).expect("a written sphere parses");
    assert!(
        parsed == sphere,
        "the parsed sphere differs from the one written"
    );
    assert_eq!(parsed.to_bytes(), bytes);
}

/// Prepared at its own rate, a ray along each vertex's direction blends that
/// vertex alone: the faces the hull made cover the whole sphere.
#[test]
fn a_built_sphere_covers_every_direction_it_was_measured_in() {
    let directions = octahedron();
    let sphere = build(&directions, &responses(6)).expect("built");
    let set = HrtfSet::build(&sphere, 48_000, 64);
    let mut hint = 0;
    for (v, &d) in directions.iter().enumerate() {
        let w = set.weights(unit_vector(d), &mut hint);
        let at = w
            .verts
            .iter()
            .position(|&x| x as usize == v)
            .expect("the vertex is in its own face");
        assert!(
            (w.w[at] - 1.0).abs() < 1e-5,
            "vertex {v}: weights {:?}",
            w.w
        );
    }
}

#[test]
fn a_measurement_set_that_cannot_enclose_the_listener_is_refused() {
    let few = &octahedron()[..3];
    assert!(matches!(
        build(few, &responses(3)),
        Err(HrtfBinauralError::NotEnclosing)
    ));
    let level = |az: f32| Direction::new(Azimuth(az), Elevation::LEVEL);
    let ring = [
        level(0.0),
        level(90.0),
        level(180.0),
        level(-90.0),
        level(45.0),
    ];
    assert!(matches!(
        build(&ring, &responses(5)),
        Err(HrtfBinauralError::NotEnclosing)
    ));
}

#[test]
fn two_measurements_in_one_direction_are_refused() {
    let mut directions = octahedron();
    directions.push(directions[1]);
    assert!(matches!(
        build(&directions, &responses(7)),
        Err(HrtfBinauralError::Duplicate {
            first: 1,
            second: 6
        })
    ));
}

#[test]
fn responses_of_unequal_length_are_refused() {
    let mut data = responses(6);
    data[4].1.pop();
    assert!(matches!(
        build(&octahedron(), &data),
        Err(HrtfBinauralError::ResponseLength { vertex: 4, expected: LEN, found }) if found == LEN - 1
    ));
}

#[test]
fn a_response_past_fifty_milliseconds_is_refused() {
    let long = vec![0.0f32; 48_000 / 20 + 1];
    let data: Vec<_> = (0..6).map(|_| (long.clone(), long.clone())).collect();
    assert!(matches!(
        build(&octahedron(), &data),
        Err(HrtfBinauralError::ResponseTooLong { max: 2400, .. })
    ));
}

#[test]
fn a_sample_out_of_range_is_refused_with_its_vertex() {
    let mut data = responses(6);
    data[2].0[1] = 2e6;
    assert!(matches!(
        build(&octahedron(), &data),
        Err(HrtfBinauralError::Vertex { vertex: 2 })
    ));
}

/// The byte format stores the rate in whole hertz: 44 100.5 Hz is refused by name, and
/// 44 100 Hz builds.
#[test]
fn a_rate_that_is_not_a_whole_number_of_hertz_is_refused() {
    let data = responses(6);
    let measurements: Vec<_> = octahedron()
        .into_iter()
        .zip(&data)
        .map(|(d, (l, r))| HrirMeasurement::new(d, Mono::new(l), Mono::new(r)))
        .collect();
    assert!(matches!(
        HrirSphere::new(SampleRate::of(44_100.5), &measurements),
        Err(HrtfBinauralError::FractionalRate { .. })
    ));
    assert!(HrirSphere::new(SampleRate::of(44_100.0), &measurements).is_ok());
}

/// A direction the hull of the others encloses (here a point at half radius, as rounding can
/// leave one just inside) is refused by its index, and the octahedron alone passes.
#[test]
fn a_direction_the_hull_drops_is_refused_by_index() {
    let mut points = vec![
        [1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, -1.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, -1.0],
    ];
    let faces = convex_hull(&points).expect("an octahedron");
    assert!(every_vertex_on(&faces, points.len()).is_ok());
    points.insert(2, [0.5, 0.0, 0.0]);
    let faces = convex_hull(&points).expect("an octahedron");
    assert!(matches!(
        every_vertex_on(&faces, points.len()),
        Err(HrtfBinauralError::Inside { vertex: 2 })
    ));
}
