use tutti_buffers::{AmbisonicOrder, Planes, PlanesMut};
use tutti_units::{ArcDegrees, Azimuth, Direction, Elevation};

use super::*;
use crate::{COEFF_INTERVAL, SpeakerLayout};

const DIRECTIONS: [(f64, f64); 8] = [
    (0.0, 0.0),
    (90.0, 0.0),
    (37.0, 25.0),
    (-120.0, -40.0),
    (170.0, 70.0),
    (-15.0, -80.0),
    (45.0, 45.0),
    (-90.0, 10.0),
];

fn direction((az, el): (f64, f64)) -> Direction {
    Direction::new(Azimuth::new(az), Elevation::new(el))
}

fn unit_vector(d: Direction) -> [f64; 3] {
    let (az, el) = (d.azimuth.get().to_radians(), d.elevation.get().to_radians());
    [
        libm::cos(el) * libm::cos(az),
        libm::cos(el) * libm::sin(az),
        libm::sin(el),
    ]
}

fn direction_of(v: [f64; 3]) -> Direction {
    Direction::new(
        Azimuth::new(libm::atan2(v[1], v[0]).to_degrees()),
        Elevation::new(libm::asin(v[2].clamp(-1.0, 1.0)).to_degrees()),
    )
}

/// The AmbiX table of SN3D harmonics to third order, in ACN order, as polynomials in the
/// direction's `(x, y, z)`.
fn ambix_table([x, y, z]: [f64; 3]) -> [f64; 16] {
    let s3 = 3.0_f64.sqrt();
    let s15 = 15.0_f64.sqrt();
    let s58 = (5.0_f64 / 8.0).sqrt();
    let s38 = (3.0_f64 / 8.0).sqrt();
    [
        1.0,
        y,
        z,
        x,
        s3 * x * y,
        s3 * y * z,
        (3.0 * z * z - 1.0) / 2.0,
        s3 * x * z,
        s3 / 2.0 * (x * x - y * y),
        s58 * y * (3.0 * x * x - y * y),
        s15 * x * y * z,
        s38 * y * (5.0 * z * z - 1.0),
        z * (5.0 * z * z - 3.0) / 2.0,
        s38 * x * (5.0 * z * z - 1.0),
        s15 / 2.0 * z * (x * x - y * y),
        s58 * x * (x * x - 3.0 * y * y),
    ]
}

const THIRD: AmbisonicOrder = AmbisonicOrder::MAX;

fn sn3d(order: AmbisonicOrder, d: Direction) -> Harmonics {
    Harmonics::new(order, HarmonicNormalization::Sn3d, d)
}

/// The harmonics of a direction are the AmbiX table's polynomials in its unit vector, to
/// the `1e-12` of a few dozen double operations.
#[test]
fn the_harmonics_are_the_ambix_table_at_every_channel() {
    for &d in &DIRECTIONS {
        let d = direction(d);
        let expected = ambix_table(unit_vector(d));
        let got = sn3d(THIRD, d);
        for (acn, (g, e)) in got.iter().zip(expected).enumerate() {
            assert!((g - e).abs() < 1e-12, "acn {acn} at {d:?}: {g} vs {e}");
        }
    }
}

/// A source due left is `y = 1`, `z = x = 0`: the first-order channels read (1, 1, 0, 0).
#[test]
fn a_source_at_ninety_degrees_fills_the_y_channel_alone() {
    let h = sn3d(AmbisonicOrder::of(1), direction((90.0, 0.0)));
    for (g, e) in h.iter().zip([1.0, 1.0, 0.0, 0.0]) {
        assert!((g - e).abs() < 1e-15);
    }
}

/// An order's harmonics are the first `(order + 1)^2` of the next order's.
#[test]
fn a_lower_order_is_the_leading_channels_of_a_higher_one() {
    let d = direction((37.0, 25.0));
    let high = sn3d(THIRD, d);
    for n in 0..3 {
        let low = sn3d(AmbisonicOrder::of(n), d);
        assert!(low.iter().eq(high.iter().take(low.iter().count())));
    }
}

/// N3D is SN3D times `sqrt(2n + 1)` in every channel of degree `n`.
#[test]
fn n3d_is_sn3d_times_the_root_of_two_n_plus_one() {
    let d = direction((-120.0, -40.0));
    let a = sn3d(THIRD, d);
    let b = Harmonics::new(THIRD, HarmonicNormalization::N3d, d);
    for (acn, (n3d, sn3d)) in b.iter().zip(a.iter()).enumerate() {
        let degree = (acn as f64).sqrt().floor();
        assert!((n3d - sn3d * (2.0 * degree + 1.0).sqrt()).abs() < 1e-12);
    }
}

fn rotations() -> Vec<(Direction, ArcDegrees)> {
    [
        (30.0, 0.0, 0.0),
        (0.0, 35.0, 0.0),
        (0.0, 0.0, 70.0),
        (-100.0, -40.0, -25.0),
        (170.0, 80.0, 130.0),
    ]
    .map(|(az, el, roll)| (direction((az, el)), ArcDegrees::new(roll)))
    .to_vec()
}

/// `Rz(yaw) Ry(pitch) Rx(roll)` applied to a vector, from the three elementary rotations
/// (a rotation by `pitch` about `y` takes the front down).
fn rotate_vector(yaw: f64, pitch: f64, roll: f64, v: [f64; 3]) -> [f64; 3] {
    let (yaw, pitch, roll) = (yaw.to_radians(), pitch.to_radians(), roll.to_radians());
    let [x, y, z] = v;
    let (y, z) = (
        y * libm::cos(roll) - z * libm::sin(roll),
        y * libm::sin(roll) + z * libm::cos(roll),
    );
    let (x, z) = (
        x * libm::cos(pitch) + z * libm::sin(pitch),
        -x * libm::sin(pitch) + z * libm::cos(pitch),
    );
    [
        x * libm::cos(yaw) - y * libm::sin(yaw),
        x * libm::sin(yaw) + y * libm::cos(yaw),
        z,
    ]
}

fn apply(rotation: &Rotation, h: &Harmonics) -> Vec<f64> {
    let x: Vec<f64> = h.iter().collect();
    (0..x.len())
        .map(|i| {
            x.iter()
                .enumerate()
                .map(|(j, x)| rotation.get(i, j) * x)
                .sum()
        })
        .collect()
}

/// For every order, rotation and direction the rotated channels are the harmonics of the
/// rotated direction: `Harmonics(R d) = M Harmonics(d)`, the definition of the matrix and
/// what the Ivanic-Ruedenberg recursion has to deliver at degrees 2 and 3.
#[test]
fn the_rotation_matrix_maps_the_harmonics_of_a_direction_to_those_of_the_rotated_one() {
    for order in 1..=3 {
        let order = AmbisonicOrder::of(order);
        for (front, roll) in rotations() {
            let m = Rotation::new(order, front, roll);
            for &d in &DIRECTIONS {
                let d = direction(d);
                let moved = rotate_vector(
                    front.azimuth.get(),
                    -front.elevation.get(),
                    roll.get(),
                    unit_vector(d),
                );
                let expected = sn3d(order, direction_of(moved));
                let got = apply(&m, &sn3d(order, d));
                for (acn, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
                    assert!(
                        (g - e).abs() < 1e-12,
                        "order {order:?} front {front:?} roll {roll:?} acn {acn}: {g} vs {e}"
                    );
                }
            }
        }
    }
}

/// Turning the front along the horizon by `t` takes a source at bearing `phi` to `phi + t`: rotating the encoding at
/// `phi` is the encoding at `phi + t`, per channel, at any height.
#[test]
fn yaw_by_theta_maps_an_encoding_at_phi_to_the_encoding_at_phi_plus_theta() {
    for (phi, el, theta) in [
        (10.0, 0.0, 35.0),
        (-170.0, 30.0, -50.0),
        (120.0, -60.0, 100.0),
    ] {
        let m = Rotation::new(THIRD, direction((theta, 0.0)), ArcDegrees::new(0.0));
        let got = apply(&m, &sn3d(THIRD, direction((phi, el))));
        let expected = sn3d(THIRD, direction((phi + theta, el)));
        for (g, e) in got.iter().zip(expected.iter()) {
            assert!((g - e).abs() < 1e-12);
        }
    }
}

/// The matrix is orthogonal: `M M^T = I`, so a rotation preserves the energy of the
/// channels, and the inverse composed with it is the identity.
#[test]
fn a_rotation_composed_with_its_inverse_is_the_identity_and_preserves_energy() {
    for (front, roll) in rotations() {
        let m = Rotation::new(THIRD, front, roll);
        let inverse = m.inverse();
        for i in 0..16 {
            for j in 0..16 {
                let product: f64 = (0..16).map(|k| m.get(i, k) * inverse.get(k, j)).sum();
                let identity = if i == j { 1.0 } else { 0.0 };
                assert!((product - identity).abs() < 1e-12, "({i}, {j}): {product}");
            }
        }
        let x = sn3d(THIRD, direction((37.0, 25.0)));
        let y = apply(&m, &x);
        let energy = |v: &[f64]| v.iter().map(|v| v * v).sum::<f64>();
        assert!((energy(&y) - energy(&x.iter().collect::<Vec<_>>())).abs() < 1e-12);
    }
}

/// A rotation of order one is the identity on the degree-0 channel and does not mix
/// degrees: the matrix is block diagonal by degree.
#[test]
fn a_rotation_leaves_degree_zero_alone_and_never_mixes_degrees() {
    let m = Rotation::new(THIRD, direction((-100.0, 40.0)), ArcDegrees::new(-25.0));
    assert_eq!(m.get(0, 0), 1.0);
    for i in 0..16 {
        for j in 0..16 {
            let degree = |c: usize| (c as f64).sqrt().floor() as usize;
            if degree(i) != degree(j) {
                assert_eq!(m.get(i, j), 0.0);
            }
        }
    }
}

fn planes_of(channels: &[Vec<f64>]) -> Vec<&[f64]> {
    channels.iter().map(Vec::as_slice).collect()
}

fn render_rotator(rotator: &mut AmbisonicRotator, input: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let frames = input[0].len();
    let mut out = vec![vec![0.0; frames]; input.len()];
    let mut refs: Vec<&mut [f64]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    rotator.process(
        Planes::new(&planes_of(input)).unwrap(),
        PlanesMut::new(&mut refs).unwrap(),
    );
    out
}

/// A held rotator multiplies every frame by its matrix.
#[test]
fn a_held_rotator_applies_its_matrix_to_every_frame() {
    let order = AmbisonicOrder::of(2);
    let m = Rotation::new(order, direction((50.0, 20.0)), ArcDegrees::new(10.0));
    let mut rotator = AmbisonicRotator::new(m);
    let frames = 40;
    let input: Vec<Vec<f64>> = (0..9)
        .map(|c| {
            (0..frames)
                .map(|f| ((c + 1) * (f + 1)) as f64 * 0.01)
                .collect()
        })
        .collect();
    let out = render_rotator(&mut rotator, &input);
    for f in 0..frames {
        for (i, channel) in out.iter().enumerate() {
            let expected: f64 = (0..9).map(|j| m.get(i, j) * input[j][f]).sum();
            assert!((channel[f] - expected).abs() < 1e-12);
        }
    }
}

/// A glide to a new rotation reads, at step `k` of `n`, the blend `(1 - k/n) M0 + (k/n) M1`
/// and lands on `M1` from step `n` on: the first frame after the retarget is step 1.
#[test]
fn a_rotator_glides_linearly_between_two_matrices_and_lands_on_the_target() {
    let order = AmbisonicOrder::of(1);
    let m0 = Rotation::new(order, direction((0.0, 0.0)), ArcDegrees::new(0.0));
    let m1 = Rotation::new(order, direction((80.0, 30.0)), ArcDegrees::new(0.0));
    let mut rotator = AmbisonicRotator::new(m0);
    rotator.retarget(m1);
    let frames = 2 * COEFF_INTERVAL;
    let input: Vec<Vec<f64>> = (0..4).map(|c| vec![(c + 1) as f64; frames]).collect();
    let out = render_rotator(&mut rotator, &input);
    let n = COEFF_INTERVAL as f64;
    for f in 0..frames {
        let t = ((f + 1) as f64 / n).min(1.0);
        for (i, channel) in out.iter().enumerate() {
            let expected: f64 = (0..4)
                .map(|j| ((1.0 - t) * m0.get(i, j) + t * m1.get(i, j)) * input[j][f])
                .sum();
            assert!(
                (channel[f] - expected).abs() < 1e-12,
                "frame {f} channel {i}"
            );
        }
    }
}

/// Cutting a render into calls renders the same bits.
#[test]
fn a_rotator_renders_the_same_bits_however_the_stretch_is_cut() {
    let order = AmbisonicOrder::of(3);
    let m0 = Rotation::new(order, direction((0.0, 0.0)), ArcDegrees::new(0.0));
    let m1 = Rotation::new(order, direction((80.0, 30.0)), ArcDegrees::new(40.0));
    let frames = 24;
    let input: Vec<Vec<f64>> = (0..16)
        .map(|c| {
            (0..frames)
                .map(|f| ((c * 7 + f * 3) % 11) as f64 * 0.1)
                .collect()
        })
        .collect();
    let mut whole = AmbisonicRotator::new(m0);
    whole.retarget(m1);
    let one = render_rotator(&mut whole, &input);
    let mut cut = AmbisonicRotator::new(m0);
    cut.retarget(m1);
    let (a, b): (Vec<Vec<f64>>, Vec<Vec<f64>>) = input
        .iter()
        .map(|c| (c[..5].to_vec(), c[5..].to_vec()))
        .unzip();
    let (first, second) = (render_rotator(&mut cut, &a), render_rotator(&mut cut, &b));
    for c in 0..16 {
        let joined: Vec<f64> = first[c].iter().chain(&second[c]).copied().collect();
        assert_eq!(joined, one[c]);
    }
}

fn render_encoder(encoder: &mut AmbisonicEncoder, input: &[f64], channels: usize) -> Vec<Vec<f64>> {
    let mut out = vec![vec![0.0; input.len()]; channels];
    let mut refs: Vec<&mut [f64]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    encoder.process(
        tutti_buffers::Mono::new(input),
        PlanesMut::new(&mut refs).unwrap(),
    );
    out
}

/// A held encoder plays each frame times the harmonics of its direction.
#[test]
fn a_held_encoder_scales_the_source_by_each_harmonic() {
    let d = direction((37.0, 25.0));
    let mut encoder = AmbisonicEncoder::new(THIRD, HarmonicNormalization::Sn3d, d);
    let input: Vec<f64> = (0..20).map(|f| f as f64 * 0.05 - 0.4).collect();
    let out = render_encoder(&mut encoder, &input, 16);
    let table = ambix_table(unit_vector(d));
    for (c, channel) in out.iter().enumerate() {
        for (o, x) in channel.iter().zip(&input) {
            assert!((o - x * table[c]).abs() < 1e-12);
        }
    }
}

/// A move of the source glides each channel's gain linearly over the interval, landing on
/// the new direction's harmonics.
#[test]
fn a_moved_encoder_glides_each_gain_to_the_new_directions_harmonics() {
    let (from, to) = (direction((0.0, 0.0)), direction((60.0, 20.0)));
    let mut encoder = AmbisonicEncoder::new(THIRD, HarmonicNormalization::Sn3d, from);
    encoder.retarget(to);
    let frames = 2 * COEFF_INTERVAL;
    let out = render_encoder(&mut encoder, &vec![1.0; frames], 16);
    let (g0, g1) = (ambix_table(unit_vector(from)), ambix_table(unit_vector(to)));
    for (c, channel) in out.iter().enumerate() {
        for (f, o) in channel.iter().enumerate() {
            let t = ((f + 1) as f64 / COEFF_INTERVAL as f64).min(1.0);
            assert!((o - ((1.0 - t) * g0[c] + t * g1[c])).abs() < 1e-12);
        }
    }
}

const fn cube_vertex(i: usize) -> (f64, f64) {
    // The eight vertices of a cube: bearings 45, 135, 225, 315 at +-asin(1/sqrt 3).
    (
        45.0 + 90.0 * (i % 4) as f64,
        if i < 4 {
            35.264_389_682_754_654
        } else {
            -35.264_389_682_754_654
        },
    )
}

/// The vertices of an icosahedron: the two poles and two rings of five at
/// `+-atan(1/2)`, the second rotated half a step.
fn icosahedron() -> Vec<Direction> {
    let ring = libm::atan(0.5).to_degrees();
    let mut v = vec![direction((0.0, 90.0)), direction((0.0, -90.0))];
    for k in 0..5 {
        v.push(direction((72.0 * k as f64, ring)));
        v.push(direction((72.0 * k as f64 + 36.0, -ring)));
    }
    v
}

fn decode_plane_wave(
    decoder: &AmbisonicDecoder,
    normalization: HarmonicNormalization,
    d: Direction,
) -> Vec<f64> {
    let h = Harmonics::new(decoder.order(), normalization, d);
    (0..decoder.speakers())
        .map(|s| {
            h.iter()
                .enumerate()
                .map(|(c, a)| decoder.gain(s, c) * a)
                .sum()
        })
        .collect()
}

/// A cube's eight vertices are a spherical 3-design, so at order 1 the sampling decoder's
/// gains for a plane wave sum to one and their squares to `(1 + 1)^2 / 8` in any direction
/// (the harmonics' orthogonality over the design), in SN3D and in N3D alike.
#[test]
fn the_sampling_decoder_on_a_cube_keeps_the_sum_one_and_the_energy_four_eighths() {
    let cube: Vec<Direction> = (0..8).map(|i| direction(cube_vertex(i))).collect();
    let cube = SpeakerLayout::try_from(cube.as_slice()).unwrap();
    for normalization in [HarmonicNormalization::Sn3d, HarmonicNormalization::N3d] {
        let decoder = AmbisonicDecoder::sampling(AmbisonicOrder::of(1), normalization, &cube);
        for &d in &DIRECTIONS {
            let g = decode_plane_wave(&decoder, normalization, direction(d));
            let sum: f64 = g.iter().sum();
            let energy: f64 = g.iter().map(|g| g * g).sum();
            assert!((sum - 1.0).abs() < 1e-12, "sum {sum}");
            assert!((energy - 4.0 / 8.0).abs() < 1e-12, "energy {energy}");
        }
    }
}

/// An icosahedron is a 5-design: at order 2 the same two identities hold with
/// `(2 + 1)^2 / 12`.
#[test]
fn the_sampling_decoder_on_an_icosahedron_keeps_the_sum_one_and_the_energy_nine_twelfths() {
    let layout = SpeakerLayout::try_from(icosahedron().as_slice()).unwrap();
    let decoder =
        AmbisonicDecoder::sampling(AmbisonicOrder::of(2), HarmonicNormalization::Sn3d, &layout);
    for &d in &DIRECTIONS {
        let g = decode_plane_wave(&decoder, HarmonicNormalization::Sn3d, direction(d));
        let sum: f64 = g.iter().sum();
        let energy: f64 = g.iter().map(|g| g * g).sum();
        assert!((sum - 1.0).abs() < 1e-12);
        assert!((energy - 9.0 / 12.0).abs() < 1e-12);
    }
}

fn ring(count: usize) -> Vec<Direction> {
    (0..count)
        .map(|k| direction((360.0 / count as f64 * k as f64, 0.0)))
        .collect()
}

fn allrad_ring(count: usize, order: AmbisonicOrder) -> AmbisonicDecoder {
    let layout = SpeakerLayout::try_from(ring(count).as_slice()).unwrap();
    AmbisonicDecoder::allrad(order, HarmonicNormalization::Sn3d, &layout)
}

/// A ring of eight speakers maps onto itself under 45 degrees, and so do the virtual
/// speakers (sixteen bearings): a source turned by 45 degrees decodes to the same gains
/// moved by one speaker, to rounding.
#[test]
fn allrad_on_a_ring_moves_by_one_speaker_when_the_source_turns_by_one_spacing() {
    let decoder = allrad_ring(8, THIRD);
    for az in [3.0, 17.0, 100.0, -80.0] {
        let a = decode_plane_wave(&decoder, HarmonicNormalization::Sn3d, direction((az, 0.0)));
        let b = decode_plane_wave(
            &decoder,
            HarmonicNormalization::Sn3d,
            direction((az + 45.0, 0.0)),
        );
        for s in 0..8 {
            assert!(
                (b[(s + 1) % 8] - a[s]).abs() < 1e-12,
                "bearing {az} speaker {s}"
            );
        }
    }
}

/// The energy vector of the feeds, the speaker directions weighted by their squared gains.
fn energy_vector(layout: &[Direction], gains: &[f64]) -> [f64; 3] {
    let mut r = [0.0; 3];
    for (d, g) in layout.iter().zip(gains) {
        let u = unit_vector(*d);
        for k in 0..3 {
            r[k] += g * g * u[k];
        }
    }
    r
}

/// The feed of speaker `s` is a trigonometric polynomial of degree at most the order in
/// the source bearing (the decoded field is band limited and the panning only weighs it),
/// so the squares of the feeds have degree at most `2 order = 6`. On a ring of eight the
/// first harmonic of the squares aliases from degree `8 - 1 = 7` and above only, so it is
/// the source's alone: the energy vector points at the source at every bearing, to
/// rounding.
#[test]
fn allrad_on_a_ring_has_its_energy_vector_exactly_on_the_source_at_every_bearing() {
    let decoder = allrad_ring(8, THIRD);
    let layout = ring(8);
    for az in [
        0.0, 3.0, 11.0, 22.5, 29.9, 67.5, 100.0, -112.5, 179.0, -45.5,
    ] {
        let g = decode_plane_wave(&decoder, HarmonicNormalization::Sn3d, direction((az, 0.0)));
        let r = energy_vector(&layout, &g);
        let found = libm::atan2(r[1], r[0]).to_degrees();
        let off = Azimuth::new(az).shortest_arc_to(Azimuth::new(found)).get();
        assert!(off.abs() < 1e-9, "bearing {az}: energy vector at {found}");
    }
}

/// A source at a speaker takes most of the energy there.
#[test]
fn allrad_on_a_ring_puts_a_source_at_a_speaker_mostly_on_that_speaker() {
    let decoder = allrad_ring(8, THIRD);
    let g = decode_plane_wave(&decoder, HarmonicNormalization::Sn3d, direction((0.0, 0.0)));
    let total: f64 = g.iter().map(|g| g * g).sum();
    assert!(g[0] * g[0] > 0.5 * total);
}

/// A decoder renders each speaker as the matrix applied to the channels of every frame.
#[test]
fn a_decoder_renders_a_speaker_as_the_matrix_over_the_channels() {
    let decoder = allrad_ring(8, AmbisonicOrder::of(2));
    let frames = 12;
    let input: Vec<Vec<f64>> = (0..9)
        .map(|c| {
            (0..frames)
                .map(|f| ((c * 5 + f) % 7) as f64 - 3.0)
                .collect()
        })
        .collect();
    for speaker in [0, 3, 7] {
        let mut out = vec![0.0; frames];
        decoder.process(
            speaker,
            Planes::new(&planes_of(&input)).unwrap(),
            tutti_buffers::MonoMut::new(&mut out),
        );
        for f in 0..frames {
            let expected: f64 = (0..9).map(|c| decoder.gain(speaker, c) * input[c][f]).sum();
            assert!((out[f] - expected).abs() < 1e-12);
        }
    }
}
