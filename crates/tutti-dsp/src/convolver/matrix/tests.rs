use super::*;
use tutti_buffers::{Mono, Planes, PlanesMut};
use tutti_fixtures::noise;

fn layout(n: u16) -> ChannelLayout {
    ChannelLayout::from(n)
}

/// A response of one tap: `gain` at `delay`.
fn tap(delay: usize, gain: f32) -> Vec<f32> {
    let mut ir = vec![0.0; delay + 1];
    ir[delay] = gain;
    ir
}

/// `x` through the matrix in calls of `lens` (cycled), `f32` views.
fn render(m: &mut MatrixConvolver, x: &[Vec<f32>], lens: &[usize]) -> Vec<Vec<f32>> {
    let frames = x[0].len();
    let mut out = vec![Vec::new(); usize::from(m.outputs().count())];
    let (mut at, mut k) = (0, 0);
    while at < frames {
        let n = lens[k % lens.len()].min(frames - at);
        let ins: Vec<&[f32]> = x.iter().map(|c| &c[at..at + n]).collect();
        let mut block = vec![vec![0.0f32; n]; out.len()];
        m.process(
            Planes::new(&ins).unwrap(),
            PlanesMut::from_vecs(&mut block).unwrap(),
        );
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
        let mut m = MatrixConvolver::new(layout(ni), layout(no), &refs(&irs), 32).unwrap();
        let want = definition(&irs, &x, usize::from(no), m.latency());
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
    let mut m = MatrixConvolver::new(layout(2), layout(2), &refs(&irs), 16).unwrap();
    let lat = m.latency();
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
        let mut m = MatrixConvolver::new(layout(2), layout(2), &refs(&irs), 16).unwrap();
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
    let mut whole = MatrixConvolver::new(layout(3), layout(2), &refs(&irs), 32).unwrap();
    let want = render(&mut whole, &x, &[1500]);
    for lens in [&[1usize][..], &[7, 61, 64, 65], &[31, 33], &[300, 5]] {
        let mut m = MatrixConvolver::new(layout(3), layout(2), &refs(&irs), 32).unwrap();
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
    let make = || MatrixConvolver::new(layout(2), layout(2), &refs(&irs), 32).unwrap();
    let want = render(&mut make(), &x, &[64]);
    let mut used = make();
    render(&mut used, &[noise(80, 500), noise(81, 500)], &[64]);
    let mut fresh = used.fresh();
    used.reset();
    for m in [&mut used, &mut fresh] {
        assert_eq!(render(m, &x, &[64]), want);
    }
}

/// The views' sample format is the caller's: `f64` views render what `f32` ones do.
#[test]
fn f64_views_render_the_f32_result() {
    let irs: Vec<Vec<f32>> = (0..4).map(|p| noise(5 + p, 200)).collect();
    let x: Vec<Vec<f32>> = (0..2).map(|c| noise(2 + c, 400)).collect();
    let mut m32 = MatrixConvolver::new(layout(2), layout(2), &refs(&irs), 32).unwrap();
    let want = render(&mut m32, &x, &[400]);
    let x64: Vec<Vec<f64>> = x
        .iter()
        .map(|c| c.iter().map(|&v| f64::from(v)).collect())
        .collect();
    let mut out = vec![vec![0.0f64; 400]; 2];
    let mut m64 = MatrixConvolver::new(layout(2), layout(2), &refs(&irs), 32).unwrap();
    m64.process(
        Planes::from_vecs(&x64).unwrap(),
        PlanesMut::from_vecs(&mut out).unwrap(),
    );
    for (o, w) in out.iter().zip(&want) {
        for (&a, &b) in o.iter().zip(w) {
            assert!((a - f64::from(b)).abs() < 1e-6);
        }
    }
}

/// A shape with no channel, or the wrong number of responses, is refused by name.
#[test]
fn a_bad_shape_is_refused_by_name() {
    let one = [1.0f32];
    assert_eq!(
        MatrixConvolver::new::<f32>(ChannelLayout::EMPTY, layout(2), &[], 16).err(),
        Some(MatrixConvolverError::NoChannels)
    );
    assert_eq!(
        MatrixConvolver::new(
            layout(2),
            layout(3),
            &[Mono::new(&one), Mono::new(&one)],
            16
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
