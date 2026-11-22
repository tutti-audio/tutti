//! Radau IIA of three stages and order 5, the reference integrator of the nonlinear tests.

/// One step `h` of Radau IIA (three stages, order 5) on `u' = rhs(t, u)` from `u` at `t`.
///
/// The stage slopes `K_i = rhs(t + c_i h, u + h sum_j a_ij K_j)` are found by Newton on the
/// `3N`-dimensional system with the Jacobian `I - h (A (x) J)`, `J` read from `jac` at each
/// stage, to `1e-13` of the largest slope (50 iterations at most). A Jacobian that is only close
/// (the iteration is then a chord iteration) converges to the same `K`. The local error is
/// `O((h L)^6)` for the Lipschitz constant `L` of `rhs`.
pub(crate) fn radau5_step<const N: usize>(
    rhs: &impl Fn(f64, &[f64; N]) -> [f64; N],
    jac: &impl Fn(f64, &[f64; N]) -> [[f64; N]; N],
    t: f64,
    u: &[f64; N],
    h: f64,
) -> [f64; N] {
    let r6 = 6.0f64.sqrt();
    let c = [(4.0 - r6) / 10.0, (4.0 + r6) / 10.0, 1.0];
    let a = [
        [
            (88.0 - 7.0 * r6) / 360.0,
            (296.0 - 169.0 * r6) / 1800.0,
            (-2.0 + 3.0 * r6) / 225.0,
        ],
        [
            (296.0 + 169.0 * r6) / 1800.0,
            (88.0 + 7.0 * r6) / 360.0,
            (-2.0 - 3.0 * r6) / 225.0,
        ],
        [(16.0 - r6) / 36.0, (16.0 + r6) / 36.0, 1.0 / 9.0],
    ];
    let at = |k: &[[f64; N]; 3], i: usize| -> [f64; N] {
        core::array::from_fn(|n| u[n] + h * (0..3).map(|j| a[i][j] * k[j][n]).sum::<f64>())
    };
    let mut k = [rhs(t, u); 3];
    let mut m = vec![vec![0.0f64; 3 * N + 1]; 3 * N];
    for _ in 0..50 {
        let stage: [[f64; N]; 3] = core::array::from_fn(|i| at(&k, i));
        let size = 3 * N;
        m.iter_mut().for_each(|row| row.fill(0.0));
        for i in 0..3 {
            let f = rhs(t + c[i] * h, &stage[i]);
            for n in 0..N {
                m[i * N + n][size] = f[n] - k[i][n];
            }
        }
        for j in 0..3 {
            let jacobian = jac(t + c[j] * h, &stage[j]);
            for i in 0..3 {
                for n in 0..N {
                    for p in 0..N {
                        m[i * N + n][j * N + p] =
                            f64::from(u8::from(i == j && n == p)) - h * a[i][j] * jacobian[n][p];
                    }
                }
            }
        }
        // Elimination with partial pivoting.
        for p in 0..size {
            let pivot = (p..size)
                .max_by(|&x, &y| m[x][p].abs().total_cmp(&m[y][p].abs()))
                .expect("a row");
            m.swap(p, pivot);
            for i in p + 1..size {
                let r = m[i][p] / m[p][p];
                for j in p..=size {
                    m[i][j] -= r * m[p][j];
                }
            }
        }
        let mut dk = [0.0f64; 64];
        for i in (0..size).rev() {
            dk[i] = (m[i][size] - (i + 1..size).map(|j| m[i][j] * dk[j]).sum::<f64>()) / m[i][i];
        }
        for i in 0..3 {
            for n in 0..N {
                k[i][n] += dk[i * N + n];
            }
        }
        let scale = k.iter().flatten().fold(1.0f64, |m, v| m.max(v.abs()));
        if dk.iter().all(|d| d.abs() <= 1e-13 * scale) {
            break;
        }
    }
    at(&k, 2)
}
