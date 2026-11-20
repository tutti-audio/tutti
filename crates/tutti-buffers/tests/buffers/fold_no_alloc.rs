//! Applying a `Fold` allocates nothing: every applier (frame, interleaved,
//! planar, planar to interleaved and one planar channel), over every shape of
//! row (copy, zero, sum, summed group, mono mix), 1 000 blocks under
//! `assert_no_alloc`. Building the fold allocates and happens outside the
//! gate.

use assert_no_alloc::assert_no_alloc;
use tutti_buffers::{
    AmbisonicFormat, AmbisonicNormalization, AmbisonicOrder, AmbisonicOrdering, ChannelLayout,
    ChannelTopology, Speaker,
};
use tutti_buffers::{Fold, Interleaved, InterleavedMut, MonoMut, Planes, PlanesMut};

fn smpte(n: u16) -> ChannelTopology {
    ChannelTopology::default_for(ChannelLayout::from(n))
}

/// 12 -> 2 is the summed group, 6 -> 1 the mono mix, the permutation and
/// 2 -> 6 the copy and zero rows, ambisonic conversions the rescaled, reordered,
/// dropped and zero-filled rows, 5.1 -> 2 plain sums. 200 frames is past the
/// planar group's 64-frame chunk.
#[test]
fn applying_a_fold_is_allocation_free() {
    use Speaker::*;
    let dts = ChannelTopology::new([
        FrontLeft,
        FrontCenter,
        FrontRight,
        SideLeft,
        SideRight,
        LowFrequency,
    ])
    .unwrap();
    let pairs = [
        (smpte(12), smpte(2)),
        (smpte(6), smpte(1)),
        (smpte(6), dts),
        (smpte(2), smpte(6)),
        (smpte(6), smpte(2)),
        (
            AmbisonicFormat::ambix(AmbisonicOrder::of(3)).into(),
            AmbisonicFormat::new(
                AmbisonicOrder::of(2),
                AmbisonicOrdering::FuMa,
                AmbisonicNormalization::N3d,
            )
            .into(),
        ),
        (
            AmbisonicFormat::fuma(AmbisonicOrder::of(1)).into(),
            AmbisonicFormat::ambix(AmbisonicOrder::of(3)).into(),
        ),
    ];
    const FRAMES: usize = 200;
    for (src, dst) in &pairs {
        let fold = Fold::between(src, dst).unwrap();
        let (sw, dw) = (
            usize::from(src.width().count()),
            usize::from(dst.width().count()),
        );
        let data: Vec<f32> = (0..sw * FRAMES).map(|i| (i as f32 * 0.37).sin()).collect();
        let planes: Vec<Vec<f32>> = (0..sw)
            .map(|c| data.chunks_exact(sw).map(|f| f[c]).collect())
            .collect();
        let mut block = vec![0.0f32; dw * FRAMES];
        let mut flat = vec![0.0f32; dw * FRAMES];
        let mut out = vec![0.0f32; FRAMES];
        let mut frame = vec![0.0f32; dw];
        let mut folded = vec![vec![0.0f32; FRAMES]; dw];
        // Warm up: first-touch faults are not the fold's.
        fold.frame(&data[..sw], &mut frame);
        assert_no_alloc(|| {
            for _ in 0..1_000 {
                fold.frame(&data[..sw], &mut frame);
                let planar = Planes::from_vecs(&planes).unwrap();
                for c in 0..dw {
                    fold.fold_planar_channel(planar, c, MonoMut::new(&mut out))
                        .unwrap();
                }
                fold.fold_planar_to_interleaved(
                    planar,
                    InterleavedMut::new(&mut block, dst.width()).unwrap(),
                )
                .unwrap();
                fold.fold(
                    Interleaved::new(&data, src.width()).unwrap(),
                    InterleavedMut::new(&mut flat, dst.width()).unwrap(),
                )
                .unwrap();
                fold.fold_planar(planar, PlanesMut::from_vecs(&mut folded).unwrap())
                    .unwrap();
            }
        });
        assert!(
            block
                .iter()
                .chain(&out)
                .chain(&flat)
                .any(|s| s.abs() > 1e-6)
                && folded.iter().flatten().any(|s| s.abs() > 1e-6),
            "the fold rendered silence; the gate would walk no DSP"
        );
    }
}
