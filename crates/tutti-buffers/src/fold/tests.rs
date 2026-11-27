use super::*;
use crate::Float;
use crate::{AmbisonicFormat, AmbisonicOrder, AmbisonicOrdering, LayoutError};
use crate::{BufferError, InterleavedMut};
use Speaker::*;

fn smpte(n: u16) -> ChannelTopology {
    ChannelTopology::default_for(ChannelLayout::from(n)).unwrap()
}

fn fold(s: &ChannelTopology, d: &ChannelTopology) -> Fold {
    Fold::<f32>::between(s, d).expect("a fold")
}

/// The placement of two bare widths: the engine's order for each where both
/// have one, channels by index where either has none.
fn widths(src: u16, dst: u16) -> Result<Fold, FoldError> {
    let (s, d) = (ChannelLayout::from(src), ChannelLayout::from(dst));
    let (from, to) = (
        ChannelTopology::default_for(s).unwrap(),
        ChannelTopology::default_for(d).unwrap(),
    );
    if from.is_unassigned() || to.is_unassigned() {
        Fold::<f32>::by_index(s, d)
    } else {
        Fold::<f32>::between(&from, &to)
    }
}

/// Destination channel `c` of `planes` folded into `out`, over every frame.
fn planar_channel(f: &Fold, planes: &[Vec<f32>], c: usize, out: &mut [f32]) {
    f.fold_planar_channel(Planes::from_vecs(planes).unwrap(), c, MonoMut::new(out))
        .unwrap();
}

/// Values a sum must carry bit for bit: NaN with a payload, both zeros, both
/// infinities, subnormals and the largest finite value.
const SPECIALS: [f32; 9] = [
    f32::from_bits(0x7fc0_1234),
    -0.0,
    f32::INFINITY,
    f32::from_bits(1),
    -f32::INFINITY,
    1.0e-40,
    f32::MAX,
    0.0,
    -1.0e-42,
];

/// A deterministic spread of finite samples, signs and magnitudes.
fn noise(n: usize, seed: u32) -> Vec<f32> {
    tutti_fixtures::noise::<f32>(seed, n)
        .iter()
        .map(|v| 1.5 * v)
        .collect()
}

/// The response of output `o` to a unit impulse on each source channel.
fn response(f: &Fold, o: usize) -> Vec<f32> {
    let (s, d) = (usize::from(f.src), f.rows.len());
    (0..s)
        .map(|k| {
            let mut frame = vec![0.0; s];
            frame[k] = 1.0;
            let mut out = vec![f32::NAN; d];
            f.frame(&frame, &mut out);
            out[o]
        })
        .collect()
}

fn near(got: &[f32], want: &[f32], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: width");
    for (c, (g, w)) in got.iter().zip(want).enumerate() {
        assert!((g - w).abs() < 1e-6, "{what}: channel {c} is {g}, want {w}");
    }
}

const M: f32 = f32::MINUS_3_DB;

/// The hand-written coefficient table of the ITU-R BS.775 / Dolby stereo
/// downmix, per source width (LFE column always 0), for the left output; the
/// right is the mirror. `quad` is `FL FR BL BR`.
#[test]
fn the_stereo_fold_is_the_itu_table() {
    let cases: [(ChannelTopology, [f32; 12], [f32; 12]); 4] = [
        (
            crate::Bed::Quad.topology(),
            [1.0, 0.0, M, 0.0, 0., 0., 0., 0., 0., 0., 0., 0.],
            [0.0, 1.0, 0.0, M, 0., 0., 0., 0., 0., 0., 0., 0.],
        ),
        (
            smpte(6),
            [1.0, 0.0, M, 0.0, M, 0.0, 0., 0., 0., 0., 0., 0.],
            [0.0, 1.0, M, 0.0, 0.0, M, 0., 0., 0., 0., 0., 0.],
        ),
        (
            smpte(8),
            [1.0, 0.0, M, 0.0, M, 0.0, M, 0.0, 0., 0., 0., 0.],
            [0.0, 1.0, M, 0.0, 0.0, M, 0.0, M, 0., 0., 0., 0.],
        ),
        (
            smpte(12),
            [1.0, 0.0, M, 0.0, M, 0.0, M, 0.0, M, 0.0, M, 0.0],
            [0.0, 1.0, M, 0.0, 0.0, M, 0.0, M, 0.0, M, 0.0, M],
        ),
    ];
    for (src, left, right) in &cases {
        let w = usize::from(src.width().count());
        let f = fold(src, &smpte(2));
        near(&response(&f, 0), &left[..w], &format!("{w} -> L"));
        near(&response(&f, 1), &right[..w], &format!("{w} -> R"));
    }
}

/// Mono: one channel copies, a stereo pair averages, anything wider is the
/// stereo fold times −3 dB (the centre then lands at `2·MINUS_3_DB² = 1`).
#[test]
fn the_mono_fold_is_the_itu_table() {
    let mono = smpte(1);
    near(&response(&fold(&smpte(1), &mono), 0), &[1.0], "1 -> 1");
    near(&response(&fold(&smpte(2), &mono), 0), &[0.5, 0.5], "2 -> 1");
    near(
        &response(&fold(&smpte(6), &mono), 0),
        &[M, M, 1.0, 0.0, M * M, M * M],
        "6 -> 1",
    );
    near(
        &response(&fold(&crate::Bed::Quad.topology(), &mono), 0),
        &[M, M, M * M, M * M],
        "quad -> 1",
    );
    // Not continuous across widths, as before.
    let mut out = [0.0];
    fold(&smpte(2), &mono).frame(&[0.1, 0.2], &mut out);
    assert!((out[0] - 0.15).abs() < 1e-7);
    let three = ChannelTopology::new([FrontLeft, FrontRight, FrontCenter]).unwrap();
    Fold::<f32>::between(&three, &mono)
        .unwrap()
        .frame(&[0.1, 0.2, 0.0], &mut out);
    assert!((out[0] - 0.212_132).abs() < 1e-6);
}

/// The ITU-R BS.775 / Dolby formulas of a width, written out: side `side` (0
/// left, 1 right) of the stereo fold of a frame of `width` channels. A width
/// with no matrix passes the front pair through. This is the closed form the
/// width folds computed, in their order of operations.
fn itu_side(width: usize, side: usize, f: &[f32]) -> f32 {
    let m = f32::MINUS_3_DB;
    let get = |c: usize| f[c];
    match width {
        1 => get(0),
        4 => get(side) + m * get(2 + side),
        6 => get(side) + m * get(2) + m * get(4 + side),
        8 => get(side) + m * get(2) + m * get(4 + side) + m * get(6 + side),
        12 => {
            get(side)
                + m * (get(2) + get(4 + side) + get(6 + side) + get(8 + side) + get(10 + side))
        }
        _ => get(side),
    }
}

/// One frame of width `src` onto `dst` by the closed forms: mono is the
/// average of two channels and the stereo fold times -3 dB from three, stereo
/// is the fold, anything wider copies and zero-fills.
fn width_oracle(f: &[f32], dst: usize) -> Vec<f32> {
    let (lo, ro) = (itu_side(f.len(), 0, f), itu_side(f.len(), 1, f));
    match dst {
        1 => vec![match f.len() {
            1 => f[0],
            2 => (f[0] + f[1]) * 0.5,
            _ => (lo + ro) * f32::MINUS_3_DB,
        }],
        2 => vec![lo, ro],
        _ => (0..dst).map(|i| f.get(i).copied().unwrap_or(0.0)).collect(),
    }
}

/// The pairs whose output differs from what the width folds wrote: a mono
/// feeds the centre or the front pair, a quad's rears are no longer the centre
/// and LFE, and a wider surround is placed by position rather than by its first
/// channels.
const PLACED_BY_POSITION: [(u16, u16); 13] = [
    (1, 4),
    (1, 6),
    (1, 8),
    (1, 12),
    (4, 6),
    (4, 8),
    (4, 12),
    (6, 4),
    (8, 4),
    (8, 6),
    (12, 4),
    (12, 6),
    (12, 8),
];

/// Widths the engine has an order for.
const ORDERED: [u16; 6] = [1, 2, 4, 6, 8, 12];

/// A narrowing with an unordered end has no place for its extra channels, and
/// is refused rather than truncated.
fn refused(sw: u16, dw: u16) -> bool {
    sw > dw && !(ORDERED.contains(&sw) && ORDERED.contains(&dw))
}

/// Invariant: for every pair of widths 1 to 14 outside [`PLACED_BY_POSITION`]
/// (and not [`refused`]),
/// the placement of two widths writes the bits of the closed-form ITU formulas
/// ([`width_oracle`]), on frames of every sign and magnitude and in the special
/// values, per frame and planar. Widths with no order (3, 5, 7, 9 to 11, 13, 14)
/// are placed by index and only widen (copy, zero-fill); a narrowing with one is
/// refused with `Unplaced` (its channels would be dropped).
#[test]
fn widths_write_the_itu_formulas_to_the_bit() {
    let mut pairs = 0;
    let mut refusals = 0;
    for sw in 1u16..=14 {
        for dw in 1u16..=14 {
            if PLACED_BY_POSITION.contains(&(sw, dw)) {
                continue;
            }
            if refused(sw, dw) {
                assert!(
                    matches!(widths(sw, dw), Err(FoldError::Unplaced { .. })),
                    "{sw} -> {dw} must be refused"
                );
                refusals += 1;
                continue;
            }
            let f = widths(sw, dw).unwrap();
            let (s, d) = (usize::from(sw), usize::from(dw));
            // Past two planar chunks, with specials among the noise.
            let frames = 150;
            let mut data = noise(s * frames, 7 + u32::from(sw) * 31 + u32::from(dw));
            for (k, x) in data.iter_mut().enumerate().filter(|(k, _)| k % 5 == 0) {
                *x = SPECIALS[k / 5 % SPECIALS.len()];
            }
            let want: Vec<f32> = data
                .chunks_exact(s)
                .flat_map(|a| width_oracle(a, d))
                .collect();
            let mut got = vec![0.0f32; d * frames];
            for (a, b) in data.chunks_exact(s).zip(got.chunks_exact_mut(d)) {
                f.frame(a, b);
            }
            let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&got), bits(&want), "{sw} -> {dw} frame");

            let planes: Vec<Vec<f32>> = (0..s)
                .map(|c| data.chunks_exact(s).map(|fr| fr[c]).collect())
                .collect();
            for c in 0..d {
                let mut a = vec![f32::NAN; frames];
                planar_channel(&f, &planes, c, &mut a);
                let col: Vec<f32> = want.chunks_exact(d).map(|fr| fr[c]).collect();
                assert_eq!(bits(&a), bits(&col), "{sw} -> {dw} planar vs frame {c}");
            }
            pairs += 1;
        }
    }
    assert_eq!(pairs + refusals + PLACED_BY_POSITION.len(), 14 * 14);
    assert!(
        refusals > 0 && pairs > 100,
        "{refusals} refused, {pairs} placed"
    );
}

/// Every pair in [`PLACED_BY_POSITION`] really differs from the closed form
/// (the list is the set that changed, no more and no fewer), and a pair outside
/// it with a matrix does not: a stale entry would otherwise hide a regression.
#[test]
fn the_placed_by_position_pairs_are_the_ones_that_differ() {
    let probe = |s: usize| -> Vec<f32> { (0..s).map(|c| 0.25 + c as f32 * 0.5).collect() };
    for sw in 1u16..=14 {
        for dw in 1u16..=14 {
            if refused(sw, dw) {
                continue;
            }
            let f = widths(sw, dw).unwrap();
            let src = probe(usize::from(sw));
            let mut got = vec![0.0f32; usize::from(dw)];
            f.frame(&src, &mut got);
            let same = got == width_oracle(&src, usize::from(dw));
            assert_eq!(
                same,
                !PLACED_BY_POSITION.contains(&(sw, dw)),
                "{sw} -> {dw}: {got:?}"
            );
        }
    }
}

/// A width with no order is placed by index, widening only: a narrowing is
/// refused, not truncated.
#[test]
fn by_index_only_widens() {
    let w = ChannelLayout::from;
    let run = |s: u16, d: u16, src: &[f32]| {
        let mut out = vec![9.0f32; usize::from(d)];
        Fold::<f32>::by_index(w(s), w(d))
            .unwrap()
            .frame(src, &mut out);
        out
    };
    assert_eq!(run(3, 5, &[1.0, 2.0, 3.0]), [1.0, 2.0, 3.0, 0.0, 0.0]);
    assert_eq!(run(3, 3, &[1.0, 2.0, 3.0]), [1.0, 2.0, 3.0]);
    assert_eq!(run(1, 3, &[0.5]), [0.5, 0.0, 0.0]);
    for (s, d) in [(5u16, 3u16), (7, 2), (3, 1), (2, 1)] {
        assert_eq!(
            Fold::<f32>::by_index(w(s), w(d)),
            Err(FoldError::Unplaced {
                channel: usize::from(d),
                speaker: Speaker::Unassigned
            }),
            "{s} -> {d}"
        );
    }
    assert_eq!(
        Fold::<f32>::by_index(ChannelLayout::EMPTY, w(2)),
        Err(FoldError::Buffer(BufferError::ZeroWidth))
    );
    assert_eq!(
        Fold::<f32>::by_index(w(2), ChannelLayout::EMPTY),
        Err(FoldError::Buffer(BufferError::ZeroWidth))
    );
}

/// The same speakers in another order is a permutation: every output is a
/// plain copy (gain exactly 1.0), bit for bit, including negative zero, NaN
/// payloads, infinities and subnormals.
#[test]
fn the_same_speakers_in_another_order_is_a_bit_exact_permutation() {
    let film = smpte(6);
    let dts = ChannelTopology::new([
        FrontLeft,
        FrontCenter,
        FrontRight,
        SideLeft,
        SideRight,
        LowFrequency,
    ])
    .unwrap();
    let f = fold(&film, &dts);
    assert!(f.rows.iter().all(|r| matches!(r, Row::One(Expr::Copy(_)))));
    let special = [
        -0.0f32,
        f32::from_bits(0x7fc0_1234),
        f32::INFINITY,
        f32::from_bits(1),
        1.0e-30,
        -3.5,
    ];
    let mut out = [0.0f32; 6];
    f.frame(&special, &mut out);
    let want = [
        special[0], special[2], special[1], special[4], special[5], special[3],
    ];
    assert_eq!(out.map(f32::to_bits), want.map(f32::to_bits));
    // And back again.
    let back = fold(&dts, &film);
    let mut round = [0.0f32; 6];
    back.frame(&out, &mut round);
    assert_eq!(round.map(f32::to_bits), special.map(f32::to_bits));
    // Every response is a one-hot with gain exactly 1.
    for o in 0..6 {
        let r = response(&f, o);
        assert_eq!(r.iter().filter(|&&g| g == 1.0).count(), 1);
        assert_eq!(r.iter().filter(|&&g| g == 0.0).count(), 5);
    }
}

/// A pair with no matrix before: 5.1 to quad keeps the centre out of the
/// rears and moves the surrounds across at full level.
#[test]
fn five_one_to_quad_places_by_position() {
    let f = fold(&smpte(6), &crate::Bed::Quad.topology());
    //                 FL   FR   C    LFE  SL   SR
    near(&response(&f, 0), &[1.0, 0.0, M, 0.0, 0.0, 0.0], "FL");
    near(&response(&f, 1), &[0.0, 1.0, M, 0.0, 0.0, 0.0], "FR");
    near(&response(&f, 2), &[0.0, 0.0, 0.0, 0.0, 1.0, 0.0], "BL");
    near(&response(&f, 3), &[0.0, 0.0, 0.0, 0.0, 0.0, 1.0], "BR");
}

/// 7.1 to 5.1 sums the rears into the sides at −3 dB; 5.1 to 7.1 zero-fills the
/// rears; 7.1.4 to 7.1 folds the heights into the front pair at −3 dB.
#[test]
fn narrowing_below_surround_is_by_position() {
    let f = fold(&smpte(8), &smpte(6));
    near(&response(&f, 4), &[0., 0., 0., 0., 1.0, 0.0, M, 0.0], "SL");
    near(&response(&f, 0), &[1., 0., 0., 0., 0., 0., 0., 0.], "FL");
    let up = fold(&smpte(6), &smpte(8));
    near(&response(&up, 6), &[0.0; 6], "BL is silent");
    near(&response(&up, 7), &[0.0; 6], "BR is silent");
    near(&response(&up, 5), &[0., 0., 0., 0., 0., 1.0], "SR");
    let heights = fold(&smpte(12), &smpte(8));
    let l = response(&heights, 0);
    near(
        &l,
        &[1., 0., 0., 0., 0., 0., 0., 0., M, 0., 0., 0.],
        "FL with heights: only the top-front joins it",
    );
    // The top-back pair joins the back pair of its side.
    near(
        &response(&heights, 6),
        &[0., 0., 0., 0., 0., 0., 1., 0., 0., 0., M, 0.],
        "BL with the top-back",
    );
}

/// The formats disagree on side versus back for four speakers (the engine's
/// quad is `FL FR BL BR`, VST3's `k40Music` and AU's quadraphonic decode to
/// `FL FR SL SR`): a fold between them is a pure reposition at gain 1.
#[test]
fn quad_and_side_surround_quad_are_the_same_four_feeds() {
    let side_quad = ChannelTopology::new([FrontLeft, FrontRight, SideLeft, SideRight]).unwrap();
    let f = fold(&crate::Bed::Quad.topology(), &side_quad);
    for o in 0..4 {
        let r = response(&f, o);
        assert_eq!(r.iter().filter(|&&g| g == 1.0).count(), 1, "{r:?}");
        assert_eq!(r.iter().filter(|&&g| g == 0.0).count(), 3, "{r:?}");
    }
    assert_eq!(response(&f, 2), [0.0, 0.0, 1.0, 0.0]);
}

/// A mono source feeds the centre of a destination that has one, else the
/// front pair at gain 1 (never at −3 dB, never in an LFE or a surround).
#[test]
fn a_lone_centre_feeds_the_centre() {
    let mono = smpte(1);
    assert_eq!(response(&fold(&mono, &smpte(2)), 0), [1.0]);
    assert_eq!(response(&fold(&mono, &smpte(2)), 1), [1.0]);
    let six = fold(&mono, &smpte(6));
    for (o, want) in [0.0, 0.0, 1.0, 0.0, 0.0, 0.0].iter().enumerate() {
        assert_eq!(response(&six, o), [*want], "5.1 channel {o}");
    }
    let no_front = ChannelTopology::new([FrontLeft, LowFrequency]).unwrap();
    assert_eq!(
        Fold::<f32>::between(&mono, &no_front),
        Err(FoldError::Unplaced {
            channel: 0,
            speaker: FrontCenter
        })
    );
}

/// Unassigned channels map one to one and never meet a matrix: they relabel at
/// equal width, and `between` refuses any other width on either side.
#[test]
fn unassigned_channels_map_one_to_one() {
    let d = |n: u16| ChannelTopology::unassigned(ChannelLayout::from(n)).unwrap();
    let f = fold(&d(6), &d(6));
    for o in 0..6 {
        let mut want = [0.0; 6];
        want[o] = 1.0;
        assert_eq!(response(&f, o), want);
    }
    // Any other width is refused: a channel that names no speaker says nothing
    // about where it goes. By index is `Fold::by_index`, asked for by name.
    let refused = |s: ChannelTopology, t: ChannelTopology| {
        assert_eq!(
            Fold::<f32>::between(&s, &t),
            Err(FoldError::Unassigned {
                src: s.width(),
                dst: t.width()
            })
        );
    };
    refused(d(2), d(4));
    refused(d(4), d(2));
    refused(d(6), smpte(2));
    refused(smpte(2), d(6));
    refused(smpte(6), d(16));
    refused(smpte(6), d(2));
    let up = Fold::<f32>::by_index(ChannelLayout::STEREO, ChannelLayout::from(6u16)).unwrap();
    assert_eq!(response(&up, 1), [0.0, 1.0]);
    assert_eq!(response(&up, 5), [0.0, 0.0]);
    // Equal width relabels by index: two inputs are a stereo pair.
    let pair = fold(&d(2), &smpte(2));
    assert_eq!(response(&pair, 0), [1.0, 0.0]);
    assert_eq!(response(&pair, 1), [0.0, 1.0]);
}

/// Every refusal names what failed.
#[test]
fn what_cannot_be_placed_is_an_error() {
    let empty = ChannelTopology::new([]).unwrap();
    assert_eq!(
        Fold::<f32>::between(&empty, &smpte(2)),
        Err(FoldError::Buffer(BufferError::ZeroWidth))
    );
    assert_eq!(
        Fold::<f32>::between(&smpte(2), &empty),
        Err(FoldError::Buffer(BufferError::ZeroWidth))
    );
    let odd = ChannelTopology::new([FrontLeft, Unknown(9)]).unwrap();
    assert_eq!(
        Fold::<f32>::between(&odd, &smpte(2)),
        Err(FoldError::Unnamed {
            end: FoldEnd::Source,
            channel: 1
        })
    );
    assert_eq!(
        Fold::<f32>::between(&smpte(2), &odd),
        Err(FoldError::Unnamed {
            end: FoldEnd::Destination,
            channel: 1
        })
    );
    // Identical topologies are an identity even when a channel is unknown.
    assert!(Fold::<f32>::between(&odd, &odd).is_ok());
    let centre_lfe = ChannelTopology::new([FrontCenter, LowFrequency]).unwrap();
    assert_eq!(
        Fold::<f32>::between(&smpte(2), &centre_lfe),
        Err(FoldError::Unplaced {
            channel: 0,
            speaker: FrontLeft
        })
    );
}

/// A bare width goes through [`ChannelTopology::default_for`]: a width with an order
/// folds by speaker, and one with none is unassigned, which `between` refuses
/// against another width rather than guess.
#[test]
fn a_bare_width_folds_through_default_for() {
    let default = |n: u16| ChannelTopology::default_for(ChannelLayout::from(n)).unwrap();
    assert_eq!(
        Fold::<f32>::between(&default(6), &default(2)).unwrap(),
        fold(&smpte(6), &smpte(2))
    );
    // A bare four is quad, whose rears are the stereo fold's surrounds.
    let mut out = [0.0f32; 2];
    Fold::<f32>::between(&default(4), &default(2))
        .unwrap()
        .frame(&[1.0, 2.0, 3.0, 4.0], &mut out);
    assert_eq!(
        out,
        [1.0 + f32::MINUS_3_DB * 3.0, 2.0 + f32::MINUS_3_DB * 4.0]
    );
    assert_eq!(
        Fold::<f32>::between(&default(3), &default(2)),
        Err(FoldError::Unassigned {
            src: ChannelLayout::from(3u16),
            dst: ChannelLayout::STEREO
        })
    );
}

/// Every pair of common topologies either refuses or places every non-LFE
/// source channel: an impulse on it reaches some output, and nothing is NaN.
#[test]
fn no_source_channel_is_silently_dropped() {
    let named = [
        ("mono", smpte(1)),
        ("stereo", smpte(2)),
        (
            "lcr",
            ChannelTopology::new([FrontLeft, FrontRight, FrontCenter]).unwrap(),
        ),
        ("quad", crate::Bed::Quad.topology()),
        (
            "5.0",
            ChannelTopology::new([FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight])
                .unwrap(),
        ),
        ("5.1", smpte(6)),
        (
            "6.1",
            ChannelTopology::new([
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
                BackCenter,
            ])
            .unwrap(),
        ),
        ("7.1", smpte(8)),
        ("7.1.4", smpte(12)),
    ];
    let mut built = 0;
    for (sn, s) in &named {
        for (dn, d) in &named {
            let Ok(f) = Fold::<f32>::between(s, d) else {
                // Only a destination with no home for a source speaker refuses.
                assert!(
                    matches!(Fold::<f32>::between(s, d), Err(FoldError::Unplaced { .. })),
                    "{sn} -> {dn}"
                );
                continue;
            };
            built += 1;
            for (k, sp) in s.positions().unwrap().iter().enumerate() {
                if sp.is_lfe() && d.lfe_index().is_none() {
                    continue;
                }
                let reached =
                    (0..usize::from(d.width().count())).any(|o| response(&f, o)[k] != 0.0);
                assert!(reached, "{sn} -> {dn}: channel {k} ({sp:?}) vanished");
            }
        }
    }
    assert!(built >= 70, "built {built}");
}

/// Planar and planar-to-interleaved output are the frame output, bit for bit, for
/// every built pair, so a node and an edge cannot disagree.
#[test]
fn planar_is_the_frame_fold() {
    let all = [
        smpte(1),
        smpte(2),
        crate::Bed::Quad.topology(),
        smpte(6),
        smpte(8),
        smpte(12),
    ];
    for s in &all {
        for d in &all {
            let Ok(f) = Fold::<f32>::between(s, d) else {
                continue;
            };
            let (sw, dw) = (
                usize::from(s.width().count()),
                usize::from(d.width().count()),
            );
            let frames = 137;
            let data = noise(sw * frames, 11);
            let planes: Vec<Vec<f32>> = (0..sw)
                .map(|c| data.chunks_exact(sw).map(|fr| fr[c]).collect())
                .collect();
            let mut out = vec![0.0; dw];
            for c in 0..dw {
                let mut plane = vec![f32::NAN; frames];
                planar_channel(&f, &planes, c, &mut plane);
                for (i, fr) in data.chunks_exact(sw).enumerate() {
                    f.frame(fr, &mut out);
                    assert_eq!(plane[i].to_bits(), out[c].to_bits(), "{sw}->{dw} c{c} f{i}");
                }
            }
            let mut device = vec![f32::NAN; dw * frames];
            f.fold_planar_to_interleaved(
                Planes::from_vecs(&planes).unwrap(),
                InterleavedMut::new(&mut device, d.width()).unwrap(),
            )
            .unwrap();
            for (i, fr) in data.chunks_exact(sw).enumerate() {
                f.frame(fr, &mut out);
                let got = &device[i * dw..(i + 1) * dw];
                assert_eq!(
                    got.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                    out.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                    "{sw}->{dw} device frame {i}"
                );
            }
        }
    }
}

/// A frame of the wrong width panics in a debug build instead of writing fewer
/// samples.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "destination width")]
fn a_short_destination_frame_panics() {
    let f = fold(&smpte(6), &smpte(2));
    f.frame(&[0.0; 6], &mut [0.0; 1]);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "source width")]
fn a_short_source_frame_panics() {
    let f = fold(&smpte(6), &smpte(2));
    f.frame(&[0.0; 5], &mut [0.0; 2]);
}

/// Every coefficient of the fallback table, pinned to its analytic value: the
/// whole response matrix of each pair, as `(dst, src, gain)` entries; every
/// other entry is zero. Fold gains are `M` (−3 dB) except a reposition (1.0).
/// The stereo rows are the ITU/Dolby figures; every non-stereo row is this
/// crate's own rule, pinned here so a change to it is a reviewed edit.
#[test]
fn every_fallback_rule_is_pinned() {
    type Entry = (usize, usize, f32);
    let cases: Vec<(&str, Vec<Speaker>, Vec<Speaker>, Vec<Entry>)> = vec![
        (
            "6.1 -> stereo",
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
                BackCenter,
            ],
            vec![FrontLeft, FrontRight],
            vec![
                (0, 0, 1.0),
                (0, 2, M),
                (0, 4, M),
                (0, 6, M),
                (1, 1, 1.0),
                (1, 2, M),
                (1, 5, M),
                (1, 6, M),
            ],
        ),
        (
            "6.1 -> 5.1",
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
                BackCenter,
            ],
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
            ],
            vec![
                (0, 0, 1.0),
                (1, 1, 1.0),
                (2, 2, 1.0),
                (3, 3, 1.0),
                (4, 4, 1.0),
                (4, 6, M),
                (5, 5, 1.0),
                (5, 6, M),
            ],
        ),
        (
            "6.1 -> quad",
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
                BackCenter,
            ],
            vec![FrontLeft, FrontRight, BackLeft, BackRight],
            vec![
                (0, 0, 1.0),
                (0, 2, M),
                (1, 1, 1.0),
                (1, 2, M),
                (2, 4, 1.0),
                (2, 6, M),
                (3, 5, 1.0),
                (3, 6, M),
            ],
        ),
        (
            "5.0 -> stereo",
            vec![FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight],
            vec![FrontLeft, FrontRight],
            vec![
                (0, 0, 1.0),
                (0, 2, M),
                (0, 3, M),
                (1, 1, 1.0),
                (1, 2, M),
                (1, 4, M),
            ],
        ),
        (
            "5.0 -> quad",
            vec![FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight],
            vec![FrontLeft, FrontRight, BackLeft, BackRight],
            vec![
                (0, 0, 1.0),
                (0, 2, M),
                (1, 1, 1.0),
                (1, 2, M),
                (2, 3, 1.0),
                (3, 4, 1.0),
            ],
        ),
        (
            "LCR -> stereo",
            vec![FrontLeft, FrontRight, FrontCenter],
            vec![FrontLeft, FrontRight],
            vec![(0, 0, 1.0), (0, 2, M), (1, 1, 1.0), (1, 2, M)],
        ),
        (
            "LCR -> 5.1 (zero-fill)",
            vec![FrontLeft, FrontRight, FrontCenter],
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
            ],
            vec![(0, 0, 1.0), (1, 1, 1.0), (2, 2, 1.0)],
        ),
        (
            "7.0 -> 5.1",
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                SideLeft,
                SideRight,
                BackLeft,
                BackRight,
            ],
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
            ],
            vec![
                (0, 0, 1.0),
                (1, 1, 1.0),
                (2, 2, 1.0),
                (4, 3, 1.0),
                (4, 5, M),
                (5, 4, 1.0),
                (5, 6, M),
            ],
        ),
        (
            "7.0 -> stereo",
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                SideLeft,
                SideRight,
                BackLeft,
                BackRight,
            ],
            vec![FrontLeft, FrontRight],
            vec![
                (0, 0, 1.0),
                (0, 2, M),
                (0, 3, M),
                (0, 5, M),
                (1, 1, 1.0),
                (1, 2, M),
                (1, 4, M),
                (1, 6, M),
            ],
        ),
        (
            "7.1 -> quad (both pairs present: sides fold at -3 dB)",
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
                BackLeft,
                BackRight,
            ],
            vec![FrontLeft, FrontRight, BackLeft, BackRight],
            vec![
                (0, 0, 1.0),
                (0, 2, M),
                (1, 1, 1.0),
                (1, 2, M),
                (2, 6, 1.0),
                (2, 4, M),
                (3, 7, 1.0),
                (3, 5, M),
            ],
        ),
        (
            "top-side -> side surround",
            vec![FrontLeft, FrontRight, TopSideLeft, TopSideRight],
            vec![FrontLeft, FrontRight, SideLeft, SideRight],
            vec![(0, 0, 1.0), (1, 1, 1.0), (2, 2, M), (3, 3, M)],
        ),
        (
            "top-side -> stereo",
            vec![FrontLeft, FrontRight, TopSideLeft, TopSideRight],
            vec![FrontLeft, FrontRight],
            vec![(0, 0, 1.0), (0, 2, M), (1, 1, 1.0), (1, 3, M)],
        ),
        (
            "top-back -> back",
            vec![FrontLeft, FrontRight, TopBackLeft, TopBackRight],
            vec![FrontLeft, FrontRight, BackLeft, BackRight],
            vec![(0, 0, 1.0), (1, 1, 1.0), (2, 2, M), (3, 3, M)],
        ),
        (
            "top-back -> side surround (no back)",
            vec![FrontLeft, FrontRight, TopBackLeft, TopBackRight],
            vec![FrontLeft, FrontRight, SideLeft, SideRight],
            vec![(0, 0, 1.0), (1, 1, 1.0), (2, 2, M), (3, 3, M)],
        ),
        (
            "top-back -> stereo",
            vec![FrontLeft, FrontRight, TopBackLeft, TopBackRight],
            vec![FrontLeft, FrontRight],
            vec![(0, 0, 1.0), (0, 2, M), (1, 1, 1.0), (1, 3, M)],
        ),
        (
            "top-back centre -> back centre",
            vec![FrontLeft, FrontRight, TopBackCenter],
            vec![FrontLeft, FrontRight, BackCenter],
            vec![(0, 0, 1.0), (1, 1, 1.0), (2, 2, M)],
        ),
        (
            "top-back centre -> back pair",
            vec![FrontLeft, FrontRight, TopBackCenter],
            vec![FrontLeft, FrontRight, BackLeft, BackRight],
            vec![(0, 0, 1.0), (1, 1, 1.0), (2, 2, M), (3, 2, M)],
        ),
        (
            "top-back centre -> stereo",
            vec![FrontLeft, FrontRight, TopBackCenter],
            vec![FrontLeft, FrontRight],
            vec![(0, 0, 1.0), (0, 2, M), (1, 1, 1.0), (1, 2, M)],
        ),
        (
            "left/right of centre -> centre",
            vec![FrontLeft, FrontRight, FrontLeftOfCenter, FrontRightOfCenter],
            vec![FrontLeft, FrontRight, FrontCenter],
            vec![(0, 0, 1.0), (1, 1, 1.0), (2, 2, M), (2, 3, M)],
        ),
        (
            "left/right of centre -> stereo",
            vec![FrontLeft, FrontRight, FrontLeftOfCenter, FrontRightOfCenter],
            vec![FrontLeft, FrontRight],
            vec![(0, 0, 1.0), (0, 2, M), (1, 1, 1.0), (1, 3, M)],
        ),
        (
            "top-front centre -> centre",
            vec![FrontLeft, FrontRight, TopFrontCenter],
            vec![FrontLeft, FrontRight, FrontCenter],
            vec![(0, 0, 1.0), (1, 1, 1.0), (2, 2, M)],
        ),
        (
            "top-front centre -> stereo",
            vec![FrontLeft, FrontRight, TopFrontCenter],
            vec![FrontLeft, FrontRight],
            vec![(0, 0, 1.0), (0, 2, M), (1, 1, 1.0), (1, 2, M)],
        ),
        (
            "top centre -> stereo",
            vec![FrontLeft, FrontRight, TopCenter],
            vec![FrontLeft, FrontRight],
            vec![(0, 0, 1.0), (0, 2, M), (1, 1, 1.0), (1, 2, M)],
        ),
        (
            "top-front -> front (5.1.2-like, front pair)",
            vec![FrontLeft, FrontRight, TopFrontLeft, TopFrontRight],
            vec![FrontLeft, FrontRight, FrontCenter],
            vec![(0, 0, 1.0), (0, 2, M), (1, 1, 1.0), (1, 3, M)],
        ),
        (
            "reposition: sides into back, no back in the source",
            vec![FrontLeft, FrontRight, SideLeft, SideRight],
            vec![FrontLeft, FrontRight, BackLeft, BackRight],
            vec![(0, 0, 1.0), (1, 1, 1.0), (2, 2, 1.0), (3, 3, 1.0)],
        ),
        (
            "reposition: back into sides, no side in the source",
            vec![FrontLeft, FrontRight, BackLeft, BackRight],
            vec![FrontLeft, FrontRight, SideLeft, SideRight],
            vec![(0, 0, 1.0), (1, 1, 1.0), (2, 2, 1.0), (3, 3, 1.0)],
        ),
    ];
    for (name, src, dst, want) in cases {
        let f = Fold::<f32>::between(
            &ChannelTopology::new(src.clone()).unwrap(),
            &ChannelTopology::new(dst.clone()).unwrap(),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        for (o, speaker) in dst.iter().enumerate() {
            let row: Vec<f32> = (0..src.len())
                .map(|k| {
                    want.iter()
                        .find(|&&(d, s, _)| d == o && s == k)
                        .map_or(0.0, |&(_, _, g)| g)
                })
                .collect();
            assert_eq!(response(&f, o), row, "{name}: output {o} ({speaker:?})");
        }
    }
}

/// `select` maps chosen source channels one to one onto an unassigned
/// destination, bit-exact, and names what is out of range.
#[test]
fn select_picks_channels_one_to_one() {
    let sixteen = ChannelLayout::from(16u16);
    let f = Fold::<f32>::select(sixteen, &[3, 4, 3]).unwrap();
    assert_eq!(f.dst_layout(), ChannelLayout::from(3u16));
    let frame: Vec<f32> = (0..16).map(|k| k as f32 + 0.5).collect();
    let mut out = [0.0; 3];
    f.frame(&frame, &mut out);
    assert_eq!(out, [3.5, 4.5, 3.5]);
    assert_eq!(
        Fold::<f32>::select(sixteen, &[0, 16]),
        Err(FoldError::NoSuchChannel {
            channel: 16,
            width: sixteen
        })
    );
    assert_eq!(
        Fold::<f32>::select(sixteen, &[]),
        Err(FoldError::Buffer(BufferError::ZeroWidth))
    );
    assert_eq!(
        Fold::<f32>::select(ChannelLayout::EMPTY, &[0]),
        Err(FoldError::Buffer(BufferError::ZeroWidth))
    );
}

/// A list longer than a width can count is refused, so `narrow` cannot panic
/// on a destination width. The longest list that still counts is built.
#[test]
fn select_refuses_a_list_past_u16() {
    let wide = ChannelLayout::from(2u16);
    let ok = vec![1usize; usize::from(u16::MAX)];
    let f = Fold::<f32>::select(wide, &ok).unwrap();
    assert_eq!(f.dst_layout().count(), u16::MAX);
    let too_many = vec![1usize; usize::from(u16::MAX) + 1];
    assert_eq!(
        Fold::<f32>::select(wide, &too_many),
        Err(FoldError::Buffer(BufferError::Layout(
            LayoutError::TooWide(u64::from(u16::MAX) + 1)
        )))
    );
}

/// The −3 dB coefficient is `1/√2`, 0.7071, and its square is a half
/// (−3.0103 dB); and a fold applies it as written, not through the constant
/// alone (a centre-only 5.1 lands at 0.7071 on both sides).
#[test]
#[allow(
    clippy::approx_constant,
    reason = "the figure is pinned independently of the constant under test"
)]
fn the_minus_3_db_coefficient_is_the_itu_figure() {
    assert!(
        (f32::MINUS_3_DB - 0.7071).abs() < 1e-4,
        "{}",
        f32::MINUS_3_DB
    );
    assert!((f32::MINUS_3_DB * f32::MINUS_3_DB - 0.5).abs() < 1e-7);
    assert!((20.0 * f32::MINUS_3_DB.log10() + 3.0103).abs() < 1e-3);
    let mut out = [0.0f32; 2];
    fold(&smpte(6), &smpte(2)).frame(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], &mut out);
    assert!(
        (out[0] - 0.7071).abs() < 1e-4 && out[0] == out[1],
        "{out:?}"
    );
}

/// The matrix is not normalised: full scale on every 5.1 channel folds to
/// `1 + 2 · 0.7071 = 2.4142` per side (the LFE is dropped).
#[test]
fn full_scale_five_one_folds_above_full_scale() {
    let mut out = [0.0f32; 2];
    fold(&smpte(6), &smpte(2)).frame(&[1.0; 6], &mut out);
    assert!((out[0] - 2.414_213_6).abs() < 1e-6, "{}", out[0]);
    assert_eq!(out[0], out[1]);
}

/// `contributions(o)` of every destination channel as a dense row over the
/// source width.
fn dense(f: &Fold, o: usize) -> Vec<f32> {
    let mut row = vec![0.0; usize::from(f.src)];
    let mut last = None;
    for (src, gain) in f.contributions(o) {
        assert!(last < Some(src), "ascending, one pair per source channel");
        last = Some(src);
        row[src] = gain;
    }
    row
}

/// The pairs, as the hand-written ITU table gives them (sparse: a source the
/// destination does not hear is absent, not a zero pair).
#[test]
fn contributions_are_the_itu_table() {
    let pairs = |f: &Fold, o| f.contributions(o);
    let stereo = smpte(2);

    let five_one = fold(&smpte(6), &stereo);
    assert_eq!(pairs(&five_one, 0), [(0, 1.0), (2, M), (4, M)]);
    assert_eq!(pairs(&five_one, 1), [(1, 1.0), (2, M), (5, M)]);

    let quad = fold(&crate::Bed::Quad.topology(), &stereo);
    assert_eq!(pairs(&quad, 0), [(0, 1.0), (2, M)]);
    assert_eq!(pairs(&quad, 1), [(1, 1.0), (3, M)]);

    // Stereo into mono: the pair average, each at one half.
    assert_eq!(pairs(&fold(&stereo, &smpte(1)), 0), [(0, 0.5), (1, 0.5)]);
    // Wider into mono: the stereo fold times -3 dB, so the centre is `M·M`
    // (the figure `the_mono_fold_is_the_itu_table` pins as `2·MINUS_3_DB² = 1`).
    assert_eq!(
        pairs(&fold(&smpte(6), &smpte(1)), 0),
        [
            (0, M * 1.0),
            (1, M * 1.0),
            (2, M * M + M * M),
            (4, M * M),
            (5, M * M)
        ]
    );
    // Mono into stereo: the lone centre at unity into each side.
    let up = fold(&smpte(1), &stereo);
    assert_eq!(pairs(&up, 0), [(0, 1.0)]);
    assert_eq!(pairs(&up, 1), [(0, 1.0)]);

    // 7.1 into 5.1: the side pair joins the surrounds, as the table says.
    let seven_one = fold(&smpte(8), &smpte(6));
    for o in 0..6 {
        let row = dense(&seven_one, o);
        assert_eq!(row, response(&seven_one, o), "7.1 -> 5.1 channel {o}");
    }
}

/// A permutation and a `select` have one pair of gain exactly 1.0 per
/// destination channel (the lowering's plain-edge case), an upmix's silent
/// channel has none, and the identity fold is `(i, 1.0)`.
#[test]
fn copies_contribute_at_exactly_unity() {
    let id = fold(&smpte(6), &smpte(6));
    assert!(id.is_identity());
    for o in 0..6 {
        assert_eq!(id.contributions(o), [(o, 1.0)]);
    }
    let swapped = fold(
        &ChannelTopology::new([FrontRight, FrontLeft]).unwrap(),
        &smpte(2),
    );
    assert_eq!(swapped.contributions(0), [(1, 1.0)]);
    assert_eq!(swapped.contributions(1), [(0, 1.0)]);

    let pick = Fold::<f32>::select(ChannelLayout::from(16u16), &[3, 4, 3]).unwrap();
    let got: Vec<_> = (0..3).flat_map(|o| pick.contributions(o)).collect();
    assert_eq!(got, [(3, 1.0), (4, 1.0), (3, 1.0)]);

    // Mono into 5.1 feeds the centre alone; the rest are silent, no pairs.
    let up = fold(&smpte(1), &smpte(6));
    for o in 0..6 {
        let want: &[(usize, f32)] = if o == 2 { &[(0, 1.0)] } else { &[] };
        assert_eq!(up.contributions(o), want, "channel {o}");
    }
}

/// Invariant, for every pair of topologies the fold accepts: the dense row of
/// `contributions` is the fold's response to a unit impulse on each source
/// channel, bit for bit (one product per path, so no rounding differs), and
/// applying the rows as a matrix to a frame of noise gives the fold's output
/// to within a few ulps (the sum order is the one thing that differs, see
/// `contributions`). The pairs run over all the engine's ordered widths, the
/// quad and side-surround spellings, a permutation, a 12-wide overhead source
/// and an unassigned relabel.
#[test]
fn contributions_are_the_fold_as_a_matrix() {
    let mut tops: Vec<ChannelTopology> = ORDERED
        .iter()
        .map(|&n| ChannelTopology::default_for(ChannelLayout::from(n)).unwrap())
        .collect();
    tops.push(ChannelTopology::new([FrontLeft, FrontRight, SideLeft, SideRight]).unwrap());
    tops.push(ChannelTopology::new([FrontRight, FrontLeft, FrontCenter]).unwrap());
    tops.push(ChannelTopology::unassigned(ChannelLayout::from(3u16)).unwrap());
    tops.push(ChannelTopology::unassigned(ChannelLayout::from(6u16)).unwrap());
    let mut checked = 0;
    for src in &tops {
        for dst in &tops {
            let Ok(f) = Fold::<f32>::between(src, dst) else {
                continue;
            };
            checked += 1;
            let frame = noise(usize::from(f.src), 0x9e37 + checked);
            let mut want = vec![0.0; f.rows.len()];
            f.frame(&frame, &mut want);
            for (o, want) in want.iter().enumerate() {
                assert_eq!(dense(&f, o), response(&f, o), "{src:?} -> {dst:?} row {o}");
                let got: f32 = f.contributions(o).iter().map(|&(k, g)| g * frame[k]).sum();
                let scale = want.abs().max(1.0);
                assert!(
                    (got - want).abs() <= 8.0 * f32::EPSILON * scale * 12.0,
                    "{src:?} -> {dst:?} row {o}: {got} vs {want}"
                );
            }
        }
    }
    assert!(checked > 40, "only {checked} folds were built");
}

/// A destination channel out of range panics.
#[test]
#[should_panic]
fn contributions_past_the_destination_width_panic() {
    let _ = fold(&smpte(2), &smpte(2)).contributions(2);
}

/// Real spherical harmonics, SN3D, by ACN index: the oracle the tests measure
/// the conversions against. Degree `l` and order `m` come from `n = l² + l + m`;
/// the value is `√((2 − δm0)(l−|m|)!/(l+|m|)!) · P_l^|m|(sin el) · trig(|m|·az)`.
mod harmonics {
    fn degree_and_order(n: usize) -> (usize, i32) {
        let l = (0..).take_while(|l| l * l <= n).last().unwrap();
        (l, n as i32 - (l * l + l) as i32)
    }

    fn factorial(n: usize) -> f64 {
        (1..=n).map(|k| k as f64).product()
    }

    /// The associated Legendre function `P_l^m(x)`, `m >= 0`, by recurrence.
    fn legendre(l: usize, m: usize, x: f64) -> f64 {
        let mut pmm = 1.0;
        let mut fact = 1.0;
        for _ in 0..m {
            pmm *= -fact * ((1.0 - x) * (1.0 + x)).sqrt();
            fact += 2.0;
        }
        if l == m {
            return pmm;
        }
        let mut pmmp1 = x * (2 * m + 1) as f64 * pmm;
        for ll in (m + 2)..=l {
            let pll =
                (x * (2 * ll - 1) as f64 * pmmp1 - (ll + m - 1) as f64 * pmm) / (ll - m) as f64;
            pmm = pmmp1;
            pmmp1 = pll;
        }
        pmmp1
    }

    pub(super) fn sn3d(n: usize, azimuth: f64, sin_elevation: f64) -> f64 {
        let (l, m) = degree_and_order(n);
        let am = m.unsigned_abs() as usize;
        let norm =
            ((if m == 0 { 1.0 } else { 2.0 }) * factorial(l - am) / factorial(l + am)).sqrt();
        let trig = if m >= 0 {
            (am as f64 * azimuth).cos()
        } else {
            (am as f64 * azimuth).sin()
        };
        norm * legendre(l, am, sin_elevation) * trig
    }

    /// Mean square over the sphere of an SN3D harmonic, by the midpoint rule on
    /// an equal-area grid (uniform in azimuth and in the sine of elevation).
    pub(super) fn mean_square(n: usize) -> f64 {
        const AZ: usize = 720;
        const EL: usize = 360;
        let mut sum = 0.0;
        for i in 0..AZ {
            let az = (i as f64 + 0.5) / AZ as f64 * std::f64::consts::TAU;
            for j in 0..EL {
                let y = sn3d(n, az, -1.0 + (j as f64 + 0.5) * 2.0 / EL as f64);
                sum += y * y;
            }
        }
        sum / (AZ * EL) as f64
    }

    /// Peak magnitude of an SN3D harmonic over a grid that includes the poles.
    pub(super) fn peak(n: usize) -> f64 {
        const AZ: usize = 720;
        const EL: usize = 361;
        let mut peak = 0.0f64;
        for i in 0..AZ {
            let az = i as f64 / AZ as f64 * std::f64::consts::TAU;
            for j in 0..EL {
                let y = sn3d(n, az, -1.0 + j as f64 * 2.0 / (EL - 1) as f64);
                peak = peak.max(y.abs());
            }
        }
        peak
    }
}

/// What multiplies an SN3D channel to give the harmonic in `norm`, measured: N3D
/// is the reciprocal root of SN3D's mean square over the sphere (fully
/// normalised means mean square one), MaxN the reciprocal of its peak, and FuMa
/// is MaxN with W at −3 dB.
fn measured_scale(norm: AmbisonicNormalization, n: usize) -> f64 {
    match norm {
        AmbisonicNormalization::Sn3d => 1.0,
        AmbisonicNormalization::N3d => 1.0 / harmonics::mean_square(n).sqrt(),
        AmbisonicNormalization::MaxN => 1.0 / harmonics::peak(n),
        AmbisonicNormalization::FuMa if n == 0 => 0.5f64.sqrt() / harmonics::peak(n),
        AmbisonicNormalization::FuMa => 1.0 / harmonics::peak(n),
        _ => unreachable!("the 2D normalizations are not full-sphere"),
    }
}

fn ambisonic(
    order: u8,
    ordering: AmbisonicOrdering,
    normalization: AmbisonicNormalization,
) -> ChannelTopology {
    AmbisonicFormat::new(AmbisonicOrder::of(order), ordering, normalization).into()
}

/// Destination channels when only source channel `k` carries a one.
fn from_source(f: &Fold, k: usize) -> Vec<f32> {
    let mut frame = vec![0.0; f.src_layout().count().into()];
    frame[k] = 1.0;
    let mut out = vec![0.0; f.dst_layout().count().into()];
    f.frame(&frame, &mut out);
    out
}

fn close(got: f32, want: f64, what: &str) {
    near_rel(got, want, 4.0 * f64::from(f32::EPSILON), what);
}

/// `got` within a relative `tol` of `want`: for figures measured on a grid. The
/// midpoint rule's error on the mean square stays near 5e-5 after the root, and
/// the grid resolves each peak to about 1e-5, so the measured scales hold to
/// 1e-4, far under the gaps between scales.
fn near_rel(got: f32, want: f64, tol: f64, what: &str) {
    assert!(
        (f64::from(got) - want).abs() <= tol * want.abs().max(1.0),
        "{what}: {got} vs {want}"
    );
}

/// N3D is SN3D times `√(2l+1)`: `√3` for the first order, `√5` for the second,
/// `√7` for the third and 1 for W.
#[test]
fn sn3d_to_n3d_scales_each_degree_by_root_of_two_l_plus_one() {
    use AmbisonicNormalization::{N3d, Sn3d};
    let f = fold(
        &ambisonic(3, AmbisonicOrdering::Acn, Sn3d),
        &ambisonic(3, AmbisonicOrdering::Acn, N3d),
    );
    for (n, want) in [
        (0, 1.0),
        (1, 3f64.sqrt()),
        (3, 3f64.sqrt()),
        (4, 5f64.sqrt()),
    ]
    .into_iter()
    .chain([(8, 5f64.sqrt()), (9, 7f64.sqrt()), (15, 7f64.sqrt())])
    {
        let r = from_source(&f, n);
        close(r[n], want, &format!("ACN {n}"));
        assert!(r.iter().enumerate().all(|(i, &v)| i == n || v == 0.0));
    }
}

/// Every pair of the four full-sphere normalizations, at orders 1 to 3, scales
/// harmonic `n` by `scale(dst, n) / scale(src, n)`, the scales measured from the
/// spherical harmonics themselves.
#[test]
fn each_normalization_pair_converts_by_the_ratio_of_the_measured_scales() {
    use AmbisonicNormalization::*;
    let norms = [Sn3d, N3d, MaxN, FuMa];
    let scales: Vec<Vec<f64>> = norms
        .iter()
        .map(|&norm| (0..16).map(|n| measured_scale(norm, n)).collect())
        .collect();
    for order in 1..=3u8 {
        let width = usize::from((order + 1).pow(2));
        for (si, &s) in norms.iter().enumerate() {
            for (di, &d) in norms.iter().enumerate() {
                let f = fold(
                    &ambisonic(order, AmbisonicOrdering::Acn, s),
                    &ambisonic(order, AmbisonicOrdering::Acn, d),
                );
                for n in 0..width {
                    let r = from_source(&f, n);
                    near_rel(
                        r[n],
                        scales[di][n] / scales[si][n],
                        1e-4,
                        &format!("{s:?} -> {d:?}, order {order}, ACN {n}"),
                    );
                    assert!(r.iter().enumerate().all(|(i, &v)| i == n || v == 0.0));
                }
                // Orders 0 and 1 are one in SN3D and MaxN alike, so those two meet
                // as an identity at order 1; one scale always does.
                assert!(s != d || f.is_identity());
            }
        }
    }
}

/// MaxN's gains over SN3D are the published ones: 1 for degrees 0 and 1,
/// `2/√3` for degree 2 but R, and `√(8/5)`, `3/√5`, `√(45/32)` and 1 across
/// degree 3 (Q O M K L N P), written as closed forms.
#[test]
fn sn3d_to_maxn_gains_are_the_published_table() {
    use AmbisonicNormalization::{MaxN, Sn3d};
    let f = fold(
        &ambisonic(3, AmbisonicOrdering::Acn, Sn3d),
        &ambisonic(3, AmbisonicOrdering::Acn, MaxN),
    );
    let two_thirds = 2.0 / 3f64.sqrt();
    let want = [
        1.0,
        1.0,
        1.0,
        1.0,
        two_thirds,
        two_thirds,
        1.0,
        two_thirds,
        two_thirds,
        (8.0f64 / 5.0).sqrt(),
        3.0 / 5f64.sqrt(),
        (45.0f64 / 32.0).sqrt(),
        1.0,
        (45.0f64 / 32.0).sqrt(),
        3.0 / 5f64.sqrt(),
        (8.0f64 / 5.0).sqrt(),
    ];
    for (n, want) in want.into_iter().enumerate() {
        close(from_source(&f, n)[n], want, &format!("ACN {n}"));
    }
}

/// FuMa's W is MaxN's at −3 dB: SN3D W (one) becomes `1/√2`, and back `√2`.
#[test]
fn fuma_w_sits_at_minus_3_db() {
    use AmbisonicNormalization::{FuMa, Sn3d};
    let there = fold(
        &ambisonic(1, AmbisonicOrdering::Acn, Sn3d),
        &ambisonic(1, AmbisonicOrdering::Acn, FuMa),
    );
    assert_eq!(from_source(&there, 0)[0], f32::MINUS_3_DB);
    assert_eq!(
        from_source(&there, 1)[1],
        1.0,
        "the first-order gains are 1"
    );
    let back = fold(
        &ambisonic(1, AmbisonicOrdering::Acn, FuMa),
        &ambisonic(1, AmbisonicOrdering::Acn, Sn3d),
    );
    close(from_source(&back, 0)[0], 2f64.sqrt(), "FuMa W to SN3D");
}

/// FuMa stores `W X Y Z R S T U V K L M N O P Q`; each letter is a harmonic
/// `(l, m)` (X = (1, +1), Y = (1, −1), Z = (1, 0), S = (2, +1), T = (2, −1),
/// U = (2, +2), V = (2, −2), L = (3, +1), M = (3, −1), N = (3, +2), O = (3, −2),
/// P = (3, +3), Q = (3, −3)) and ACN index `l² + l + m`: the reorder lands each
/// letter on its ACN channel.
#[test]
fn fuma_channels_reorder_to_acn() {
    use AmbisonicNormalization::Sn3d;
    let letters: [(&str, i32, i32); 16] = [
        ("W", 0, 0),
        ("X", 1, 1),
        ("Y", 1, -1),
        ("Z", 1, 0),
        ("R", 2, 0),
        ("S", 2, 1),
        ("T", 2, -1),
        ("U", 2, 2),
        ("V", 2, -2),
        ("K", 3, 0),
        ("L", 3, 1),
        ("M", 3, -1),
        ("N", 3, 2),
        ("O", 3, -2),
        ("P", 3, 3),
        ("Q", 3, -3),
    ];
    let f = fold(
        &ambisonic(3, AmbisonicOrdering::FuMa, Sn3d),
        &ambisonic(3, AmbisonicOrdering::Acn, Sn3d),
    );
    for (i, (letter, l, m)) in letters.into_iter().enumerate() {
        let acn = (l * l + l + m) as usize;
        let r = from_source(&f, i);
        assert_eq!(r[acn], 1.0, "{letter} (FuMa {i}) is ACN {acn}");
        assert_eq!(r.iter().filter(|&&v| v != 0.0).count(), 1);
    }
    // ACN to FuMa is the inverse.
    let back = fold(
        &ambisonic(3, AmbisonicOrdering::Acn, Sn3d),
        &ambisonic(3, AmbisonicOrdering::FuMa, Sn3d),
    );
    for (i, (_, l, m)) in letters.into_iter().enumerate() {
        assert_eq!(from_source(&back, (l * l + l + m) as usize)[i], 1.0);
    }
}

/// FuMa B-format to AmbiX is the reorder and FuMa's per-harmonic gains undone:
/// SN3D = FuMa ÷ (MaxN gain, W at −3 dB), measured from the harmonics.
#[test]
fn fuma_to_ambix_reorders_and_undoes_the_fuma_gains() {
    for order in 1..=3u8 {
        let f = fold(
            &AmbisonicFormat::fuma(AmbisonicOrder::of(order)).into(),
            &AmbisonicFormat::ambix(AmbisonicOrder::of(order)).into(),
        );
        let fuma_to_acn = [0usize, 3, 1, 2, 6, 7, 5, 8, 4, 12, 13, 11, 14, 10, 15, 9];
        for (i, &acn) in fuma_to_acn[..usize::from((order + 1).pow(2))]
            .iter()
            .enumerate()
        {
            let want = 1.0 / measured_scale(AmbisonicNormalization::FuMa, acn);
            near_rel(
                from_source(&f, i)[acn],
                want,
                1e-4,
                &format!("order {order}, FuMa {i}"),
            );
        }
    }
}

/// A narrowing keeps the lower orders and reads nothing of the higher ones; a
/// widening keeps them and fills the new orders with silence.
#[test]
fn ambisonic_narrowing_drops_higher_orders_and_widening_zero_fills() {
    use AmbisonicNormalization::{N3d, Sn3d};
    let third = ambisonic(3, AmbisonicOrdering::Acn, Sn3d);
    let first = ambisonic(1, AmbisonicOrdering::Acn, Sn3d);
    let down = fold(&third, &first);
    assert_eq!(down.dst_layout(), ChannelLayout::QUAD);
    for k in 0..16 {
        let r = from_source(&down, k);
        if k < 4 {
            assert_eq!(r[k], 1.0);
        } else {
            assert!(r.iter().all(|&v| v == 0.0), "ACN {k} is above first order");
        }
    }
    let up = fold(&first, &ambisonic(2, AmbisonicOrdering::Acn, N3d));
    assert_eq!(up.dst_layout().count(), 9);
    close(
        from_source(&up, 2)[2],
        3f64.sqrt(),
        "a kept channel is rescaled",
    );
    for k in 0..4 {
        assert!(from_source(&up, k)[4..].iter().all(|&v| v == 0.0));
    }
    // Silence in, silence out for the new orders.
    let mut out = [1.0f32; 9];
    up.frame(&[0.5, 0.25, -0.5, 1.0], &mut out);
    assert!(out[4..].iter().all(|&v| v == 0.0));
    // Across orderings: FuMa third order to AmbiX first order keeps W X Y Z.
    let narrow = fold(&ambisonic(3, AmbisonicOrdering::FuMa, Sn3d), &first);
    assert_eq!(from_source(&narrow, 1)[3], 1.0, "FuMa X is ACN 3");
    assert!(
        from_source(&narrow, 4).iter().all(|&v| v == 0.0),
        "R is second order"
    );
}

/// Speakers and ambisonics never fold: they need an encoder or a decoder node.
#[test]
fn speakers_to_ambisonic_and_back_need_a_codec() {
    let first = ambisonic(1, AmbisonicOrdering::Acn, AmbisonicNormalization::Sn3d);
    for speakers in [
        crate::Bed::Quad.topology(),
        crate::Bed::Stereo.topology(),
        ChannelTopology::unassigned(ChannelLayout::QUAD).unwrap(),
    ] {
        assert_eq!(
            Fold::<f32>::between(&first, &speakers),
            Err(FoldError::NeedsCodec)
        );
        assert_eq!(
            Fold::<f32>::between(&speakers, &first),
            Err(FoldError::NeedsCodec)
        );
    }
}

/// An ambisonic set meets itself as the identity.
#[test]
fn an_ambisonic_set_folds_into_itself_as_the_identity() {
    let first = ambisonic(1, AmbisonicOrdering::FuMa, AmbisonicNormalization::FuMa);
    let f = fold(&first, &first);
    assert!(f.is_identity());
    assert_eq!(f.src_layout(), ChannelLayout::QUAD);
}

/// The horizontal-only normalizations convert to themselves, a pure reorder or
/// order change, and to nothing else: a 2D scale is defined for the horizontal
/// harmonics alone, so 2D to 3D has no coefficient for the rest, and the other
/// 2D scale is not converted.
#[test]
fn a_2d_normalization_converts_only_to_itself() {
    use AmbisonicNormalization::*;
    let acn = AmbisonicOrdering::Acn;
    for planar in [Sn2d, N2d] {
        let f = fold(&ambisonic(2, acn, planar), &ambisonic(1, acn, planar));
        assert_eq!(
            from_source(&f, 3)[3],
            1.0,
            "a narrowing within one 2D scale"
        );
        let g = fold(
            &ambisonic(1, AmbisonicOrdering::FuMa, planar),
            &ambisonic(1, acn, planar),
        );
        assert_eq!(from_source(&g, 1)[3], 1.0, "a reorder within one 2D scale");
        for other in [
            Sn3d,
            N3d,
            MaxN,
            FuMa,
            if planar == Sn2d { N2d } else { Sn2d },
        ] {
            assert_eq!(
                Fold::<f32>::between(&ambisonic(1, acn, planar), &ambisonic(1, acn, other)),
                Err(FoldError::PlanarNormalization {
                    src: planar,
                    dst: other
                })
            );
            assert_eq!(
                Fold::<f32>::between(&ambisonic(1, acn, other), &ambisonic(1, acn, planar)),
                Err(FoldError::PlanarNormalization {
                    src: other,
                    dst: planar
                })
            );
        }
    }
}

/// The default levels are ITU-R BS.775's, written out: 5.1 into stereo takes the
/// centre and each surround at -3 dB and omits the LFE, and `between` is that
/// fold.
#[test]
fn the_default_levels_are_bs775_minus_3_db() {
    use crate::Bed;
    use tutti_units::Amplitude;
    let m = f32::MINUS_3_DB;
    let levels = FoldLevels::default();
    let minus_3_db = Amplitude::new(f64::MINUS_3_DB);
    assert_eq!(
        (levels.centre, levels.surround, levels.height, levels.lfe),
        (minus_3_db, minus_3_db, minus_3_db, None)
    );
    let f = fold(&Bed::Surround51.topology(), &Bed::Stereo.topology());
    assert_eq!(f.contributions(0), [(0, 1.0), (2, m), (4, m)]);
    assert_eq!(f.contributions(1), [(1, 1.0), (2, m), (5, m)]);
}

/// A level that is not a gain (NaN, infinite or negative) is refused by name
/// when the fold is built, so it never reaches a signal.
#[test]
fn a_level_that_is_not_a_gain_is_refused() {
    use crate::Bed;
    use tutti_units::Amplitude;
    let (five_one, stereo) = (Bed::Surround51.topology(), Bed::Stereo.topology());
    for bad in [f64::NAN, f64::INFINITY, -0.5] {
        let spec = |levels| Fold::<f32>::spec(&five_one, &stereo).levels(levels).build();
        let base = FoldLevels::default();
        assert_eq!(
            spec(FoldLevels {
                centre: Amplitude::new(bad),
                ..base
            }),
            Err(FoldError::NotAGain {
                level: FoldLevel::Centre
            })
        );
        assert_eq!(
            spec(FoldLevels {
                lfe: Some(Amplitude::new(bad)),
                ..base
            }),
            Err(FoldError::NotAGain {
                level: FoldLevel::Lfe
            })
        );
    }
    assert!(
        Fold::<f32>::spec(&five_one, &stereo)
            .levels(FoldLevels {
                height: Amplitude::new(0.0),
                ..FoldLevels::default()
            })
            .build()
            .is_ok(),
        "silence is a gain"
    );
}

/// Stated levels are the coefficients of the fold, per class of speaker: the
/// centre, the surrounds and the heights each at their own gain.
#[test]
fn levels_state_the_centre_the_surround_and_the_height_gains() {
    use crate::Bed;
    use tutti_units::Amplitude;
    let levels = FoldLevels {
        centre: Amplitude::new(0.5),
        surround: Amplitude::new(0.25),
        height: Amplitude::new(0.125),
        lfe: None,
    };
    let five_one = Bed::Surround51.topology();
    let stereo = Bed::Stereo.topology();
    let f = Fold::<f32>::spec(&five_one, &stereo)
        .levels(levels)
        .build()
        .unwrap();
    // 5.1: FL FR FC LFE SL SR. The LFE is omitted at the default.
    assert_eq!(f.contributions(0), [(0, 1.0), (2, 0.5), (4, 0.25)]);
    assert_eq!(f.contributions(1), [(1, 1.0), (2, 0.5), (5, 0.25)]);

    // 7.1.4 into 7.1: each height folds into the layer below at the height
    // gain. FL FR FC LFE SL SR BL BR TFL TFR TBL TBR.
    let seven_one = Bed::Surround71.topology();
    let f = Fold::<f32>::spec(&Bed::Surround714.topology(), &seven_one)
        .levels(levels)
        .build()
        .unwrap();
    assert_eq!(f.contributions(0), [(0, 1.0), (8, 0.125)]);
    assert_eq!(f.contributions(1), [(1, 1.0), (9, 0.125)]);
    assert_eq!(f.contributions(6), [(6, 1.0), (10, 0.125)]);
    assert_eq!(f.contributions(7), [(7, 1.0), (11, 0.125)]);
    // The centre and the LFE are in both beds: straight across.
    assert_eq!(f.contributions(2), [(2, 1.0)]);
    assert_eq!(f.contributions(3), [(3, 1.0)]);
}

/// A destination that lacks the speaker takes the gain; one that has it takes
/// the speaker at unity whatever the level says.
#[test]
fn a_level_applies_only_where_the_speaker_folds() {
    use crate::Bed;
    use tutti_units::Amplitude;
    let levels = FoldLevels {
        surround: Amplitude::new(0.25),
        ..FoldLevels::default()
    };
    // 5.1 to quad: the side pair is repositioned to the back pair, at unity.
    let f = Fold::<f32>::spec(&Bed::Surround51.topology(), &Bed::Quad.topology())
        .levels(levels)
        .build()
        .unwrap();
    let back_left = f.contributions(2);
    assert!(back_left.contains(&(4, 1.0)), "{back_left:?}");
    // 7.1 to 5.1: the back pair is missing and folds into the side pair, at
    // the surround level.
    let f = Fold::<f32>::spec(&Bed::Surround71.topology(), &Bed::Surround51.topology())
        .levels(levels)
        .build()
        .unwrap();
    assert!(
        f.contributions(4).contains(&(6, 0.25)),
        "{:?}",
        f.contributions(4)
    );
}

/// `lfe: Some(g)` feeds the LFE to the front pair at `g`, and to the one
/// channel of a mono destination at `g`; `None` omits it.
#[test]
fn the_lfe_level_feeds_the_front_pair_or_the_mono_channel() {
    use crate::Bed;
    use tutti_units::Amplitude;
    let five_one = Bed::Surround51.topology();
    let with = FoldLevels {
        lfe: Some(Amplitude::new(0.5)),
        ..FoldLevels::default()
    };
    let stereo = Bed::Stereo.topology();
    let f = Fold::<f32>::spec(&five_one, &stereo)
        .levels(with)
        .build()
        .unwrap();
    assert!(f.contributions(0).contains(&(3, 0.5)));
    assert!(f.contributions(1).contains(&(3, 0.5)));
    let none = Fold::<f32>::between(&five_one, &stereo).unwrap();
    assert!(none.contributions(0).iter().all(|&(c, _)| c != 3));

    // Mono: the stereo fold times -3 dB, and the LFE at its own gain.
    let mono = Bed::Mono.topology();
    let f = Fold::<f32>::spec(&five_one, &mono)
        .levels(with)
        .build()
        .unwrap();
    let c = f.contributions(0);
    assert_eq!(
        c.iter().filter(|&&(s, _)| s == 3).collect::<Vec<_>>(),
        [&(3, 0.5)]
    );
    // And only the LFE changed from the default mono mix.
    let plain = Fold::<f32>::between(&five_one, &mono)
        .unwrap()
        .contributions(0);
    let others: Vec<_> = c.iter().filter(|&&(s, _)| s != 3).copied().collect();
    assert_eq!(others, plain);
}

/// A composition is the matrix product of the two folds' coefficients, written
/// out by hand: 5.1 into stereo, then stereo into mono (`(L + R) / 2`).
#[test]
fn then_is_the_matrix_product() {
    use crate::Bed;
    let m = f32::MINUS_3_DB;
    let a = fold(&Bed::Surround51.topology(), &Bed::Stereo.topology());
    let b = fold(&Bed::Stereo.topology(), &Bed::Mono.topology());
    let both = a.then(&b).unwrap();
    // Hand product. A: L = x0 + m·x2 + m·x4, R = x1 + m·x2 + m·x5;
    // B: out = ½L + ½R.
    let expected = [
        (0, 0.5 * 1.0),
        (1, 0.5 * 1.0),
        (2, 0.5 * m + 0.5 * m),
        (4, 0.5 * m),
        (5, 0.5 * m),
    ];
    assert_eq!(both.contributions(0), expected);
    assert_eq!(both.src_layout(), a.src_layout());
    assert_eq!(both.dst_layout(), b.dst_layout());
    // An analytic frame: (x0 + x1)/2 + m·x2 + (x4 + x5)·m/2, at rounding.
    let x = [0.2f32, -0.4, 0.6, 0.9, 0.3, 0.5];
    let mut out = [0.0f32];
    both.frame(&x, &mut out);
    let want = (x[0] + x[1]) / 2.0 + m * x[2] + m * (x[4] + x[5]) / 2.0;
    assert!((out[0] - want).abs() < 1e-6, "{} vs {want}", out[0]);
}

/// An identity on either side leaves the other as it is, and a width that does
/// not meet is refused.
#[test]
fn then_with_an_identity_is_the_other_fold_and_widths_must_meet() {
    use crate::Bed;
    let a = fold(&Bed::Surround51.topology(), &Bed::Stereo.topology());
    let stereo_id = fold(&Bed::Stereo.topology(), &Bed::Stereo.topology());
    assert_eq!(a.then(&stereo_id).unwrap(), a);
    let six_id = fold(&Bed::Surround51.topology(), &Bed::Surround51.topology());
    assert_eq!(six_id.then(&a).unwrap(), a);
    assert_eq!(
        a.then(&a),
        Err(BufferError::WidthMismatch {
            src: ChannelLayout::STEREO,
            dst: ChannelLayout::from(6u16),
        })
    );
}

/// A route sends each source to its destination channel and silences the rest;
/// two sources to one destination are summed; a channel past the width is
/// refused by name.
#[test]
fn route_sends_channels_to_channels() {
    let (two, four) = (ChannelLayout::STEREO, ChannelLayout::QUAD);
    let f = Fold::<f32>::route(two, four, &[3, 1]).unwrap();
    let mut out = [9.0f32; 4];
    f.frame(&[0.25, 0.75], &mut out);
    assert_eq!(out, [0.0, 0.75, 0.0, 0.25]);
    let sum = Fold::<f32>::route(two, four, &[0, 0]).unwrap();
    sum.frame(&[0.25, 0.75], &mut out);
    assert_eq!(out, [1.0, 0.0, 0.0, 0.0]);
    assert_eq!(
        Fold::<f32>::route(two, four, &[0, 4]),
        Err(FoldError::NoSuchChannel {
            channel: 4,
            width: four
        })
    );
    assert!(matches!(
        Fold::<f32>::route(two, four, &[0]),
        Err(FoldError::Buffer(BufferError::WidthMismatch { .. }))
    ));
}

/// Narrowing keeps the routing and rounds each gain once: the 5.1 to stereo fold built in
/// `f64` and narrowed to `f32` has the ITU table's gains (1 for the front, −3 dB for the centre
/// and the surround) and is the fold built in `f32`; narrowed to `f64` it is the fold itself.
#[test]
fn a_narrowed_fold_applies_the_same_gains() {
    let wide = Fold::<f64>::between(&smpte(6), &smpte(2)).unwrap();
    assert_eq!(wide.narrowed::<f64>(), wide);
    let narrow: Fold<f32> = wide.narrowed();
    assert_eq!(narrow, Fold::<f32>::between(&smpte(6), &smpte(2)).unwrap());
    assert_eq!(narrow.contributions(0), [(0, 1.0), (2, M), (4, M)]);
    assert_eq!(narrow.contributions(1), [(1, 1.0), (2, M), (5, M)]);
}
