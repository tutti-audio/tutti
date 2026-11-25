//! A synthetic SOFA writer and a reader of the sphere's byte format.

use std::path::PathBuf;

use hdf5_pure::{AttrValue, FileBuilder};
use tutti_dsp::HrirSphere;

pub(crate) const TAPS: usize = 4;

/// The six directions of the octahedron as spherical SOFA rows (azimuth,
/// elevation, distance): front, left, back, right, up, down.
pub(crate) const OCTAHEDRON: [[f64; 3]; 6] = [
    [0.0, 0.0, 1.0],
    [90.0, 0.0, 1.0],
    [180.0, 0.0, 1.0],
    [270.0, 0.0, 1.0],
    [0.0, 90.0, 1.0],
    [0.0, -90.0, 1.0],
];

/// What a SimpleFreeFieldHRIR file holds, each part replaceable so a test
/// breaks one.
pub(crate) struct Sofa {
    pub(crate) convention: Option<&'static str>,
    pub(crate) ir_shape: Vec<u64>,
    pub(crate) ir: Vec<f64>,
    pub(crate) rate: Vec<f64>,
    pub(crate) rate_units: Option<&'static str>,
    pub(crate) delay: Option<(Vec<u64>, Vec<f64>)>,
    pub(crate) position_shape: Vec<u64>,
    pub(crate) positions: Vec<f64>,
    pub(crate) position_type: Option<&'static str>,
    pub(crate) position_units: Option<&'static str>,
}

/// Measurement `m`, receiver `r`, tap `n`: a value `f32` holds exactly and
/// that tells the three apart.
pub(crate) fn tap(m: usize, r: usize, n: usize) -> f64 {
    (100 * m + 10 * r + n) as f64 + 0.25
}

impl Sofa {
    /// The octahedron, spherical, 48 kHz, no delay.
    pub(crate) fn octahedron() -> Self {
        let m = OCTAHEDRON.len();
        let ir = (0..m * 2 * TAPS)
            .map(|i| tap(i / (2 * TAPS), i / TAPS % 2, i % TAPS))
            .collect();
        Self {
            convention: Some("SimpleFreeFieldHRIR"),
            ir_shape: vec![m as u64, 2, TAPS as u64],
            ir,
            rate: vec![48_000.0],
            rate_units: Some("hertz"),
            delay: Some((vec![1, 2], vec![0.0, 0.0])),
            position_shape: vec![m as u64, 3],
            positions: OCTAHEDRON.iter().flatten().copied().collect(),
            position_type: Some("spherical"),
            position_units: Some("degree, degree, metre"),
        }
    }

    /// Writes the file under the test target's scratch directory and returns
    /// its path.
    pub(crate) fn write(&self, name: &str) -> PathBuf {
        let mut b = FileBuilder::new();
        if let Some(c) = self.convention {
            b.set_attr("SOFAConventions", AttrValue::String(c.into()));
        }
        b.create_dataset("Data.IR")
            .with_f64_data(&self.ir)
            .with_shape(&self.ir_shape);
        let rate = b
            .create_dataset("Data.SamplingRate")
            .with_f64_data(&self.rate);
        if let Some(u) = self.rate_units {
            rate.set_attr("Units", AttrValue::String(u.into()));
        }
        if let Some((shape, values)) = &self.delay {
            b.create_dataset("Data.Delay")
                .with_f64_data(values)
                .with_shape(shape);
        }
        let position = b
            .create_dataset("SourcePosition")
            .with_f64_data(&self.positions)
            .with_shape(&self.position_shape);
        if let Some(t) = self.position_type {
            position.set_attr("Type", AttrValue::String(t.into()));
        }
        if let Some(u) = self.position_units {
            position.set_attr("Units", AttrValue::String(u.into()));
        }
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sofa");
        std::fs::create_dir_all(&dir).expect("the scratch directory is writable");
        let path = dir.join(format!("{name}.sofa"));
        std::fs::write(&path, b.finish().expect("the file serializes")).expect("written");
        path
    }
}

/// A sphere read back from its bytes by the format's own layout: `"HRIR"`,
/// little-endian `u32` rate, response length, vertex count and index count,
/// the face indices, then per vertex a position and the left and right
/// responses.
pub(crate) struct Written {
    pub(crate) rate: u32,
    pub(crate) len: usize,
    pub(crate) faces: Vec<[u32; 3]>,
    pub(crate) positions: Vec<[f32; 3]>,
    pub(crate) left: Vec<Vec<f32>>,
    pub(crate) right: Vec<Vec<f32>>,
}

pub(crate) fn read_back(sphere: &HrirSphere) -> Written {
    let bytes = sphere.to_bytes();
    assert_eq!(&bytes[..4], b"HRIR");
    let words: Vec<[u8; 4]> = bytes[4..].as_chunks::<4>().0.to_vec();
    let u32_at = |i: usize| u32::from_le_bytes(words[i]);
    let f32_at = |i: usize| f32::from_le_bytes(words[i]);
    let (rate, len) = (u32_at(0), u32_at(1) as usize);
    let (vertices, indices) = (u32_at(2) as usize, u32_at(3) as usize);
    let faces = (0..indices / 3)
        .map(|f| [4 + 3 * f, 5 + 3 * f, 6 + 3 * f].map(u32_at))
        .collect();
    let mut at = 4 + indices;
    let mut out = Written {
        rate,
        len,
        faces,
        positions: Vec::new(),
        left: Vec::new(),
        right: Vec::new(),
    };
    for _ in 0..vertices {
        out.positions
            .push([f32_at(at), f32_at(at + 1), f32_at(at + 2)]);
        at += 3;
        out.left.push((0..len).map(|i| f32_at(at + i)).collect());
        out.right
            .push((0..len).map(|i| f32_at(at + len + i)).collect());
        at += 2 * len;
    }
    assert_eq!(
        at,
        words.len(),
        "the bytes hold exactly the header's sphere"
    );
    out
}

/// `a` and `b` agree within `1e-6` per coordinate.
pub(crate) fn same_point(a: [f32; 3], b: [f32; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6)
}
