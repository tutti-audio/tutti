//! The ambisonic fold: reorder, rescale, drop or zero-fill, by harmonic.
//!
//! A channel is one real spherical harmonic, named by its ACN index
//! `n = l² + l + m` (degree `l`, `m` in `-l..=l`). A format stores the
//! harmonics in an order ([`AmbisonicOrdering`]) and scales each by a
//! normalization ([`AmbisonicNormalization`]); a conversion reads every
//! destination harmonic from the source channel that holds it, times the ratio
//! of the two scales, or silence where the source has no such harmonic.

use super::{Expr, FoldError, Row, Term};
use crate::Float;
use tutti_types::{AmbisonicFormat, AmbisonicNormalization, AmbisonicOrdering};

/// The ACN index held by FuMa channel `i` (`W X Y Z R S T U V K L M N O P Q`).
///
/// Furse-Malham exists to third order, which is the highest an
/// [`AmbisonicOrder`](tutti_types::AmbisonicOrder) holds, so no format asks for
/// a FuMa channel past `Q` and a "FuMa above third order" refusal is
/// unwritable: the type states it.
const FUMA_TO_ACN: [u16; 16] = [0, 3, 1, 2, 6, 7, 5, 8, 4, 12, 13, 11, 14, 10, 15, 9];

/// The FuMa channel holding ACN harmonic `n`: the inverse of [`FUMA_TO_ACN`]
/// (`W Y Z X V T R S U Q O M K L N P` in ACN order).
const ACN_TO_FUMA: [u16; 16] = [0, 2, 3, 1, 8, 6, 4, 5, 7, 15, 13, 11, 9, 10, 12, 14];

/// The square of MaxN's gain over SN3D, as a ratio, per ACN harmonic: the
/// reciprocal of the square of the harmonic's peak over the sphere. Degrees 0
/// and 1 peak at one; degree 2 is `4/3` except `R` (1); degree 3 is `8/5` for
/// `P` and `Q`, `9/5` for `N` and `O`, `45/32` for `L` and `M`, and 1 for `K`.
/// These are the values of Malham's Furse-Malham definition and of the AmbiX
/// paper's MaxN table (Nachbar et al., 2011).
const MAXN_SQUARED: [(u32, u32); 16] = [
    (1, 1),
    (1, 1),
    (1, 1),
    (1, 1),
    (4, 3),
    (4, 3),
    (1, 1),
    (4, 3),
    (4, 3),
    (8, 5),
    (9, 5),
    (45, 32),
    (1, 1),
    (45, 32),
    (9, 5),
    (8, 5),
];

/// The degree `l` of ACN harmonic `n`: `⌊√n⌋`.
fn degree(n: u16) -> u16 {
    n.isqrt()
}

/// A full-sphere normalization: the four whose scales over SN3D are defined
/// for every harmonic.
#[derive(Clone, Copy)]
enum Scale {
    Sn3d,
    N3d,
    MaxN,
    FuMa,
}

impl Scale {
    /// The full-sphere scale of `normalization`, or `None` for a 2D one.
    fn of(normalization: AmbisonicNormalization) -> Option<Self> {
        use AmbisonicNormalization as N;
        match normalization {
            N::Sn3d => Some(Self::Sn3d),
            N::N3d => Some(Self::N3d),
            N::MaxN => Some(Self::MaxN),
            N::FuMa => Some(Self::FuMa),
            N::Sn2d | N::N2d => None,
        }
    }

    /// What multiplies an SN3D channel to give ACN harmonic `n` in this scale.
    fn over_sn3d(self, n: u16) -> f64 {
        let maxn = || {
            let (num, den) = MAXN_SQUARED[usize::from(n)];
            (f64::from(num) / f64::from(den)).sqrt()
        };
        match self {
            Self::Sn3d => 1.0,
            Self::N3d => f64::from(2 * degree(n) + 1).sqrt(),
            Self::MaxN => maxn(),
            // FuMa is MaxN with W at -3 dB.
            Self::FuMa if n == 0 => maxn() * 0.5f64.sqrt(),
            Self::FuMa => maxn(),
        }
    }
}

/// The ACN harmonic stored in channel `i` of `ordering`.
fn acn(ordering: AmbisonicOrdering, i: u16) -> u16 {
    match ordering {
        AmbisonicOrdering::Acn => i,
        AmbisonicOrdering::FuMa => FUMA_TO_ACN[usize::from(i)],
    }
}

/// The channel of `ordering` that holds ACN harmonic `n`.
fn channel(ordering: AmbisonicOrdering, n: u16) -> u16 {
    match ordering {
        AmbisonicOrdering::Acn => n,
        AmbisonicOrdering::FuMa => ACN_TO_FUMA[usize::from(n)],
    }
}

/// One destination channel per harmonic of `dst`: the source channel holding it
/// scaled by the ratio of the two normalizations, or zero past the source's
/// order. Orders above the destination's are not read.
///
/// A 2D normalization ([`Sn2d`](AmbisonicNormalization::Sn2d),
/// [`N2d`](AmbisonicNormalization::N2d)) is defined for the horizontal
/// harmonics only, while the set holds every channel of its order; it converts
/// to itself (a reorder, drop or zero-fill) and to nothing else, and any other
/// pairing is [`FoldError::PlanarNormalization`].
pub(super) fn rows<F: Float>(
    src: AmbisonicFormat,
    dst: AmbisonicFormat,
) -> Result<Box<[Row<F>]>, FoldError> {
    let (from, to) = (src.normalization(), dst.normalization());
    // `None` is one scale on both sides, a plain reorder; `Some` holds both
    // full-sphere scales.
    let scales = if from == to {
        None
    } else {
        match (Scale::of(from), Scale::of(to)) {
            (Some(f), Some(t)) => Some((f, t)),
            _ => return Err(FoldError::PlanarNormalization { src: from, dst: to }),
        }
    };
    let src_width = src.width().count();
    Ok((0..dst.width().count())
        .map(|j| {
            let n = acn(dst.ordering(), j);
            if n >= src_width {
                return Row::One(Expr::Zero);
            }
            let c = channel(src.ordering(), n);
            let gain = scales.map_or(1.0, |(f, t)| t.over_sn3d(n) / f.over_sn3d(n));
            Row::One(if gain == 1.0 {
                Expr::Copy(c)
            } else {
                Expr::Sum(Box::new([Term {
                    gain: F::from_f64(gain),
                    srcs: Box::new([c]),
                }]))
            })
        })
        .collect())
}
