//! [`Rotation`]: the matrix that turns an ambisonic sound field, and the rotator that
//! applies a glide between two of them.

use tutti_buffers::{AmbisonicOrder, Float, MAX_CHANNELS, Planes, PlanesMut};
use tutti_units::{ArcDegrees, Direction};

use crate::design::along;
use crate::{COEFF_INTERVAL, Lerp, Ramp};

/// The highest degree: the order cap.
const MAX_DEGREE: usize = 3;

/// A square matrix over the ambisonic channels, row by column.
type Matrix = [[f64; MAX_CHANNELS]; MAX_CHANNELS];

/// A rotation of the sound field about the listener, as the matrix on ambisonic channels
/// that carries a source at `d` to a source at `R d`: for every direction `d`,
/// `Harmonics(R d) = M Harmonics(d)`.
///
/// `R = Rz(azimuth) Ry(-elevation) Rx(roll)`, each a right-handed rotation about an axis of
/// the listener's frame (`x` ahead, `y` left, `z` up): it carries a source straight ahead to
/// the `front` direction, and `roll` first turns the field about the front axis (positive
/// from `y` toward `z`). A turn of the front along the horizon by `t` carries a source at
/// bearing `phi` to `phi + t`.
///
/// The matrix is block diagonal by degree, built from `R` by the recursion of Ivanic and
/// Ruedenberg (Rotation Matrices for Real Spherical Harmonics, 1996, with the corrections
/// of 1998). It is the same in SN3D and N3D, whose channels differ by a factor per
/// degree, and it is orthogonal: it preserves the energy of every degree.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rotation {
    order: AmbisonicOrder,
    matrix: Matrix,
}

impl Rotation {
    /// The rotation that carries the front to `front`, after a `roll` about the front.
    #[must_use]
    pub fn new(order: AmbisonicOrder, front: Direction, roll: ArcDegrees) -> Self {
        let r = cartesian(
            front.azimuth.get().to_radians(),
            -front.elevation.get().to_radians(),
            roll.get().to_radians(),
        );
        // `blocks[n]` is the degree-`n` block, `(2n + 1)^2` entries indexed by `m + n`.
        let mut blocks = [[[0.0; 2 * MAX_DEGREE + 1]; 2 * MAX_DEGREE + 1]; MAX_DEGREE + 1];
        blocks[0][0][0] = 1.0;
        // Degree 1 in ACN order `(y, z, x)`: index `m + 1` is the axis `[1, 2, 0][m + 1]`.
        const AXIS: [usize; 3] = [1, 2, 0];
        for (i, row) in blocks[1].iter_mut().take(3).enumerate() {
            for (j, entry) in row.iter_mut().take(3).enumerate() {
                *entry = r[AXIS[i]][AXIS[j]];
            }
        }
        for n in 2..=usize::from(order.get()) {
            let (lower, upper) = blocks.split_at_mut(n);
            recurse(n, &lower[n - 1], &lower[1], &mut upper[0]);
        }
        let mut matrix = [[0.0; MAX_CHANNELS]; MAX_CHANNELS];
        for n in 0..=usize::from(order.get()) {
            for i in 0..=2 * n {
                for j in 0..=2 * n {
                    matrix[n * n + i][n * n + j] = blocks[n][i][j];
                }
            }
        }
        Self { order, matrix }
    }

    /// The inverse rotation: the transpose, as the matrix is orthogonal.
    #[must_use]
    pub fn inverse(&self) -> Self {
        let mut matrix = [[0.0; MAX_CHANNELS]; MAX_CHANNELS];
        for (i, row) in matrix.iter_mut().enumerate() {
            for (j, entry) in row.iter_mut().enumerate() {
                *entry = self.matrix[j][i];
            }
        }
        Self {
            order: self.order,
            matrix,
        }
    }

    /// The order.
    #[must_use]
    pub const fn order(&self) -> AmbisonicOrder {
        self.order
    }

    /// The entry at output channel `row`, input channel `column` (ACN), zero outside the
    /// channels of the order.
    #[must_use]
    pub fn get(&self, row: usize, column: usize) -> f64 {
        self.matrix
            .get(row)
            .and_then(|r| r.get(column))
            .copied()
            .unwrap_or(0.0)
    }
}

/// Linear in every entry.
impl Lerp for Rotation {
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        let t = along::<f64>(k, n);
        let mut matrix = self.matrix;
        for (row, (from, to)) in matrix.iter_mut().zip(self.matrix.iter().zip(&to.matrix)) {
            for (v, (&a, &b)) in row.iter_mut().zip(from.iter().zip(to)) {
                *v = crate::lerp(a, b, t);
            }
        }
        Self {
            order: self.order,
            matrix,
        }
    }
}

/// `Rz(yaw) Ry(pitch) Rx(roll)` as a matrix on `(x, y, z)`, the angles in radians.
fn cartesian(yaw: f64, pitch: f64, roll: f64) -> [[f64; 3]; 3] {
    let (sy, cy) = (libm::sin(yaw), libm::cos(yaw));
    let (sp, cp) = (libm::sin(pitch), libm::cos(pitch));
    let (sr, cr) = (libm::sin(roll), libm::cos(roll));
    let rz = [[cy, -sy, 0.0], [sy, cy, 0.0], [0.0, 0.0, 1.0]];
    let ry = [[cp, 0.0, sp], [0.0, 1.0, 0.0], [-sp, 0.0, cp]];
    let rx = [[1.0, 0.0, 0.0], [0.0, cr, -sr], [0.0, sr, cr]];
    let mul = |a: [[f64; 3]; 3], b: [[f64; 3]; 3]| {
        let mut c = [[0.0; 3]; 3];
        for (i, row) in c.iter_mut().enumerate() {
            for (j, entry) in row.iter_mut().enumerate() {
                *entry = (0..3).map(|k| a[i][k] * b[k][j]).sum();
            }
        }
        c
    };
    mul(rz, mul(ry, rx))
}

type Block = [[f64; 2 * MAX_DEGREE + 1]; 2 * MAX_DEGREE + 1];

/// The entry `(a, b)` of a block, with `a` and `b` in `-n..=n`.
fn at(block: &Block, n: usize, a: isize, b: isize) -> f64 {
    let n = n as isize;
    if a.abs() > n || b.abs() > n {
        return 0.0;
    }
    block[(a + n) as usize][(b + n) as usize]
}

/// Degree `n` of the recursion: the block from the block `below` of degree `n - 1` and
/// the degree-1 block `first`.
fn recurse(n: usize, below: &Block, first: &Block, block: &mut Block) {
    let l = n as isize;
    let r1 = |i: isize, j: isize| at(first, 1, i, j);
    let prev = |a: isize, b: isize| at(below, n - 1, a, b);
    // `P(i, a, b)` of the recursion.
    let p = |i: isize, a: isize, b: isize| {
        if b == l {
            r1(i, 1) * prev(a, l - 1) - r1(i, -1) * prev(a, -l + 1)
        } else if b == -l {
            r1(i, 1) * prev(a, -l + 1) + r1(i, -1) * prev(a, l - 1)
        } else {
            r1(i, 0) * prev(a, b)
        }
    };
    let delta = |a: isize, b: isize| if a == b { 1.0 } else { 0.0 };
    for m in -l..=l {
        for nn in -l..=l {
            let d = if nn.abs() < l {
                ((l + nn) * (l - nn)) as f64
            } else {
                (2 * l * (2 * l - 1)) as f64
            };
            let am = m.abs();
            let u = libm::sqrt(((l + m) * (l - m)) as f64 / d);
            let v = 0.5
                * libm::sqrt((1.0 + delta(m, 0)) * ((l + am - 1) * (l + am)) as f64 / d)
                * (1.0 - 2.0 * delta(m, 0));
            let w = -0.5 * libm::sqrt(((l - am - 1) * (l - am)) as f64 / d) * (1.0 - delta(m, 0));
            let mut value = 0.0;
            if u != 0.0 {
                value += u * p(0, m, nn);
            }
            if v != 0.0 {
                let big_v = if m == 0 {
                    p(1, 1, nn) + p(-1, -1, nn)
                } else if m > 0 {
                    p(1, m - 1, nn) * libm::sqrt(1.0 + delta(m, 1))
                        - p(-1, -m + 1, nn) * (1.0 - delta(m, 1))
                } else {
                    p(1, m + 1, nn) * (1.0 - delta(m, -1))
                        + p(-1, -m - 1, nn) * libm::sqrt(1.0 + delta(m, -1))
                };
                value += v * big_v;
            }
            if w != 0.0 {
                let big_w = if m > 0 {
                    p(1, m + 1, nn) + p(-1, -m - 1, nn)
                } else {
                    p(1, m - 1, nn) - p(-1, -m + 1, nn)
                };
                value += w * big_w;
            }
            block[(m + l) as usize][(nn + l) as usize] = value;
        }
    }
}

/// The rotator of an ambisonic signal: a [`Rotation`] that glides to a new one over
/// [`COEFF_INTERVAL`] frames.
#[derive(Clone, Copy, Debug)]
pub struct AmbisonicRotator {
    rotation: Ramp<Rotation>,
}

impl AmbisonicRotator {
    /// A rotator at rest on `rotation`.
    #[must_use]
    pub fn new(rotation: Rotation) -> Self {
        Self {
            rotation: Ramp::new(rotation).steps(COEFF_INTERVAL as u32),
        }
    }

    /// Glides to `rotation` over [`COEFF_INTERVAL`] frames, from where the glide is now.
    pub fn retarget(&mut self, rotation: Rotation) {
        self.rotation.retarget(rotation);
    }

    /// Lands on the rotation it glides to at once.
    pub fn jump_to_target(&mut self) {
        self.rotation.jump_to_target();
    }

    /// Rotates `input` into `output`, both the channels of the order and as many frames:
    /// each frame is the blend of the glide's two matrices applied to it.
    ///
    /// The blend of `M0` and `M1` at `t` applied to a frame `x` is `(M0 + t (M1 - M0)) x`;
    /// the frames past the glide apply `M1` alone. The matrix is block diagonal, so the
    /// zero entries off the blocks are skipped.
    pub fn process<F: Float>(&mut self, input: Planes<'_, F>, mut output: PlanesMut<'_, '_, F>) {
        let frames = input.plane(0).samples().len();
        let width = usize::from(self.rotation.target().order().channels().count());
        let blended = self.rotation.blended_frames(frames);
        let first = self.rotation.steps_taken() + 1;
        let (from, to) = (self.rotation.from.matrix, self.rotation.to.matrix);
        let mut t = [F::ZERO; COEFF_INTERVAL];
        for (j, t) in t.iter_mut().enumerate().take(blended) {
            *t = along::<F>(first + j as u32, self.rotation.n);
        }
        for (i, row) in from.iter().enumerate().take(width) {
            let mut out = output.plane_mut(i);
            let out = out.samples_mut();
            out.fill(F::ZERO);
            for (j, &base) in row.iter().enumerate().take(width) {
                let end = to[i][j];
                if base == 0.0 && end == 0.0 {
                    continue;
                }
                let x = input.plane(j);
                let x = x.samples();
                let (base, slope, end) = (
                    F::from_design(base),
                    F::from_design(end - base),
                    F::from_design(end),
                );
                let (head, tail) = x.split_at(blended);
                let (out_head, out_tail) = out.split_at_mut(blended);
                for ((o, &x), &t) in out_head.iter_mut().zip(head).zip(&t) {
                    *o += (base + slope * t) * x;
                }
                for (o, &x) in out_tail.iter_mut().zip(tail) {
                    *o += end * x;
                }
            }
        }
        self.rotation
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}
