use super::*;
use tutti_buffers::InputOutput;
use tutti_buffers::{Mono, Planes, PlanesMut};
use tutti_fixtures::noise;

fn layout(n: u16) -> ChannelLayout {
    ChannelLayout::from(n)
}

/// The matrix of `inputs` by `outputs` channels over `irs`, at the head partition `block`.
fn build<F: FftFloat, S: Sample>(
    inputs: ChannelLayout,
    outputs: ChannelLayout,
    irs: &[Mono<'_, S>],
    block: Samples,
) -> Result<MatrixConvolver<F>, MatrixConvolverError> {
    MatrixConvolverConfig::new(inputs, outputs, irs)
        .block_size(block)
        .build()
}

/// A response of one tap: `gain` at `delay`.
fn tap(delay: usize, gain: f32) -> Vec<f32> {
    let mut ir = vec![0.0; delay + 1];
    ir[delay] = gain;
    ir
}

/// `x` through the matrix in calls of `lens` (cycled), `f32` views: the session's audio format,
/// so the tolerances of the tests that use it are `f32`'s.
fn render(m: &mut MatrixConvolver<f32>, x: &[Vec<f32>], lens: &[usize]) -> Vec<Vec<f32>> {
    let frames = x[0].len();
    let mut out = vec![Vec::new(); usize::from(m.outputs().count())];
    let (mut at, mut k) = (0, 0);
    while at < frames {
        let n = lens[k % lens.len()].min(frames - at);
        let ins: Vec<&[f32]> = x.iter().map(|c| &c[at..at + n]).collect();
        let mut block = vec![vec![0.0f32; n]; out.len()];
        m.process(
            &mut InputOutput::paired(
                Planes::new(&ins).unwrap(),
                PlanesMut::from_vecs(&mut block).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        for (o, b) in out.iter_mut().zip(block) {
            o.extend(b);
        }
        at += n;
        k += 1;
    }
    out
}

/// Output `o` of the definition: `sum_i sum_k ir(i, o)[k] x_i[n - k - latency]`, in `f64`.
fn definition(irs: &[Vec<f32>], x: &[Vec<f32>], outputs: usize, latency: usize) -> Vec<Vec<f64>> {
    let frames = x[0].len();
    (0..outputs)
        .map(|o| {
            (0..frames)
                .map(|n| {
                    let Some(m) = n.checked_sub(latency) else {
                        return 0.0;
                    };
                    let mut sum = 0.0;
                    for (i, xi) in x.iter().enumerate() {
                        let ir = &irs[i * outputs + o];
                        for (k, &h) in ir.iter().enumerate().take(m + 1) {
                            sum += f64::from(h) * f64::from(xi[m - k]);
                        }
                    }
                    sum
                })
                .collect()
        })
        .collect()
}

fn refs(irs: &[Vec<f32>]) -> Vec<Mono<'_, f32>> {
    irs.iter().map(|ir| Mono::new(ir)).collect()
}

/// Every output is the sum over inputs of the input through its own path, by the
/// definition of convolution, for a square and two non-square shapes: a path routed to the
/// wrong output, or an input read by the wrong row, moves samples by orders of magnitude
/// more than the round-off bound.
#[test]
fn each_output_is_the_sum_of_its_paths_by_definition() {
    for (ni, no) in [(2u16, 2u16), (3, 2), (1, 4), (4, 3)] {
        let irs: Vec<Vec<f32>> = (0..ni * no)
            .map(|p| {
                noise(100 + u32::from(p), 40 + 37 * usize::from(p))
                    .iter()
                    .map(|v| v * 0.2)
                    .collect()
            })
            .collect();
        let x: Vec<Vec<f32>> = (0..ni).map(|c| noise(7 + u32::from(c), 900)).collect();
        let mut m = build::<f32, _>(layout(ni), layout(no), &refs(&irs), Samples::new(32)).unwrap();
        let want = definition(&irs, &x, usize::from(no), m.latency().get());
        let got = render(&mut m, &x, &[64]);
        for (o, (g, w)) in got.iter().zip(&want).enumerate() {
            for (n, (&g, &w)) in g.iter().zip(w).enumerate() {
                assert!(
                    (f64::from(g) - w).abs() < 1e-4,
                    "{ni}x{no} out {o} frame {n}: {g} vs {w}"
                );
            }
        }
    }
}

/// An impulse on input `j` leaves output `i` as the response of path `(j, i)` and nothing
/// else, at the declared latency: four single-tap responses at distinct delays and gains
/// make a swapped or dropped path a different figure.
#[test]
fn an_impulse_on_an_input_is_each_outputs_path_response() {
    let irs = [tap(3, 0.5), tap(7, -0.25), tap(11, 0.75), tap(5, 0.125)];
    let mut m = build::<f32, _>(layout(2), layout(2), &refs(&irs), Samples::new(16)).unwrap();
    let lat = m.latency().get();
    for j in 0..2 {
        m.reset();
        let mut x = vec![vec![0.0f32; 128]; 2];
        x[j][0] = 1.0;
        let got = render(&mut m, &x, &[16]);
        for (o, g) in got.iter().enumerate() {
            let mut want = vec![0.0f32; 128];
            let ir = &irs[j * 2 + o];
            want[lat + ir.len() - 1] = *ir.last().unwrap();
            for (n, (&g, &w)) in g.iter().zip(&want).enumerate() {
                assert!((g - w).abs() < 1e-6, "in {j} out {o} frame {n}: {g} vs {w}");
            }
        }
    }
}

/// Linearity: a signal on both inputs is the sum of each input alone.
#[test]
fn both_inputs_together_are_the_sum_of_each_alone() {
    let irs = [tap(3, 0.5), tap(7, -0.25), tap(11, 0.75), tap(5, 0.125)];
    let (a, b) = (noise(1, 300), noise(2, 300));
    let silent = vec![0.0f32; 300];
    let run = |x: [&Vec<f32>; 2]| {
        let x = [x[0].clone(), x[1].clone()];
        let mut m = build::<f32, _>(layout(2), layout(2), &refs(&irs), Samples::new(16)).unwrap();
        render(&mut m, &x, &[50])
    };
    let both = run([&a, &b]);
    let (only_a, only_b) = (run([&a, &silent]), run([&silent, &b]));
    for o in 0..2 {
        for n in 0..300 {
            let want = only_a[o][n] + only_b[o][n];
            assert!((both[o][n] - want).abs() < 1e-5, "out {o} frame {n}");
        }
    }
}

/// Every call length gives the same bits, from one frame to past two partitions and a
/// whole stream, with responses long enough to be cut into levels.
#[test]
fn any_call_length_gives_the_same_bits() {
    let irs: Vec<Vec<f32>> = (0..6)
        .map(|p| noise(30 + p, 2500 + 100 * p as usize))
        .collect();
    let x: Vec<Vec<f32>> = (0..3).map(|c| noise(9 + c, 1500)).collect();
    let mut whole = build::<f32, _>(layout(3), layout(2), &refs(&irs), Samples::new(32)).unwrap();
    let want = render(&mut whole, &x, &[1500]);
    for lens in [&[1usize][..], &[7, 61, 64, 65], &[31, 33], &[300, 5]] {
        let mut m = build::<f32, _>(layout(3), layout(2), &refs(&irs), Samples::new(32)).unwrap();
        let got = render(&mut m, &x, lens);
        assert!(
            got.iter()
                .flatten()
                .zip(want.iter().flatten())
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "calls {lens:?} differ from one call"
        );
    }
}

/// A matrix reset after running renders the bits of a fresh one, and so does a `fresh`
/// copy of a matrix that has run.
#[test]
fn reset_and_fresh_equal_a_new_matrix() {
    let irs: Vec<Vec<f32>> = (0..4).map(|p| noise(60 + p, 700)).collect();
    let x: Vec<Vec<f32>> = (0..2).map(|c| noise(3 + c, 600)).collect();
    let make = || build::<f32, _>(layout(2), layout(2), &refs(&irs), Samples::new(32)).unwrap();
    let want = render(&mut make(), &x, &[64]);
    let mut used = make();
    render(&mut used, &[noise(80, 500), noise(81, 500)], &[64]);
    let mut fresh = used.fresh();
    used.reset();
    for m in [&mut used, &mut fresh] {
        assert_eq!(render(m, &x, &[64]), want);
    }
}

/// A shape with no channel, or the wrong number of responses, is refused by name.
#[test]
fn a_bad_shape_is_refused_by_name() {
    let one = [1.0f32];
    assert_eq!(
        build::<f32, f32>(ChannelLayout::EMPTY, layout(2), &[], Samples::new(16)).err(),
        Some(MatrixConvolverError::NoChannels)
    );
    assert_eq!(
        build::<f32, _>(
            layout(2),
            layout(3),
            &[Mono::new(&one), Mono::new(&one)],
            Samples::new(16)
        )
        .err(),
        Some(MatrixConvolverError::IrCount {
            inputs: 2,
            outputs: 3,
            want: 6,
            got: 2
        })
    );
}

/// The matrix of `f64` unit-impulse responses is the identity at its latency, on every
/// channel, to the `f64` FFT's rounding (about 1e-12): the signal needs more than 24 bits,
/// so an `f32` engine misses by 1e-8.
#[test]
fn an_f64_matrix_of_unit_impulses_is_the_identity_at_its_latency() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let (frames, unit, zero) = (400, [1.0f64], [0.0f64; 1]);
    let irs = [
        Mono::new(&unit),
        Mono::new(&zero),
        Mono::new(&zero),
        Mono::new(&unit),
    ];
    let mut m = build::<f64, _>(layout(2), layout(2), &irs, Samples::new(32)).unwrap();
    let lat = m.latency().get();
    let left = wide_signal(frames);
    let right: Vec<f64> = left.iter().rev().map(|v| -v).collect();
    let mut out = vec![vec![0.0f64; frames]; 2];
    m.process(
        &mut InputOutput::paired(
            Planes::from_vecs(&[left.clone(), right.clone()]).unwrap(),
            PlanesMut::from_vecs(&mut out).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    for (got, x) in out.iter().zip([&left, &right]) {
        let mut want = vec![0.0f64; frames];
        want[lat..].copy_from_slice(&x[..frames - lat]);
        assert_close_f64(got, &want, 1e-12);
    }
}

/// A delayed, scaled impulse is the input delayed by that many frames and scaled: with
/// `LR = 0.5 z^-5` and `RL = 0.25 z^-9` between unit diagonals, `out_L[n] = x_L[n - lat] +
/// 0.25 x_R[n - lat - 9]` and `out_R[n] = 0.5 x_L[n - lat - 5] + x_R[n - lat]`, in `f64` to
/// 1e-12.
#[test]
fn an_f64_matrix_of_delayed_scaled_impulses_delays_and_scales() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    fn tap64(delay: usize, gain: f64) -> Vec<f64> {
        let mut ir = vec![0.0; delay + 1];
        ir[delay] = gain;
        ir
    }
    let frames = 400;
    let irs = [tap64(0, 1.0), tap64(5, 0.5), tap64(9, 0.25), tap64(0, 1.0)];
    let monos: Vec<Mono<'_, f64>> = irs.iter().map(|ir| Mono::new(ir)).collect();
    let mut m = build::<f64, _>(layout(2), layout(2), &monos, Samples::new(32)).unwrap();
    let lat = m.latency().get();
    let left = wide_signal(frames);
    let right: Vec<f64> = left.iter().rev().map(|v| v * 0.75).collect();
    let mut out = vec![vec![0.0f64; frames]; 2];
    m.process(
        &mut InputOutput::paired(
            Planes::from_vecs(&[left.clone(), right.clone()]).unwrap(),
            PlanesMut::from_vecs(&mut out).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let at = |x: &[f64], n: usize, delay: usize| n.checked_sub(lat + delay).map_or(0.0, |k| x[k]);
    let want_l: Vec<f64> = (0..frames)
        .map(|n| at(&left, n, 0) + 0.25 * at(&right, n, 9))
        .collect();
    let want_r: Vec<f64> = (0..frames)
        .map(|n| 0.5 * at(&left, n, 5) + at(&right, n, 0))
        .collect();
    assert_close_f64(&out[0], &want_l, 1e-12);
    assert_close_f64(&out[1], &want_r, 1e-12);
}
