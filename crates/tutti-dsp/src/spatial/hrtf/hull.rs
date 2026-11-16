//! The convex hull of points in 3D, as outward-wound triangles.
//!
//! An incremental hull: a tetrahedron of four well-spread points, then each
//! remaining point replaces the faces it sees by a fan from the horizon of
//! those faces to the point. A point inside the hull sees no face and is not
//! a vertex of the result. Coplanar points on the hull's surface are
//! triangulated, never merged. Time is `O(n · faces)`, which is quadratic and
//! enough for the few thousand directions of a measured HRIR set.

use std::collections::HashSet;

type Vec3 = [f64; 3];

/// The smallest `|det|` of three edge vectors from one point that counts as
/// a tetrahedron with volume; for points of unit length, a thinner one is
/// coplanar.
const MIN_VOLUME: f64 = 1e-9;

/// How far above a face's plane a point must lie to see the face. Rounding in
/// the plane and the distance is of order `1e-16` for points of unit length.
const SEES: f64 = 1e-12;

fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// A hull face: its vertices, counter-clockwise seen from outside, and its
/// plane `dot(normal, p) = offset` with the unit normal pointing outward.
struct Face {
    verts: [u32; 3],
    normal: Vec3,
    offset: f64,
}

impl Face {
    fn new(verts: [u32; 3], points: &[Vec3]) -> Self {
        let [a, b, c] = verts.map(|v| points[v as usize]);
        let n = cross(sub(b, a), sub(c, a));
        let len = dot(n, n).sqrt();
        let normal = n.map(|x| x / len);
        Self {
            verts,
            normal,
            offset: dot(normal, a),
        }
    }

    /// The signed distance of `p` above the plane; NaN for a face of zero
    /// area, which no point sees.
    fn height(&self, p: Vec3) -> f64 {
        dot(self.normal, p) - self.offset
    }

    fn edges(&self) -> [(u32, u32); 3] {
        let [a, b, c] = self.verts;
        [(a, b), (b, c), (c, a)]
    }
}

/// The triangles of the convex hull of `points`, each a triple of indices
/// into `points` wound counter-clockwise seen from outside.
///
/// `None` for fewer than four points, or points that are all coplanar (the
/// hull has no volume).
pub(super) fn convex_hull(points: &[Vec3]) -> Option<Vec<[u32; 3]>> {
    let seed = tetrahedron(points)?;
    let [a, b, c, d] = seed;
    let mut faces: Vec<Face> = [[a, b, c], [a, d, b], [b, d, c], [c, d, a]]
        .into_iter()
        .map(|verts| {
            let face = Face::new(verts, points);
            // The vertex the face does not hold lies behind it.
            let opposite = seed.iter().copied().find(|v| !verts.contains(v))?;
            if face.height(points[opposite as usize]) > 0.0 {
                let [x, y, z] = verts;
                Some(Face::new([x, z, y], points))
            } else {
                Some(face)
            }
        })
        .collect::<Option<_>>()?;

    for (i, &p) in points.iter().enumerate() {
        let i = i as u32;
        if seed.contains(&i) {
            continue;
        }
        let seen: Vec<usize> = (0..faces.len())
            .filter(|&f| faces[f].height(p) > SEES)
            .collect();
        if seen.is_empty() {
            continue;
        }
        let edges: HashSet<(u32, u32)> = seen.iter().flat_map(|&f| faces[f].edges()).collect();
        // A seen face's edge whose twin is not seen lies on the horizon.
        let horizon: Vec<(u32, u32)> = edges
            .iter()
            .copied()
            .filter(|&(x, y)| !edges.contains(&(y, x)))
            .collect();
        // Descending, so a removal moves only a face already passed over.
        for &f in seen.iter().rev() {
            faces.swap_remove(f);
        }
        let mut fan: Vec<[u32; 3]> = horizon.into_iter().map(|(x, y)| [x, y, i]).collect();
        // The horizon came out of a hash set: sort, so the result does not
        // depend on its order.
        fan.sort_unstable();
        faces.extend(fan.into_iter().map(|verts| Face::new(verts, points)));
    }
    let mut triangles: Vec<[u32; 3]> = faces.into_iter().map(|f| f.verts).collect();
    triangles.sort_unstable();
    Some(triangles)
}

/// Four points that span a tetrahedron: the first, the farthest from it, the
/// one farthest from their line, the one farthest from their plane.
fn tetrahedron(points: &[Vec3]) -> Option<[u32; 4]> {
    let farthest = |score: &dyn Fn(Vec3) -> f64| -> Option<(u32, f64)> {
        points
            .iter()
            .enumerate()
            .map(|(i, &p)| (i as u32, score(p).abs()))
            .max_by(|x, y| x.1.total_cmp(&y.1))
    };
    let p0 = *points.first()?;
    let (i1, far) = farthest(&|p| dot(sub(p, p0), sub(p, p0)))?;
    if far < MIN_VOLUME * MIN_VOLUME {
        return None;
    }
    let e1 = sub(points[i1 as usize], p0);
    let (i2, area) = farthest(&|p| {
        let n = cross(e1, sub(p, p0));
        dot(n, n)
    })?;
    if area < MIN_VOLUME * MIN_VOLUME {
        return None;
    }
    let normal = cross(e1, sub(points[i2 as usize], p0));
    let (i3, volume) = farthest(&|p| dot(normal, sub(p, p0)))?;
    (volume >= MIN_VOLUME).then_some([0, i1, i2, i3])
}

#[cfg(test)]
mod tests;
