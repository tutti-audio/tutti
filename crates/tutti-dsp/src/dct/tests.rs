use super::*;

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn full(n: usize) -> Dct2 {
    Dct2::new(nz(n), nz(n))
}

/// The columns of the matrix, `D e_n`.
fn columns(d: &Dct2) -> Vec<Vec<f64>> {
    let n = d.inputs();
    (0..n)
        .map(|j| {
            let mut e = vec![0.0; n];
            e[j] = 1.0;
            let mut out = vec![0.0; d.outputs()];
            d.process(&e, &mut out);
            out
        })
        .collect()
}

/// `D D^T = I`: the matrix is orthogonal, rows and columns unit and mutually perpendicular
/// (a square matrix's columns are orthonormal exactly when its rows are).
#[test]
fn the_dct_matrix_is_orthonormal() {
    for n in [1usize, 2, 7, 20, 40] {
        let cols = columns(&full(n));
        for (a, ca) in cols.iter().enumerate() {
            for (b, cb) in cols.iter().enumerate() {
                let dot: f64 = ca.iter().zip(cb).map(|(x, y)| x * y).sum();
                let want = if a == b { 1.0 } else { 0.0 };
                assert!((dot - want).abs() < 1e-12, "n {n} ({a},{b}): {dot}");
            }
        }
    }
}

/// A constant `c` of `N` points is the DC basis scaled: `c_0 = sqrt(N) c`, `c_k = 0` after.
#[test]
fn a_constant_vector_maps_to_its_dc_coefficient_alone() {
    let n = 16;
    let d = full(n);
    let mut out = vec![9.0; n];
    d.process(&vec![2.5; n], &mut out);
    assert!(
        (out[0] - 2.5 * (n as f64).sqrt()).abs() < 1e-12,
        "{}",
        out[0]
    );
    for (k, c) in out.iter().enumerate().skip(1) {
        assert!(c.abs() < 1e-12, "c_{k} = {c}");
    }
}

/// The basis cosine `cos(pi k (2n + 1) / 2N)` is the row `k` of the transform divided by
/// its scale, so it maps to `s_k * N / 2 = sqrt(N / 2)` at `k` and to zero elsewhere.
#[test]
fn a_cosine_at_index_k_maps_to_one_coefficient() {
    let n = 24usize;
    let d = full(n);
    for k in [1usize, 5, 23] {
        let x: Vec<f64> = (0..n)
            .map(|i| (core::f64::consts::PI * (k * (2 * i + 1)) as f64 / (2 * n) as f64).cos())
            .collect();
        let mut out = vec![0.0; n];
        d.process(&x, &mut out);
        for (j, c) in out.iter().enumerate() {
            let want = if j == k { (n as f64 / 2.0).sqrt() } else { 0.0 };
            assert!((c - want).abs() < 1e-12, "k {k} coefficient {j}: {c}");
        }
    }
}

/// Keeping `K < N` coefficients keeps the first `K` of the full transform, and more outputs
/// than inputs keeps `N`.
#[test]
fn keeping_fewer_outputs_is_a_prefix_and_outputs_cap_at_inputs() {
    let x: Vec<f64> = (0..12).map(|i| ((i * i) % 7) as f64 - 3.0).collect();
    let mut all = vec![0.0; 12];
    full(12).process(&x, &mut all);
    let few = Dct2::new(nz(12), nz(5));
    let mut head = vec![0.0; 5];
    few.process(&x, &mut head);
    assert_eq!(few.outputs(), 5);
    assert_eq!(head, all[..5]);
    assert_eq!(Dct2::new(nz(12), nz(40)).outputs(), 12);
}
