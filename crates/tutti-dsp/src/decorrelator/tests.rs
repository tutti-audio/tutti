use super::*;
use tutti_buffers::Float;

const RATE: SampleRate = SampleRate::SR_48K;
/// 2048 impulses a second over 62.5 ms: 128 impulses, a period of 23.4375 frames (exact in
/// binary) and 3000 frames.
const DENSITY: f64 = 2_048.0;
const LENGTH: f64 = 0.0625;
const FRAMES: f64 = 3_000.0;
const COUNT: usize = 128;
const PERIOD: f64 = 48_000.0 / DENSITY;

fn bank(width: u16, seed: u64) -> Decorrelator<f64> {
    Decorrelator::new(
        ChannelLayout::from_count(width),
        VelvetDensity::new(Hz::new(DENSITY)).unwrap(),
        VelvetLength::new(Seconds::new(LENGTH)).unwrap(),
        RATE,
        seed,
    )
    .unwrap()
}

/// Channel `c`'s dense impulse response from the closed form: tap `k` is `s_k e(m_k) / sqrt(sum
/// e^2)` at `m_k`, `e(m) = 10^(-3 m / frames)` (60 dB over the length), from the sequence alone.
fn response(d: &Decorrelator<f64>, c: usize) -> Vec<f64> {
    let pulses = d.sequence(c).unwrap().pulses();
    let e = |m: usize| 10f64.powf(-3.0 * m as f64 / FRAMES);
    let norm = pulses
        .iter()
        .map(|p| e(p.position.get()).powi(2))
        .sum::<f64>()
        .sqrt();
    let mut h = vec![0.0; FRAMES as usize];
    for p in pulses {
        h[p.position.get()] = p.sign.get() * e(p.position.get()) / norm;
    }
    h
}

/// `input` through `d` in one block, with every channel as given.
fn run(d: &mut Decorrelator<f64>, input: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let mut out = vec![vec![0.0; input[0].len()]; input.len()];
    d.process(
        Planes::from_vecs(input).unwrap(),
        PlanesMut::from_vecs(&mut out).unwrap(),
    );
    out
}

/// A seeded white input, uniform in `[-1, 1)`: a variance of 1/3.
fn white(frames: usize, seed: u64) -> Vec<f64> {
    let mut rng = SplitMix64::new(seed);
    (0..frames)
        .map(|_| 2.0 * f64::from_fraction_bits(rng.next_u64()) - 1.0)
        .collect()
}

/// `sum_n a[n] b[n + l]`.
fn cross(a: &[f64], b: &[f64], l: isize) -> f64 {
    (0..a.len() as isize)
        .filter(|n| (0..b.len() as isize).contains(&(n + l)))
        .map(|n| a[n as usize] * b[(n + l) as usize])
        .sum()
}

/// The variance, in units of `sigma^4 / N`, of the sample covariance of `a * x` and `b * x`
/// for a white `x` (Isserlis): `sum_l [Raa(l) Rbb(l) + Rab(l) Rab(-l)]`. A uniform input's
/// negative excess kurtosis only lowers it, so it bounds the variance from above.
fn covariance_variance(a: &[f64], b: &[f64]) -> f64 {
    let reach = a.len() as isize;
    (-reach..=reach)
        .map(|l| cross(a, a, l) * cross(b, b, l) + cross(a, b, l) * cross(a, b, -l))
        .sum()
}

/// An impulse into channel `c` alone comes out of channel `c` as the enveloped sequence: the
/// impulse response is exactly the taps `s_k e(m_k) / sqrt(sum e^2)` at the sequence's positions
/// and zero elsewhere, and the other channel stays silent.
#[test]
fn the_impulse_response_is_the_enveloped_normalized_sequence() {
    let mut d = bank(2, 3);
    for c in 0..2 {
        d.reset();
        let mut x = vec![vec![0.0; 3_100]; 2];
        x[c][0] = 1.0;
        let y = run(&mut d, &x);
        let h = response(&d, c);
        for (n, &v) in y[c].iter().enumerate() {
            let want = h.get(n).copied().unwrap_or(0.0);
            assert!(
                (v - want).abs() <= 1e-14,
                "channel {c}, frame {n}: {v} against {want}"
            );
        }
        assert!(y[1 - c].iter().all(|v| *v == 0.0));
    }
}

/// A sequence of `length` seconds at `density` holds `floor(length * density)` impulses, one in
/// each period `Td`: the `k`-th sits within `[round(k Td), round((k + 1) Td) - 1]`, so the
/// positions are distinct and rising, and each sign is `+1` or `-1`; the same holds of the
/// nonzero taps of the rendered response.
#[test]
fn a_sequence_has_one_signed_impulse_in_each_period() {
    let d = bank(2, 11);
    for c in 0..2 {
        let pulses = d.sequence(c).unwrap().pulses();
        assert_eq!(pulses.len(), COUNT);
        for (k, p) in pulses.iter().enumerate() {
            let lo = libm::round(k as f64 * PERIOD) as usize;
            let hi = libm::round((k + 1) as f64 * PERIOD) as usize - 1;
            assert!((lo..=hi).contains(&p.position.get()), "pulse {k}: {p:?}");
            assert!(p.sign == Gain::UNITY || p.sign == Gain::INVERTING);
        }
        assert!(pulses.windows(2).all(|w| w[0].position < w[1].position));
        let nonzero = response(&d, c).iter().filter(|v| **v != 0.0).count();
        assert_eq!(nonzero, COUNT);
    }
}

/// The stream a seed names is the sequence: the same seed twice is the same sequences, another
/// seed another, and a bank's channels draw different ones.
#[test]
fn the_seed_names_the_sequences_and_each_channel_draws_its_own() {
    let (a, b, c) = (bank(2, 5), bank(2, 5), bank(2, 6));
    assert_eq!(a.sequence(0), b.sequence(0));
    assert_eq!(a.sequence(1), b.sequence(1));
    assert_ne!(a.sequence(0), c.sequence(0));
    assert_ne!(a.sequence(0), a.sequence(1));
    assert_eq!(a.sequence(2), None);
}

/// Two channels fed one white input are correlated as their sequences are:
/// `rho = sum_n h_a[n] h_b[n]`, the zero-lag inner product, to within five standard deviations
/// of the sample estimate, `sqrt(V / N)` with `V` the sequences' own
/// [`covariance_variance`]; and a channel against itself is `1`, so the two are decorrelated
/// by as far as `rho` is from it.
#[test]
fn two_channels_correlate_as_their_sequences_do() {
    let mut d = bank(2, 21);
    let (ha, hb) = (response(&d, 0), response(&d, 1));
    let rho: f64 = ha.iter().zip(&hb).map(|(a, b)| a * b).sum();
    let n = 1 << 18;
    let x = white(n + 3_000, 99);
    let y = run(&mut d, &[x.clone(), x]);
    // The first `3000` frames are the filters filling.
    let sigma2 = 1.0 / 3.0;
    let estimate = |p: &[f64], q: &[f64]| {
        p[3_000..]
            .iter()
            .zip(&q[3_000..])
            .map(|(a, b)| a * b)
            .sum::<f64>()
            / n as f64
            / sigma2
    };
    let bound = |a: &[f64], b: &[f64]| 5.0 * (covariance_variance(a, b) / n as f64).sqrt();
    let got = estimate(&y[0], &y[1]);
    assert!(
        (got - rho).abs() <= bound(&ha, &hb),
        "{got} against {rho} within {}",
        bound(&ha, &hb)
    );
    let own = estimate(&y[0], &y[0]);
    assert!((own - 1.0).abs() <= bound(&ha, &ha), "{own} against 1");
    assert!(rho.abs() < 0.5 && 1.0 - rho.abs() > 5.0 * bound(&ha, &hb));
}

/// A channel's energy gain to white noise is `sum h^2`, which the normalization makes 1: the
/// measured `E y^2 / E x^2` is within five standard deviations of the taps' own `sum h^2`
/// (`sqrt(V / N)`, `V = 2 sum_l R(l)^2` for `a = b`).
#[test]
fn the_energy_gain_is_the_sum_of_the_squared_taps() {
    let mut d = bank(1, 8);
    let h = response(&d, 0);
    let analytic: f64 = h.iter().map(|v| v * v).sum();
    assert!((analytic - 1.0).abs() < 1e-12, "{analytic}");
    let n = 1 << 18;
    let x = white(n + 3_000, 4);
    let y = run(&mut d, &[x])[0].clone();
    let energy = y[3_000..].iter().map(|v| v * v).sum::<f64>() / n as f64 * 3.0;
    let bound = 5.0 * (covariance_variance(&h, &h) / n as f64).sqrt();
    assert!(
        (energy - analytic).abs() <= bound,
        "{energy} against {analytic} within {bound}"
    );
}

/// The tail is the latest impulse of any channel, and a density equal to the rate is one impulse
/// a frame: the positions are then the frames themselves.
#[test]
fn the_tail_is_the_latest_impulse_and_a_dense_sequence_fills_every_frame() {
    let d = bank(3, 2);
    let last = (0..3)
        .map(|c| d.sequence(c).unwrap().pulses().last().unwrap().position)
        .max()
        .unwrap();
    assert_eq!(d.tail(), last);
    let dense = VelvetNoise::new(
        VelvetDensity::new(Hz::new(20_000.0)).unwrap(),
        VelvetLength::new(Seconds::new(0.01)).unwrap(),
        SampleRate::of(20_000.0),
        1,
    )
    .unwrap();
    let positions: Vec<usize> = dense.pulses().iter().map(|p| p.position.get()).collect();
    assert_eq!(positions, (0..200).collect::<Vec<_>>());
}

/// A density above the rate is refused, whole, by the sequence and the decorrelator; one at the
/// rate is taken; and `clamped_to_rate` lowers a density to the rate and leaves a lower one.
#[test]
fn a_density_past_the_rate_is_refused_and_one_at_the_rate_is_not() {
    let density = VelvetDensity::new(Hz::new(20_000.0)).unwrap();
    let length = VelvetLength::new(Seconds::new(0.01)).unwrap();
    let under = SampleRate::of(19_999.0);
    let refused = VelvetNoiseError::DensityPastRate {
        density,
        rate: under,
    };
    assert_eq!(VelvetNoise::new(density, length, under, 1), Err(refused));
    assert_eq!(
        Decorrelator::<f64>::new(ChannelLayout::STEREO, density, length, under, 1).err(),
        Some(refused)
    );
    assert!(VelvetNoise::new(density, length, SampleRate::of(20_000.0), 1).is_ok());
    assert_eq!(density.clamped_to_rate(under).get(), Hz::new(19_999.0));
    assert_eq!(density.clamped_to_rate(RATE), density);
}
