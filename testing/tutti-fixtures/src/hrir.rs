//! An HRIR sphere (the `HRIR` byte format) built in memory, for a test that needs a sphere
//! that parses without a dataset file, and the little-endian writers it shares with a
//! test's own sphere.

/// Appends `v` little-endian.
pub fn push_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

/// Appends `v` little-endian.
pub fn push_f32(b: &mut Vec<u8>, v: f32) {
    b.extend_from_slice(&v.to_le_bytes());
}

/// A minimal, format-valid HRIR sphere: a tetrahedron of 4 vertices with
/// unit-delta HRIRs (left an identity delta, right scaled per vertex), at
/// the rate given, so a test can pick the rate nothing is resampled at.
pub fn synthetic_hrir_sphere(sample_rate: u32, ir_len: usize) -> Vec<u8> {
    let verts: [[f32; 3]; 4] = [
        [1.0, 1.0, 1.0],
        [-1.0, -1.0, 1.0],
        [-1.0, 1.0, -1.0],
        [1.0, -1.0, -1.0],
    ];
    let faces: [[u32; 3]; 4] = [[0, 1, 2], [0, 3, 1], [0, 2, 3], [1, 3, 2]];
    let mut b = Vec::new();
    b.extend_from_slice(b"HRIR");
    push_u32(&mut b, sample_rate);
    push_u32(&mut b, ir_len as u32);
    push_u32(&mut b, verts.len() as u32);
    push_u32(&mut b, (faces.len() * 3) as u32);
    for f in faces {
        for idx in f {
            push_u32(&mut b, idx);
        }
    }
    for (vi, v) in verts.iter().enumerate() {
        for x in v {
            push_f32(&mut b, *x);
        }
        for i in 0..ir_len {
            push_f32(&mut b, if i == 0 { 1.0 } else { 0.0 });
        }
        for i in 0..ir_len {
            push_f32(&mut b, if i == 0 { 0.5 + 0.1 * vi as f32 } else { 0.0 });
        }
    }
    b
}
