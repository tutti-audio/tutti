//! [`Fold`]: the one way a signal of one channel topology becomes another's.

use crate::convert::same_frames;
use crate::{AmbisonicNormalization, ChannelLayout, ChannelTopology, Speaker};
use crate::{BufferError, Float, Interleaved, InterleavedMut, MonoMut, Planes, PlanesMut};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

mod ambisonic;
mod compose;
mod levels;

pub use levels::{FoldLevel, FoldLevels, FoldSpec};

/// Which end of a fold a [`FoldError`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FoldEnd {
    /// The topology being read.
    Source,
    /// The topology being written.
    Destination,
}

/// Why two topologies cannot be folded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FoldError {
    /// A width is zero channels ([`BufferError::ZeroWidth`]) or does not fit a
    /// channel layout ([`BufferError::Layout`], carrying a [`LayoutError`](crate::LayoutError)).
    #[error(transparent)]
    Buffer(#[from] BufferError),
    /// Source channel `channel` (speaker `speaker`) has no place in the
    /// destination and no rule folds it elsewhere.
    #[error("source channel {channel} ({speaker:?}) has no place in the destination")]
    Unplaced {
        /// The source channel index.
        channel: usize,
        /// The speaker it feeds.
        speaker: Speaker,
    },
    /// A channel is [`Unknown`](Speaker::Unknown) and the topologies differ, so
    /// nothing can be said about where it goes.
    #[error("{end:?} channel {channel} has an unknown position")]
    Unnamed {
        /// Which topology holds it.
        end: FoldEnd,
        /// Its channel index.
        channel: usize,
    },
    /// Speakers meet an ambisonic set. An ambisonic signal is not speaker
    /// feeds: going either way needs an encoder or a decoder node, which a
    /// fold is not.
    #[error("speakers and an ambisonic set need an encoder or decoder node, not a fold")]
    NeedsCodec,
    /// A conversion between ambisonic formats where one normalization is 2D
    /// ([`Sn2d`] or [`N2d`]) and the other differs. A 2D scale is defined for the
    /// horizontal harmonics alone, while the set holds every channel of its
    /// order, so 2D to 3D (or 3D to 2D) has no coefficient for the rest. 2D to
    /// the other 2D scale is not converted either.
    ///
    /// [`Sn2d`]: AmbisonicNormalization::Sn2d
    /// [`N2d`]: AmbisonicNormalization::N2d
    #[error("no conversion between the {src:?} and {dst:?} normalizations is defined")]
    PlanarNormalization {
        /// The source normalization.
        src: AmbisonicNormalization,
        /// The destination normalization.
        dst: AmbisonicNormalization,
    },
    /// An unassigned side meets another at a different width. The only rule is a
    /// one to one relabelling of equal widths; by index is
    /// [`Fold::by_index`], asked for by name.
    #[error("cannot fold {src:?} into {dst:?}: unassigned channels only relabel at equal width")]
    Unassigned {
        /// The source width.
        src: ChannelLayout,
        /// The destination width.
        dst: ChannelLayout,
    },
    /// A downmix level is not a gain: NaN, infinite or negative.
    #[error("the {level:?} level is not a gain (NaN, infinite or negative)")]
    NotAGain {
        /// Which level.
        level: FoldLevel,
    },
    /// [`Fold::select`] named a channel the source does not have.
    #[error("channel {channel} is not in a {width:?} source")]
    NoSuchChannel {
        /// The selected channel.
        channel: usize,
        /// The source width.
        width: ChannelLayout,
    },
}

/// Frames per chunk of the planar group sum's stack scratch.
const CHUNK: usize = 64;

/// One product term of an output: `gain · (x[a] + x[b] + …)`.
#[derive(Debug, Clone, PartialEq)]
struct Term<F> {
    gain: F,
    srcs: Box<[u16]>,
}

/// A sum of terms, evaluated left to right as `((t0 + t1) + t2)…`.
#[derive(Debug, Clone, PartialEq)]
enum Expr<F> {
    Zero,
    Copy(u16),
    Sum(Box<[Term<F>]>),
}

/// One output channel.
#[derive(Debug, Clone, PartialEq)]
enum Row<F> {
    One(Expr<F>),
    /// `(a + b) · scale`: the mono mix of a stereo pair.
    Mix {
        a: Expr<F>,
        b: Expr<F>,
        scale: F,
    },
}

impl<F: Float> Expr<F> {
    /// Value at one frame, `get(c)` reading source channel `c`.
    #[inline]
    fn at(&self, get: &impl Fn(usize) -> F) -> F {
        match self {
            Self::Zero => F::ZERO,
            Self::Copy(c) => get(usize::from(*c)),
            Self::Sum(terms) => terms
                .iter()
                .map(|t| {
                    t.gain
                        * t.srcs
                            .iter()
                            .map(|&c| get(usize::from(c)))
                            .reduce(|a, b| a + b)
                            .unwrap_or(F::ZERO)
                })
                .reduce(|a, b| a + b)
                .unwrap_or(F::ZERO),
        }
    }

    /// The same values over a block, term by term. A lone-source term runs as
    /// a plain loop over whole slices; a summed group reads per frame.
    fn planar<'a>(&self, src: &impl Fn(usize) -> &'a [F], out: &mut [F]) {
        let n = out.len();
        match self {
            Self::Zero => out.fill(F::ZERO),
            Self::Copy(c) => out.copy_from_slice(&src(usize::from(*c))[..n]),
            Self::Sum(terms) => {
                for (k, t) in terms.iter().enumerate() {
                    match &*t.srcs {
                        [c] => {
                            let x = &src(usize::from(*c))[..n];
                            if k == 0 {
                                out.iter_mut().zip(x).for_each(|(o, &x)| *o = t.gain * x);
                            } else {
                                out.iter_mut().zip(x).for_each(|(o, &x)| *o += t.gain * x);
                            }
                        }
                        group => {
                            // The group sum a chunk at a time, in a stack
                            // scratch, so every loop runs over whole slices.
                            let mut sum = [F::ZERO; CHUNK];
                            for (n, o) in out.chunks_mut(CHUNK).enumerate() {
                                let at = n * CHUNK..n * CHUNK + o.len();
                                let sum = &mut sum[..o.len()];
                                let mut planes =
                                    group.iter().map(|&c| &src(usize::from(c))[at.clone()]);
                                if let Some(first) = planes.next() {
                                    sum.copy_from_slice(first);
                                }
                                for x in planes {
                                    sum.iter_mut().zip(x).for_each(|(s, &x)| *s += x);
                                }
                                if k == 0 {
                                    o.iter_mut().zip(&*sum).for_each(|(o, &s)| *o = t.gain * s);
                                } else {
                                    o.iter_mut().zip(&*sum).for_each(|(o, &s)| *o += t.gain * s);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Adds every source of this expression, each at `scale · gain`, to `acc`.
    fn contribute(&self, scale: F, acc: &mut Vec<(usize, F)>) {
        let mut add = |src: usize, gain: F| match acc.iter_mut().find(|(s, _)| *s == src) {
            Some((_, g)) => *g += gain,
            None => acc.push((src, gain)),
        };
        match self {
            Self::Zero => {}
            Self::Copy(c) => add(usize::from(*c), scale),
            Self::Sum(terms) => {
                for t in terms {
                    for &c in &*t.srcs {
                        add(usize::from(c), scale * t.gain);
                    }
                }
            }
        }
    }
}

/// The `(source, gain)` pairs a row sums, unsorted, one per source.
fn row_coefficients<F: Float>(row: &Row<F>) -> Vec<(usize, F)> {
    let mut acc = Vec::new();
    match row {
        Row::One(e) => e.contribute(F::ONE, &mut acc),
        Row::Mix { a, b, scale } => {
            a.contribute(*scale, &mut acc);
            b.contribute(*scale, &mut acc);
        }
    }
    acc
}

/// The expression summing `gain · x[src]` for each pair, one source a term,
/// in the order given: a lone unity pair is a copy, none is silence.
fn expr_of_coefficients<F: Float>(pairs: Vec<(usize, F)>) -> Expr<F> {
    match &*pairs {
        [] => Expr::Zero,
        [(c, g)] if *g == F::ONE => Expr::Copy(narrow(*c)),
        _ => Expr::Sum(
            pairs
                .into_iter()
                .map(|(c, gain)| Term {
                    gain,
                    srcs: Box::new([narrow(c)]),
                })
                .collect(),
        ),
    }
}

fn lane_mut<F>(samples: &mut [F], ch: usize, width: usize) -> impl Iterator<Item = &mut F> {
    samples.iter_mut().skip(ch).step_by(width)
}

/// An expression of exactly `N` terms that each read one source, as
/// `(gain, source)`: the shape of a stereo fold up to 7.1.
fn lone_terms<F: Float, const N: usize>(e: &Expr<F>) -> Option<[(F, u16); N]> {
    match e {
        Expr::Sum(terms) if terms.len() == N && terms.iter().all(|t| t.srcs.len() == 1) => {
            Some(core::array::from_fn(|k| (terms[k].gain, terms[k].srcs[0])))
        }
        _ => None,
    }
}

/// The hot kernel of a mono mix: `(Σ a + Σ b) · scale` over a block, both
/// sums of `N` single-source terms, with the sums in registers so each source
/// is read once. A plain index loop because the term count is a const generic:
/// the loop over terms unrolls and the loop over frames vectorises, which an
/// iterator chain over the stored terms does not. Returns whether the row had
/// this shape.
fn mix_fused<'a, F: Float, const N: usize>(
    a: &Expr<F>,
    b: &Expr<F>,
    src: &impl Fn(usize) -> &'a [F],
    scale: F,
    out: &mut [F],
) -> bool {
    let (Some(ta), Some(tb)) = (lone_terms::<F, N>(a), lone_terms::<F, N>(b)) else {
        return false;
    };
    let n = out.len();
    let plane = |(g, c): (F, u16)| (g, &src(usize::from(c))[..n]);
    let (ta, tb) = (ta.map(plane), tb.map(plane));
    for (i, o) in out.iter_mut().enumerate() {
        let sum = |t: &[(F, &[F]); N]| {
            t.iter()
                .map(|&(g, x)| g * x[i])
                .reduce(|p, q| p + q)
                .unwrap_or(F::ZERO)
        };
        *o = (sum(&ta) + sum(&tb)) * scale;
    }
    true
}

impl<F: Float> Row<F> {
    #[inline]
    fn at(&self, get: &impl Fn(usize) -> F) -> F {
        match self {
            Self::One(e) => e.at(get),
            Self::Mix { a, b, scale } => (a.at(get) + b.at(get)) * *scale,
        }
    }
}

/// How a signal of one [`ChannelTopology`] becomes another's.
///
/// A signal states the [`ChannelTopology`] it has. [`Fold::between`] is built
/// once, on the control side (it allocates), from two topologies, and applied
/// to views by [`fold`](Self::fold) (interleaved to interleaved),
/// [`fold_planar`](Self::fold_planar) (planes to planes), [`fold_planar_to_interleaved`](Self::fold_planar_to_interleaved)
/// and [`fold_planar_channel`](Self::fold_planar_channel) (one destination
/// channel). [`frame`](Self::frame) is the same fold over one frame, on raw
/// slices. None of them allocate, so all are safe on the audio thread. A fold
/// places channels by *speaker*, never by width: a width alone is not a
/// placement (see [`ChannelTopology`]); [`ChannelTopology::default_for`] says what
/// a bare width is taken to mean.
///
/// `F` is the float the gains and the sums are in: `f32` by default, `f64`
/// where the signal is.
///
/// # The rules
///
/// - **Same speakers, any order:** a pure permutation. Gain exactly 1.0, the
///   samples copied bit for bit.
/// - **A speaker in both:** it goes straight across at gain 1.0.
/// - **A speaker the destination lacks** folds into the nearest ones the
///   destination has, at −3 dB ([`MINUS_3_DB`](Float::MINUS_3_DB)), by a fixed table:
///   the centre into front left and right; a side or rear speaker into its
///   counterpart (side for rear and back) or else the front speaker on
///   its side; a top-back speaker into the back, else the side surround, else
///   the front speaker on its side; the top-front centre and the left/right of
///   centre pair into the centre if the destination has one, else the front
///   pair; any other overhead speaker into the front speaker on its side. The
///   LFE is omitted unless the destination has one. This is the ITU-R BS.775 /
///   Dolby consumer downmix for a stereo destination. For any other
///   destination (5.1 to quad, 7.1.4 to 7.1) the same −3 dB table applies and
///   the coefficients and the choice of nearest speaker are this crate's own:
///   no standard fixes them. A side or rear speaker whose counterpart the
///   *source* lacks and the destination has is moved there at gain 1.0 (a pure
///   reposition, so 5.1 to quad keeps its surrounds at full level).
/// - **A lone centre channel** (a mono source) feeds the destination's centre
///   if it has one, else its front left and right, each at gain 1.0.
/// - **A destination with one channel** is a mono mix, whatever it is called:
///   a one-channel source is copied, front left and right alone are averaged
///   (`(L + R) / 2`), and anything else is the stereo fold times −3 dB. The
///   two rules do not meet (`[0.1, 0.2]` gives `0.15`, `[0.1, 0.2, 0.0]` gives
///   `0.212`).
/// - **Upmix zero-fills.** A destination speaker nothing feeds is silent;
///   nothing is invented.
/// - **Unassigned channels** never pass through a surround matrix, and are
///   never padded or dropped by a guess. An unassigned side against any other at
///   the same width is a relabelling, channel `i` to channel `i`; at another
///   width it is [`FoldError::Unassigned`], since a channel that names no
///   speaker says nothing about where it goes. A caller that wants channels by
///   index says so: [`Fold::by_index`] widens and zero-fills, [`Fold::select`]
///   chooses.
/// - **Ambisonic** sets are not speaker feeds. One converts into another
///   by harmonic, by the AmbiX and Furse-Malham conventions: FuMa channels are
///   reordered to ACN; SN3D, N3D, MaxN and FuMa scales are rescaled (N3D is SN3D
///   times `√(2l+1)` for degree `l`; MaxN peaks at one per harmonic; FuMa is
///   MaxN with W at −3 dB); orders above the destination's are dropped and
///   orders it has beyond the source's are zero-filled. A 2D normalization
///   ([`Sn2d`](AmbisonicNormalization::Sn2d),
///   [`N2d`](AmbisonicNormalization::N2d)) converts only to itself, otherwise
///   [`FoldError::PlanarNormalization`]. Speakers to ambisonic or back is
///   [`FoldError::NeedsCodec`].
/// - **Not silent.** A source channel the destination has no place for is
///   [`FoldError::Unplaced`]; an [`Unknown`](Speaker::Unknown) speaker is
///   [`FoldError::Unnamed`] (unless the two topologies are identical, which is
///   an identity).
///
/// The matrix is **not normalised**: full scale on every 5.1 channel gives
/// `1 + 2·`[`MINUS_3_DB`](Float::MINUS_3_DB)` = 2.414` per side. Headroom is
/// the caller's.
///
/// # Summing order
///
/// For the topologies [`Bed::topology`](crate::Bed::topology) gives, a fold to one or two channels sums
/// in a fixed order: the direct channel, then each folded channel in source
/// order, `acc + gain·x`. With overhead speakers in the source (7.1.4) the
/// −3 dB channels of a side are summed first and then scaled once; summing then
/// scaling rounds differently from scaling each channel, so the grouping is
/// kept for exactly the sources that have it. Every applier sums in this order,
/// so a planar fold and a per-frame fold agree bit for bit.
///
/// # Examples
///
/// ```
/// use tutti_buffers::{Float, Fold};
/// use tutti_buffers::Bed;
/// use tutti_units::Real;
///
/// let five_one = Bed::Surround51.topology();
/// let stereo = Bed::Stereo.topology();
/// let fold: Fold = Fold::between(&five_one, &stereo).unwrap();
///
/// // Only the centre active: both sides get it at −3 dB.
/// let mut out = [0.0f32; 2];
/// fold.frame(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], &mut out);
/// assert_eq!(out, [f32::MINUS_3_DB, f32::MINUS_3_DB]);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Fold<F: Float = f32> {
    src: u16,
    rows: Box<[Row<F>]>,
}

/// What a [`Hit`] is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The same speaker on both sides (or a lone mono fan-out), not a fold.
    Direct,
    /// A side or rear speaker moved to its counterpart the source lacks.
    Reposition,
    /// A speaker the destination lacks, folded into the nearest ones.
    Folded,
}

/// One source channel's contribution to one destination channel.
struct Hit<F> {
    src: usize,
    dst: usize,
    gain: F,
    kind: Kind,
}

/// The gains a fold applies where a speaker folds, as the float `F`.
#[derive(Clone, Copy)]
struct Gains<F> {
    centre: F,
    surround: F,
    height: F,
    lfe: Option<F>,
}

impl<F: Float> Gains<F> {
    /// The gain a missing speaker folds at: by the class it belongs to.
    fn of(&self, missing: Speaker) -> F {
        use Speaker::*;
        if is_overhead(missing) {
            self.height
        } else if matches!(
            missing,
            FrontCenter | FrontLeftOfCenter | FrontRightOfCenter
        ) {
            self.centre
        } else {
            self.surround
        }
    }
}

/// Where a speaker the destination lacks folds, in order of preference: the
/// first list whose speakers the destination all has. Every entry is at −3 dB
/// except a pure reposition (see [`Fold`]); the stereo destination uses only
/// the front pair, which is the ITU-R BS.775 / Dolby consumer downmix.
const fn fallbacks(missing: Speaker) -> &'static [&'static [Speaker]] {
    use Speaker::*;
    match missing {
        FrontCenter | TopCenter => &[&[FrontLeft, FrontRight]],
        TopFrontCenter => &[&[FrontCenter], &[FrontLeft, FrontRight]],
        FrontLeftOfCenter => &[&[FrontCenter], &[FrontLeft]],
        FrontRightOfCenter => &[&[FrontCenter], &[FrontRight]],
        TopFrontLeft => &[&[FrontLeft]],
        TopFrontRight => &[&[FrontRight]],
        TopSideLeft => &[&[SideLeft], &[FrontLeft]],
        TopSideRight => &[&[SideRight], &[FrontRight]],
        TopBackLeft => &[&[BackLeft], &[SideLeft], &[FrontLeft]],
        TopBackRight => &[&[BackRight], &[SideRight], &[FrontRight]],
        TopBackCenter => &[
            &[BackCenter],
            &[BackLeft, BackRight],
            &[SideLeft, SideRight],
            &[FrontLeft, FrontRight],
        ],
        SideLeft => &[&[BackLeft], &[FrontLeft]],
        SideRight => &[&[BackRight], &[FrontRight]],
        BackLeft => &[&[SideLeft], &[FrontLeft]],
        BackRight => &[&[SideRight], &[FrontRight]],
        BackCenter => &[
            &[BackLeft, BackRight],
            &[SideLeft, SideRight],
            &[FrontLeft, FrontRight],
        ],
        FrontLeft | FrontRight | LowFrequency | Unknown(_) | Unassigned => &[],
    }
}

const fn is_overhead(sp: Speaker) -> bool {
    use Speaker::*;
    matches!(
        sp,
        TopCenter
            | TopFrontLeft
            | TopFrontCenter
            | TopFrontRight
            | TopBackLeft
            | TopBackCenter
            | TopBackRight
            | TopSideLeft
            | TopSideRight
    )
}

fn index(of: &[Speaker], sp: Speaker) -> Option<usize> {
    of.iter().position(|&x| x == sp)
}

fn narrow(i: usize) -> u16 {
    u16::try_from(i).expect("a topology's channel count fits u16 by construction")
}

/// Every source channel's hits into `dst`, or the first channel with none.
fn hits<F: Float>(
    src: &[Speaker],
    dst: &[Speaker],
    gains: &Gains<F>,
) -> Result<Vec<Hit<F>>, FoldError> {
    let lone_mono = src == [Speaker::FrontCenter];
    src.iter()
        .enumerate()
        .map(|(i, &sp)| {
            if let Some(j) = index(dst, sp) {
                return Ok(vec![Hit {
                    src: i,
                    dst: j,
                    gain: F::ONE,
                    kind: Kind::Direct,
                }]);
            }
            let unplaced = FoldError::Unplaced {
                channel: i,
                speaker: sp,
            };
            if sp.is_lfe() {
                // Omitted unless the caller sends it to the front pair.
                let Some(gain) = gains.lfe else {
                    return Ok(Vec::new());
                };
                return index(dst, Speaker::FrontLeft)
                    .zip(index(dst, Speaker::FrontRight))
                    .map(|(l, r)| {
                        [l, r]
                            .map(|j| Hit {
                                src: i,
                                dst: j,
                                gain,
                                kind: Kind::Folded,
                            })
                            .into()
                    })
                    .ok_or(unplaced);
            }
            if lone_mono {
                return index(dst, Speaker::FrontLeft)
                    .zip(index(dst, Speaker::FrontRight))
                    .map(|(l, r)| {
                        [l, r]
                            .map(|j| Hit {
                                src: i,
                                dst: j,
                                gain: F::ONE,
                                kind: Kind::Direct,
                            })
                            .into()
                    })
                    .ok_or(unplaced);
            }
            fallbacks(sp)
                .iter()
                .find_map(|targets| {
                    targets
                        .iter()
                        .map(|&t| index(dst, t))
                        .collect::<Option<Vec<_>>>()
                })
                .map(|js| {
                    js.into_iter()
                        .map(|j| {
                            let reposition =
                                sp.counterpart() == Some(dst[j]) && index(src, dst[j]).is_none();
                            Hit {
                                src: i,
                                dst: j,
                                gain: if reposition { F::ONE } else { gains.of(sp) },
                                kind: if reposition {
                                    Kind::Reposition
                                } else {
                                    Kind::Folded
                                },
                            }
                        })
                        .collect()
                })
                .ok_or(unplaced)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|v| v.into_iter().flatten().collect())
}

/// The expression of destination channel `j`: the direct channel, then the
/// folded ones in source order; with overhead speakers in the source the ones
/// that fold at one gain are summed first and scaled once.
fn expr<F: Float>(src: &[Speaker], hits: &[Hit<F>], j: usize) -> Expr<F> {
    let mine = || hits.iter().filter(move |h| h.dst == j);
    let single = |h: &Hit<F>| Term {
        gain: h.gain,
        srcs: Box::new([narrow(h.src)]),
    };
    let folded = || mine().filter(|h| h.kind != Kind::Direct);
    let grouped = src.iter().copied().any(is_overhead);
    let (summed, apart): (Vec<&Hit<F>>, Vec<&Hit<F>>) = if grouped {
        folded().partition(|h| h.kind == Kind::Folded)
    } else {
        (Vec::new(), folded().collect())
    };
    // One group per distinct gain, in order of first appearance.
    let mut groups: Vec<Term<F>> = Vec::new();
    for h in summed {
        match groups.iter_mut().find(|t| t.gain == h.gain) {
            Some(t) => {
                let mut srcs = t.srcs.to_vec();
                srcs.push(narrow(h.src));
                t.srcs = srcs.into();
            }
            None => groups.push(Term {
                gain: h.gain,
                srcs: Box::new([narrow(h.src)]),
            }),
        }
    }
    let terms: Vec<Term<F>> = mine()
        .filter(|h| h.kind == Kind::Direct)
        .map(single)
        .chain(apart.into_iter().map(single))
        .chain(groups)
        .collect();
    match &*terms {
        [] => Expr::Zero,
        [Term { gain, srcs }] if *gain == F::ONE && srcs.len() == 1 => Expr::Copy(srcs[0]),
        _ => Expr::Sum(terms.into()),
    }
}

impl<F: Float> Fold<F> {
    /// Builds the fold from `src` into `dst`.
    ///
    /// Control side: it allocates. The rules are on [`Fold`].
    ///
    /// # Errors
    ///
    /// [`FoldError`] when a channel cannot be placed, never a silent drop.
    pub fn between(src: &ChannelTopology, dst: &ChannelTopology) -> Result<Self, FoldError> {
        Self::spec(src, dst).build()
    }

    /// The fold from `src` into `dst` as a spec, to state other downmix levels
    /// than the defaults ([`FoldSpec::levels`]) before it is
    /// [`build`](FoldSpec::build)t. [`between`](Self::between) is the default
    /// spec.
    pub fn spec<'a>(src: &'a ChannelTopology, dst: &'a ChannelTopology) -> FoldSpec<'a, F> {
        FoldSpec::new(src, dst)
    }

    fn build(
        src: &ChannelTopology,
        dst: &ChannelTopology,
        levels: FoldLevels,
    ) -> Result<Self, FoldError> {
        levels.check()?;
        let level = |a: tutti_units::Amplitude| F::from_f64(a.get());
        let gains = Gains {
            centre: level(levels.centre),
            surround: level(levels.surround),
            height: level(levels.height),
            lfe: levels.lfe.map(level),
        };
        let (Some(s), Some(d)) = (src.positions(), dst.positions()) else {
            return Self::between_ambisonic(src, dst);
        };
        if s.is_empty() || d.is_empty() {
            return Err(BufferError::ZeroWidth.into());
        }
        let rows = if s == d {
            (0..s.len())
                .map(|i| Row::One(Expr::Copy(narrow(i))))
                .collect()
        } else if dst.is_unassigned() || s.iter().any(|p| p.is_unassigned()) {
            Self::unassigned(src, dst)?
        } else {
            for (end, side) in [(FoldEnd::Source, s), (FoldEnd::Destination, d)] {
                if let Some(channel) = side.iter().position(|p| matches!(p, Speaker::Unknown(_))) {
                    return Err(FoldError::Unnamed { end, channel });
                }
            }
            if let [_] = d {
                Self::mono(s, &gains)?
            } else {
                Self::placed(s, d, &gains)?
            }
        };
        Ok(Self {
            src: narrow(s.len()),
            rows,
        })
    }

    /// An ambisonic set converts into another by harmonic; anything else is a
    /// codec's.
    fn between_ambisonic(src: &ChannelTopology, dst: &ChannelTopology) -> Result<Self, FoldError> {
        let (Some(s), Some(d)) = (src.ambisonic(), dst.ambisonic()) else {
            return Err(FoldError::NeedsCodec);
        };
        Ok(Self {
            src: s.width().count(),
            rows: ambisonic::rows(s, d)?,
        })
    }

    /// Places the channels of a source by index, for widths that have no order:
    /// widening only.
    ///
    /// Destination channel `i` is source channel `i` at gain 1.0 and the rest of
    /// a wider destination is zero-filled. A destination narrower than the
    /// source would have to drop channels, and a channel that cannot be placed
    /// is an error, never a silent drop: that is [`FoldError::Unplaced`] (the
    /// first channel with no place, carrying [`Speaker::Unassigned`] since a
    /// width names no speaker). A caller who chooses to truncate says so with
    /// [`select`](Self::select).
    ///
    /// # Errors
    ///
    /// [`FoldError::Buffer`] with [`BufferError::ZeroWidth`] for a width of
    /// zero, [`FoldError::Unplaced`] for a narrowing.
    pub fn by_index(src: ChannelLayout, dst: ChannelLayout) -> Result<Self, FoldError> {
        let (s, d) = (src.count(), dst.count());
        if s == 0 || d == 0 {
            return Err(BufferError::ZeroWidth.into());
        }
        if s > d {
            return Err(FoldError::Unplaced {
                channel: usize::from(d),
                speaker: Speaker::Unassigned,
            });
        }
        let rows = (0..d)
            .map(|i| Row::One(if i < s { Expr::Copy(i) } else { Expr::Zero }))
            .collect();
        Ok(Self { src: s, rows })
    }

    /// Chooses source channels for an unassigned destination: destination channel
    /// `i` is source channel `channels[i]`, gain 1.0, bit-exact. A channel may
    /// be chosen twice (one input to both sides); the rest are not read.
    ///
    /// For taking part of a wider source: a stereo pair out of an eight-channel
    /// run. A named source is chosen by index too; use
    /// [`between`](Self::between) when speakers matter.
    ///
    /// # Errors
    ///
    /// [`FoldError::Buffer`] with [`BufferError::ZeroWidth`] for no channels or
    /// an empty source and [`BufferError::Layout`] for more than `u16::MAX`
    /// channels, and [`FoldError::NoSuchChannel`] for a channel at or past
    /// `src`'s width.
    pub fn select(src: ChannelLayout, channels: &[usize]) -> Result<Self, FoldError> {
        if src == ChannelLayout::EMPTY || channels.is_empty() {
            return Err(BufferError::ZeroWidth.into());
        }
        // A list is a destination width, which `narrow` later assumes fits.
        ChannelLayout::try_from(channels.len()).map_err(BufferError::from)?;
        let rows = channels
            .iter()
            .map(|&channel| {
                u16::try_from(channel)
                    .ok()
                    .filter(|&c| c < src.count())
                    .map(|c| Row::One(Expr::Copy(c)))
                    .ok_or(FoldError::NoSuchChannel {
                        channel,
                        width: src,
                    })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            src: src.count(),
            rows,
        })
    }

    /// An unassigned side relabels at equal width, channel `i` to channel `i`,
    /// and is refused at any other.
    fn unassigned(
        src: &ChannelTopology,
        dst: &ChannelTopology,
    ) -> Result<Box<[Row<F>]>, FoldError> {
        if src.width() != dst.width() {
            return Err(FoldError::Unassigned {
                src: src.width(),
                dst: dst.width(),
            });
        }
        Ok((0..src.width().count())
            .map(|i| Row::One(Expr::Copy(i)))
            .collect())
    }

    fn placed(s: &[Speaker], d: &[Speaker], gains: &Gains<F>) -> Result<Box<[Row<F>]>, FoldError> {
        let hits = hits(s, d, gains)?;
        Ok((0..d.len()).map(|j| Row::One(expr(s, &hits, j))).collect())
    }

    fn mono(s: &[Speaker], gains: &Gains<F>) -> Result<Box<[Row<F>]>, FoldError> {
        use Speaker::{FrontLeft, FrontRight};
        let row = match (s, index(s, FrontLeft), index(s, FrontRight)) {
            ([_], ..) => Row::One(Expr::Copy(0)),
            ([_, _], Some(l), Some(r)) => Row::Mix {
                a: Expr::Copy(narrow(l)),
                b: Expr::Copy(narrow(r)),
                scale: F::lit(0.5),
            },
            _ => {
                let pair = [FrontLeft, FrontRight];
                // The LFE joins the mono channel itself, below, not the pair.
                let pair_gains = Gains {
                    lfe: None,
                    ..*gains
                };
                let hits = hits(s, &pair, &pair_gains)?;
                Row::Mix {
                    a: expr(s, &hits, 0),
                    b: expr(s, &hits, 1),
                    scale: F::MINUS_3_DB,
                }
            }
        };
        let row = match gains.lfe {
            // A mono destination takes the LFE at its own gain, the centre.
            Some(g) if s.iter().any(|p| p.is_lfe()) => {
                let mut acc = row_coefficients(&row);
                for (c, _) in s.iter().enumerate().filter(|(_, p)| p.is_lfe()) {
                    acc.push((c, g));
                }
                Row::One(expr_of_coefficients(acc))
            }
            _ => row,
        };
        Ok(Box::new([row]))
    }

    /// Whether every destination channel is the source channel of the same
    /// index, copied: a fold a caller may skip, reading the planes in place.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        usize::from(self.src) == self.rows.len()
            && self
                .rows
                .iter()
                .enumerate()
                .all(|(i, row)| matches!(row, Row::One(Expr::Copy(c)) if usize::from(*c) == i))
    }

    /// The source width this fold reads.
    #[must_use]
    pub fn src_layout(&self) -> ChannelLayout {
        ChannelLayout::from_count(self.src)
    }

    /// The destination width this fold writes.
    #[must_use]
    pub fn dst_layout(&self) -> ChannelLayout {
        ChannelLayout::from_count(narrow(self.rows.len()))
    }

    /// Refuses a view whose width is not the fold's, input then output.
    fn check_widths(&self, src: ChannelLayout, dst: ChannelLayout) -> Result<(), BufferError> {
        if src != self.src_layout() {
            return Err(BufferError::WidthMismatch {
                src,
                dst: self.src_layout(),
            });
        }
        if dst != self.dst_layout() {
            return Err(BufferError::WidthMismatch {
                src: self.dst_layout(),
                dst,
            });
        }
        Ok(())
    }

    /// Folds one frame on raw slices, the per-frame hot path: `src` has
    /// [`src_layout`](Self::src_layout) samples and `dst`
    /// [`dst_layout`](Self::dst_layout), and every `dst` sample is written.
    ///
    /// The one applier that takes slices rather than views, so a caller that
    /// holds a frame in a fixed array pays for no view. Allocation-free.
    ///
    /// The widths are a precondition, debug-asserted and not checked in a
    /// release build: `dst.len()` equals the destination width and `src.len()`
    /// the source width. A `dst` of another width leaves samples partly
    /// unwritten.
    ///
    /// # Panics
    ///
    /// A `src` shorter than the source width panics at the index it reads.
    #[inline]
    pub fn frame(&self, src: &[F], dst: &mut [F]) {
        debug_assert_eq!(src.len(), usize::from(self.src), "source width");
        debug_assert_eq!(dst.len(), self.rows.len(), "destination width");
        // The one hot kernel: a flat zip, no per-channel arithmetic on indices.
        self.rows
            .iter()
            .zip(dst)
            .for_each(|(row, o)| *o = row.at(&|c| src[c]));
    }

    /// Folds every frame of `src` into `dst`.
    ///
    /// Every sample of `dst` is written. Allocation-free.
    ///
    /// # Errors
    ///
    /// [`BufferError::WidthMismatch`] if `src` is not
    /// [`src_layout`](Self::src_layout) wide (`src` is the view's width, `dst`
    /// the fold's) or `dst` is not [`dst_layout`](Self::dst_layout) wide (`src`
    /// is the fold's width, `dst` the view's), and
    /// [`BufferError::LengthMismatch`] if the frame counts differ. Nothing is
    /// written then.
    pub fn fold(
        &self,
        src: Interleaved<'_, F>,
        mut dst: InterleavedMut<'_, F>,
    ) -> Result<(), BufferError> {
        self.check_widths(src.layout(), dst.layout())?;
        same_frames(src.frames(), dst.frames())?;
        for (s, d) in src.iter().zip(dst.iter_mut()) {
            self.frame(s, d);
        }
        Ok(())
    }

    /// Folds every plane of `src` into the planes of `dst`.
    ///
    /// Every sample of `dst` is written. Allocation-free.
    ///
    /// # Errors
    ///
    /// As [`fold`](Self::fold).
    pub fn fold_planar(
        &self,
        src: Planes<'_, F>,
        mut dst: PlanesMut<'_, '_, F>,
    ) -> Result<(), BufferError> {
        self.check_widths(src.layout(), dst.layout())?;
        same_frames(src.frames(), dst.frames())?;
        let read = |c: usize| src.plane(c).samples();
        for (c, mut out) in dst.iter_mut().enumerate() {
            self.planar_row(&read, c, out.samples_mut());
        }
        Ok(())
    }

    /// Folds planar channels into an interleaved block, a chunk of frames at a
    /// time through a stack scratch so each channel runs over whole slices.
    ///
    /// Every sample of `dst` is written. Allocation-free.
    ///
    /// # Errors
    ///
    /// As [`fold`](Self::fold).
    pub fn fold_planar_to_interleaved(
        &self,
        src: Planes<'_, F>,
        mut dst: InterleavedMut<'_, F>,
    ) -> Result<(), BufferError> {
        self.check_widths(src.layout(), dst.layout())?;
        same_frames(src.frames(), dst.frames())?;
        let dw = dst.channels();
        let frames = dst.frames().get();
        let samples = dst.samples_mut();
        let mut scratch = [F::ZERO; CHUNK];
        for start in (0..frames).step_by(CHUNK) {
            let n = CHUNK.min(frames - start);
            let block = &mut samples[start * dw..(start + n) * dw];
            for ch in 0..dw {
                let out = &mut scratch[..n];
                self.planar_row(&|c| &src.plane(c).samples()[start..start + n], ch, out);
                lane_mut(block, ch, dw)
                    .zip(&*out)
                    .for_each(|(o, &x)| *o = x);
            }
        }
        Ok(())
    }

    /// Folds planar channels into destination channel `dst_c`. Every sample of
    /// `out` is the bit-identical value [`frame`](Self::frame) writes.
    /// Allocation-free.
    ///
    /// # Errors
    ///
    /// [`BufferError::WidthMismatch`] if `src` is not
    /// [`src_layout`](Self::src_layout) wide, and
    /// [`BufferError::LengthMismatch`] if `out` and `src` hold different frame
    /// counts. Nothing is written then.
    ///
    /// # Panics
    ///
    /// If `dst_c` is not below the destination width.
    pub fn fold_planar_channel(
        &self,
        src: Planes<'_, F>,
        dst_c: usize,
        mut out: MonoMut<'_, F>,
    ) -> Result<(), BufferError> {
        assert!(
            dst_c < self.rows.len(),
            "destination channel {dst_c} is out of bounds for {} channels",
            self.rows.len()
        );
        if src.layout() != self.src_layout() {
            return Err(BufferError::WidthMismatch {
                src: src.layout(),
                dst: self.src_layout(),
            });
        }
        same_frames(src.frames(), out.frames())?;
        self.planar_row(&|c| src.plane(c).samples(), dst_c, out.samples_mut());
        Ok(())
    }

    /// The `(source channel, gain)` pairs destination channel `dst_c` is the
    /// weighted sum of: `out[dst_c] = Σ gain · src[channel]`, in ascending
    /// source channel, one pair per source channel (a channel the fold reaches
    /// by two paths has them added). A source channel that does not feed
    /// `dst_c` (the LFE a stereo destination drops, a channel a select left
    /// out) is absent, and a silent destination channel has no pairs.
    ///
    /// The fold as a matrix, for a caller that wants each coefficient. It is
    /// the same fold to rounding only: the appliers sum in the fixed order and
    /// grouping documented on [`Fold`] (`(L + R) · ½`, a side's −3 dB channels
    /// summed and then scaled), and a caller summing `gain · x` term by term
    /// rounds differently, by a few ulps. Control side: it allocates.
    ///
    /// ```
    /// use tutti_buffers::{Float, Fold};
    /// use tutti_buffers::Bed;
    /// use tutti_units::Real;
    ///
    /// let five_one = Bed::Surround51.topology();
    /// let stereo = Bed::Stereo.topology();
    /// let fold: Fold = Fold::between(&five_one, &stereo).unwrap();
    /// // Left: the front left, the centre at -3 dB, the left surround at -3 dB;
    /// // the LFE (channel 3) is dropped.
    /// let m = f32::MINUS_3_DB;
    /// assert_eq!(fold.contributions(0), [(0, 1.0), (2, m), (4, m)]);
    /// ```
    ///
    /// # Panics
    ///
    /// If `dst_c` is not below the destination width.
    #[must_use]
    pub fn contributions(&self, dst_c: usize) -> Vec<(usize, F)> {
        let mut acc = row_coefficients(&self.rows[dst_c]);
        acc.sort_unstable_by_key(|&(src, _)| src);
        acc
    }

    /// Destination channel `dst_c` over a block: `src(c)` is source channel
    /// `c`, at least `out.len()` frames long.
    fn planar_row<'a>(&self, src: &impl Fn(usize) -> &'a [F], dst_c: usize, out: &mut [F]) {
        match &self.rows[dst_c] {
            Row::One(e) => e.planar(src, out),
            Row::Mix { a, b, scale }
                if mix_fused::<F, 2>(a, b, src, *scale, out)
                    || mix_fused::<F, 3>(a, b, src, *scale, out)
                    || mix_fused::<F, 4>(a, b, src, *scale, out) => {}
            Row::Mix { a, b, scale } => {
                // Both halves a chunk at a time, `b` in a stack scratch, so
                // every loop runs over whole slices like the other rows.
                let mut half = [F::ZERO; CHUNK];
                for (n, o) in out.chunks_mut(CHUNK).enumerate() {
                    let at = n * CHUNK..n * CHUNK + o.len();
                    let chunk = |c: usize| &src(c)[at.clone()];
                    let half = &mut half[..o.len()];
                    a.planar(&chunk, o);
                    b.planar(&chunk, half);
                    o.iter_mut()
                        .zip(&*half)
                        .for_each(|(o, &b)| *o = (*o + b) * *scale);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
