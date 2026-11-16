//! Where a SOFA source position lands in tutti's frame: +x right, +y up,
//! +z behind, so front is -z and left is -x. SOFA's own frame has azimuth
//! counter-clockwise from the front, x front, y left, z up.

use tutti_hrir_convert::convert;

use crate::support::{OCTAHEDRON, Sofa, read_back, same_point};

const FRONT: [f32; 3] = [0.0, 0.0, -1.0];
const LEFT: [f32; 3] = [-1.0, 0.0, 0.0];
const BACK: [f32; 3] = [0.0, 0.0, 1.0];
const RIGHT: [f32; 3] = [1.0, 0.0, 0.0];
const UP: [f32; 3] = [0.0, 1.0, 0.0];
const DOWN: [f32; 3] = [0.0, -1.0, 0.0];

#[test]
fn spherical_positions_land_on_tuttis_axes() {
    let sphere = convert(&Sofa::octahedron().write("spherical_axes")).expect("converts");
    let got = read_back(&sphere).positions;
    for (what, (have, want)) in [
        "front",
        "left (azimuth 90)",
        "back",
        "right",
        "up (elevation 90)",
        "down",
    ]
    .iter()
    .zip(got.iter().zip([FRONT, LEFT, BACK, RIGHT, UP, DOWN]))
    {
        assert!(
            same_point(*have, want),
            "{what} is {have:?}, expected {want:?}"
        );
    }
}

#[test]
fn cartesian_positions_land_on_tuttis_axes() {
    let mut sofa = Sofa::octahedron();
    sofa.position_type = Some("cartesian");
    sofa.position_units = Some("metre, metre, metre");
    // x front, y left, z up; distances differ from 1 and do not matter.
    sofa.positions = [
        [2.0, 0.0, 0.0],
        [0.0, 2.0, 0.0],
        [-2.0, 0.0, 0.0],
        [0.0, -2.0, 0.0],
        [0.0, 0.0, 2.0],
        [0.0, 0.0, -2.0],
    ]
    .into_iter()
    .flatten()
    .collect();
    let sphere = convert(&sofa.write("cartesian_axes")).expect("converts");
    let got = read_back(&sphere).positions;
    for (what, (have, want)) in ["front", "left", "back", "right", "up", "down"]
        .iter()
        .zip(got.iter().zip([FRONT, LEFT, BACK, RIGHT, UP, DOWN]))
    {
        assert!(
            same_point(*have, want),
            "{what} is {have:?}, expected {want:?}"
        );
    }
}

/// The point at azimuth 30, elevation 20, as spherical and as cartesian
/// (x front, y left, z up), is one vertex.
#[test]
fn spherical_and_cartesian_give_the_same_vertex() {
    let (az, el) = (30.0f64.to_radians(), 20.0f64.to_radians());
    let mut spherical = Sofa::octahedron();
    spherical.positions[..3].copy_from_slice(&[30.0, 20.0, 1.0]);
    let mut cartesian = Sofa::octahedron();
    cartesian.position_type = Some("cartesian");
    cartesian.position_units = Some("metre, metre, metre");
    cartesian.positions = OCTAHEDRON
        .iter()
        .flat_map(|&[a, e, d]| {
            let (a, e) = (a.to_radians(), e.to_radians());
            [d * e.cos() * a.cos(), d * e.cos() * a.sin(), d * e.sin()]
        })
        .collect();
    cartesian.positions[..3].copy_from_slice(&[
        1.5 * el.cos() * az.cos(),
        1.5 * el.cos() * az.sin(),
        1.5 * el.sin(),
    ]);
    let a = read_back(&convert(&spherical.write("point_spherical")).expect("converts"));
    let b = read_back(&convert(&cartesian.write("point_cartesian")).expect("converts"));
    for (v, (p, q)) in a.positions.iter().zip(&b.positions).enumerate() {
        assert!(same_point(*p, *q), "vertex {v}: {p:?} against {q:?}");
    }
    // The point itself, from the formula in tutti's frame.
    let want = [
        (-az.sin() * el.cos()) as f32,
        el.sin() as f32,
        (-az.cos() * el.cos()) as f32,
    ];
    assert!(
        same_point(a.positions[0], want),
        "{:?} against {want:?}",
        a.positions[0]
    );
}

/// The octahedron's hull: eight faces over its six vertices.
#[test]
fn the_directions_are_triangulated_over_the_sphere() {
    let written = read_back(&convert(&Sofa::octahedron().write("faces")).expect("converts"));
    assert_eq!(written.faces.len(), 8);
    assert_eq!(written.positions.len(), 6);
}
