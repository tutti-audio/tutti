use std::collections::HashMap;

use crate::testing::uniform;
use tutti_rt::SplitMix64;

use super::*;

/// Every property a closed convex triangulation has, checked against the
/// points alone: each point is on or behind every face's plane, each
/// directed edge appears once and its reverse once (a closed, consistently
/// wound surface), and every point is a vertex.
fn assert_convex_triangulation(points: &[Vec3], faces: &[[u32; 3]]) {
    for (f, tri) in faces.iter().enumerate() {
        let face = Face::new(*tri, points);
        for (i, &p) in points.iter().enumerate() {
            assert!(
                face.height(p) <= 1e-9,
                "point {i} lies {} outside face {f} {tri:?}",
                face.height(p)
            );
        }
    }
    let mut directed: HashMap<(u32, u32), usize> = HashMap::new();
    for tri in faces {
        for edge in Face::new(*tri, points).edges() {
            *directed.entry(edge).or_default() += 1;
        }
    }
    for (&(a, b), &n) in &directed {
        assert_eq!(n, 1, "edge {a}->{b} is wound by {n} faces");
        assert_eq!(
            directed.get(&(b, a)),
            Some(&1),
            "edge {a}->{b} has no twin, so the surface is not closed"
        );
    }
    let vertices: HashSet<u32> = faces.iter().flatten().copied().collect();
    assert_eq!(vertices.len(), points.len(), "every point is a vertex");
    // Euler's formula for a sphere: V - E + F = 2.
    let (v, e, f) = (
        vertices.len() as i64,
        directed.len() as i64 / 2,
        faces.len() as i64,
    );
    assert_eq!(v - e + f, 2, "V - E + F for V={v}, E={e}, F={f}");
}

#[test]
fn the_octahedrons_hull_has_eight_faces() {
    let points = [
        [1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, -1.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, -1.0],
    ];
    let faces = convex_hull(&points).expect("the octahedron has volume");
    assert_eq!(faces.len(), 8);
    assert_convex_triangulation(&points, &faces);
}

/// The cube's six square faces are four coplanar points each: two triangles
/// every, twelve in all.
#[test]
fn the_cubes_hull_triangulates_each_square_face_in_two() {
    let mut points = Vec::new();
    for x in [-1.0, 1.0] {
        for y in [-1.0, 1.0] {
            for z in [-1.0, 1.0] {
                points.push([x, y, z]);
            }
        }
    }
    let faces = convex_hull(&points).expect("the cube has volume");
    assert_eq!(faces.len(), 12);
    assert_convex_triangulation(&points, &faces);
}

/// A seeded cloud on the sphere is in convex position, so the hull has all
/// `n` vertices and `2n - 4` faces.
#[test]
fn a_cloud_on_the_sphere_is_a_closed_convex_triangulation() {
    for (seed, n) in [(1u64, 40usize), (2, 300), (3, 700)] {
        let mut rng = SplitMix64::new(seed);
        let points: Vec<Vec3> = (0..n)
            .map(|_| {
                let z: f64 = uniform(&mut rng, -1.0, 1.0);
                let phi: f64 = uniform(&mut rng, 0.0, std::f64::consts::TAU);
                let r = (1.0 - z * z).sqrt();
                [r * phi.cos(), r * phi.sin(), z]
            })
            .collect();
        let faces = convex_hull(&points).expect("a cloud on the sphere has volume");
        assert_eq!(faces.len(), 2 * n - 4, "seed {seed}");
        assert_convex_triangulation(&points, &faces);
    }
}

#[test]
fn an_interior_point_is_not_a_vertex() {
    let points = [
        [1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, -1.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, -1.0],
        [0.1, 0.1, 0.1],
    ];
    let faces = convex_hull(&points).expect("the octahedron has volume");
    assert_eq!(faces.len(), 8);
    assert!(faces.iter().flatten().all(|&v| v != 6));
}

#[test]
fn points_without_volume_have_no_hull() {
    assert!(convex_hull(&[]).is_none());
    assert!(convex_hull(&[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]).is_none());
    let ring: Vec<Vec3> = (0..12)
        .map(|i| {
            let a = f64::from(i) * std::f64::consts::TAU / 12.0;
            [a.cos(), a.sin(), 0.0]
        })
        .collect();
    assert!(convex_hull(&ring).is_none(), "a planar ring is coplanar");
    let line: Vec<Vec3> = (0..5).map(|i| [f64::from(i), 0.0, 0.0]).collect();
    assert!(convex_hull(&line).is_none(), "collinear points");
}

/// A measurement grid as SOFA sets lay them out: rings of 72 directions every 10 degrees
/// of elevation from -40 to 80, aligned in azimuth so each four neighbours are cocircular,
/// and the pole. Exact cocircular quads must still triangulate closed, with every
/// direction a vertex.
#[test]
fn a_ring_grid_with_cocircular_quads_is_a_closed_convex_triangulation() {
    let mut points: Vec<Vec3> = Vec::new();
    for ring in -4..=8 {
        let el = f64::from(ring * 10).to_radians();
        for k in 0..72 {
            let az = f64::from(k * 5).to_radians();
            points.push([el.cos() * az.cos(), el.cos() * az.sin(), el.sin()]);
        }
    }
    points.push([0.0, 0.0, 1.0]);
    let faces = convex_hull(&points).expect("the grid has volume");
    assert_convex_triangulation(&points, &faces);
}
