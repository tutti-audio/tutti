//! [`ChannelLayout`] — the engine's one answer to "mono, stereo, or how many?".
//!
//! A newtype over the count rather than an enum of named widths, so there is
//! exactly one way to spell each width: two layouts of the same width are
//! always the same value, hash the same, and match the same arm. `from_count`
//! stays as the `const` primitive the `From` impls delegate to, because
//! `From::from` cannot be `const`. Every `u16` is a width, so there is no
//! `new` returning `Option`; a wider integer converts through a refusing
//! `TryFrom`.

use tutti_units::Samples;

/// The most channels one bus, the root, a sampler voice or a speaker layout holds: sixteen,
/// which is a 7.1.4 bed with room to spare and a third-order ambisonic signal exactly.
///
/// A width past it is refused where it is stated, never truncated.
pub const MAX_CHANNELS: usize = 16;

tutti_units::ranged! {
    /// The index of one channel of a layout, `0..u16::MAX`: any channel a [`ChannelLayout`]
    /// can have, the widest holding `u16::MAX` channels. The range is static; whether a
    /// channel is in a given layout is the layout's check, [`ChannelLayout::index`]. A bus is
    /// narrower ([`MAX_CHANNELS`]), and bus-limited code refuses a width past it where the
    /// width is stated, not through this type.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    ChannelIndex(usize), ChannelIndexOutOfRange, "a channel index below u16::MAX",
    |raw| raw < u16::MAX as usize
}

/// How many audio channels a signal, node port, bus, wave, or device carries.
///
/// The shared "mono vs stereo vs N" vocabulary across the whole engine, in
/// place of a private enum or a bare `channels: usize` per subsystem. Read the
/// count back with [`count`](Self::count).
///
/// It is a count, not a placement: a surround bus is a width of 6, with no
/// statement about which channel is front-left or LFE. That is
/// [`ChannelTopology`](crate::ChannelTopology)'s job. Foreign layout types that
/// carry placement (VST3 `SpeakerArrangement`, CLAP port tags) convert to this
/// at their crate boundary.
///
/// # Constructing one
///
/// A named constant if the width has a name ([`MONO`](Self::MONO),
/// [`STEREO`](Self::STEREO), [`QUAD`](Self::QUAD), [`EMPTY`](Self::EMPTY)),
/// [`From`]/[`Into`] from a `u8` or `u16` otherwise, and [`TryFrom`] from a
/// `u32` or `usize`, which refuses a count above `u16::MAX` with
/// [`LayoutError`]. 5.1 and 7.1 get no names, because a name would
/// imply a speaker *order* this count-only type does not carry.
///
/// Ordering is by channel count, so `a > b` reads as "a is wider than b".
///
/// # No `Default`
///
/// Nothing about a width has a neutral value (stereo is a guess, and `EMPTY`
/// is a different guess), so a struct holding one names the width it means:
///
/// ```compile_fail
/// # use tutti_buffers::ChannelLayout;
/// // A width is always chosen, never defaulted.
/// let _ = ChannelLayout::default();
/// ```
///
/// # Examples
///
/// ```
/// use tutti_buffers::ChannelLayout;
///
/// let surround = ChannelLayout::from(6u16);
/// assert_eq!(surround.count(), 6);
/// assert_eq!(ChannelLayout::try_from(6usize), Ok(surround));
/// assert!(ChannelLayout::try_from(65_538usize).is_err());
/// assert!(surround > ChannelLayout::STEREO);
/// assert!(ChannelLayout::STEREO.is_multi());
/// assert!(!ChannelLayout::EMPTY.is_mono() && !ChannelLayout::EMPTY.is_multi());
/// assert_eq!(format!("{:?}", ChannelLayout::try_from(2u32).unwrap()), "Stereo");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChannelLayout(u16);

/// Names the common widths, so a diagnostic reads `Stereo` rather than
/// `ChannelLayout(2)`.
///
/// Hand-written rather than derived: the derive on a newtype prints the wrapper
/// and the number, and error messages depend on the name. "cannot record
/// Stereo into 6 channels" is the line an author has to act on.
impl core::fmt::Debug for ChannelLayout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            0 => f.write_str("Empty"),
            1 => f.write_str("Mono"),
            2 => f.write_str("Stereo"),
            4 => f.write_str("Quad"),
            n => write!(f, "{n} channels"),
        }
    }
}

impl ChannelLayout {
    /// No channels at all — the empty bus.
    ///
    /// Neither mono nor multi: both predicates report `false`, which is what a
    /// caller branching on "is there a channel to read?" needs.
    pub const EMPTY: Self = Self(0);

    /// One channel.
    pub const MONO: Self = Self(1);

    /// Two channels.
    pub const STEREO: Self = Self(2);

    /// Four channels (quad / ambisonic B-format). Named because it's the one
    /// surround width common enough — and unambiguous enough (4 is 4, no
    /// placement question) — to earn a name.
    pub const QUAD: Self = Self(4);

    /// `channel` as an index into this layout, or `None` when the layout has no such channel
    /// (an index at or past [`count`](Self::count)): a channel is refused, never wrapped or
    /// truncated.
    #[must_use]
    pub fn index(self, channel: usize) -> Option<ChannelIndex> {
        ChannelIndex::new(channel).filter(|i| i.get() < usize::from(self.0))
    }

    /// Returns the channel count this layout describes.
    #[must_use]
    pub const fn count(self) -> u16 {
        self.0
    }

    /// Returns how many **samples** `frames` occupy in a flat interleaved
    /// buffer this layout wide: `frames × channels`.
    ///
    /// The one sanctioned way from a frame count to a slice length.
    ///
    /// Returns a bare `usize` on purpose: the result indexes a `&[f32]`, and a
    /// slice has no other unit. It is the *only* place on the
    /// `Source` / `Sink` edge
    /// where a count leaves [`Samples`], so a stride mistake has one site to live
    /// in rather than one per caller.
    ///
    /// Saturating, like `Samples`' `Mul` impl: an overflowing length can only come
    /// from a corrupt count, and slicing with `usize::MAX` panics at the slice
    /// rather than wrapping to a short, plausible-looking window.
    #[inline]
    #[must_use]
    pub const fn interleaved_len(self, frames: Samples) -> usize {
        frames.get().saturating_mul(self.0 as usize)
    }

    /// Returns how many **whole frames** a flat interleaved buffer of `len`
    /// samples holds at this layout's width: `len / channels`, the inverse of
    /// [`interleaved_len`](Self::interleaved_len).
    ///
    /// A trailing partial frame is **not** counted: a short frame is not a
    /// frame, and writing one desynchronises the interleave for everything
    /// after it.
    ///
    /// A zero-width layout holds no frames, so [`EMPTY`](Self::EMPTY) gives
    /// [`Samples::ZERO`] rather than a divide-by-zero panic. That is the answer
    /// a pump over an empty bus wants: nothing to move.
    #[inline]
    #[must_use]
    pub const fn from_interleaved_len(self, len: usize) -> Samples {
        match self.0 {
            0 => Samples::ZERO,
            ch => Samples::new(len / ch as usize),
        }
    }

    /// Returns whether this layout carries exactly one channel.
    ///
    /// The canonical "is there a second channel to read?" test, for the very
    /// common node shape that takes `left` and falls back to `left` for `right`
    /// when the input is mono. [`EMPTY`](Self::EMPTY) is not mono.
    #[must_use]
    pub const fn is_mono(self) -> bool {
        self.0 == 1
    }

    /// Returns whether this layout carries two or more channels — the negation of
    /// [`is_mono`](Self::is_mono) for any non-empty layout.
    ///
    /// [`EMPTY`](Self::EMPTY) is neither mono nor multi: it has no channels at
    /// all. Both predicates report `false` for it.
    #[must_use]
    pub const fn is_multi(self) -> bool {
        self.0 > 1
    }

    /// Creates a layout from an unsigned count, saturating at `u16::MAX`.
    ///
    /// Named saturation, for a boundary where refusing is wrong and no error
    /// path exists: a count past `u16::MAX` is not a width, so it saturates
    /// (and any per-site cap then refuses it) instead of wrapping into a small
    /// width as `as u16` does. Everywhere else use [`TryFrom`].
    #[must_use]
    pub fn clamped(n: usize) -> Self {
        Self(u16::try_from(n).unwrap_or(u16::MAX))
    }

    /// Creates a layout from a channel count, in `const` context.
    ///
    /// **Prefer [`From`]/[`Into`] at call sites** — `ChannelLayout::from(n)` or
    /// `n.into()`. This exists because `From::from` cannot be `const`, so a
    /// `const` item or an associated constant has no other way to spell a width
    /// that isn't one of the named ones. The `From` impls delegate here.
    #[must_use]
    pub const fn from_count(n: u16) -> Self {
        Self(n)
    }
}

/// Why a [`ChannelLayout`] was not built from a count.
///
/// Zero is not among the reasons: [`ChannelLayout::EMPTY`] is a layout. A
/// caller that needs at least one channel refuses it with its own error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LayoutError {
    /// The count is above `u16::MAX`. Carries the refused count.
    #[error("{0} channels do not fit a layout (at most {max})", max = u16::MAX)]
    TooWide(u64),
}

/// Build a layout from a width that cannot be too large: `u8` and `u16`.
impl From<u8> for ChannelLayout {
    fn from(n: u8) -> Self {
        Self::from_count(u16::from(n))
    }
}

impl From<u16> for ChannelLayout {
    fn from(n: u16) -> Self {
        Self::from_count(n)
    }
}

/// Build a layout from a width an upstream API reports in a wider integer:
/// `u32` (CoreAudio's `mChannelsPerFrame`) and `usize` (`Vec::len`, VST3's bus
/// counts). A count above `u16::MAX` is refused rather than truncated, so a
/// plugin that reports 65 538 channels is an error, not a stereo bus.
///
/// There is deliberately **no reverse `From`**: `ChannelLayout -> u8` would
/// truncate above 255 channels. Read the width with
/// [`count`](ChannelLayout::count).
macro_rules! count_try_from {
    ($($t:ty),*) => {$(
        impl TryFrom<$t> for ChannelLayout {
            type Error = LayoutError;

            fn try_from(n: $t) -> Result<Self, Self::Error> {
                u16::try_from(n).map(Self::from_count).map_err(|_| {
                    // `u64` holds every `u32` and `usize` here.
                    LayoutError::TooWide(u64::try_from(n).unwrap_or(u64::MAX))
                })
            }
        }
    )*};
}

count_try_from!(u32, usize);

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames <-> interleaved length, checked at six channels, where confusing
    /// the two is a 6x error rather than a 2x one that can hide in a round
    /// number.
    #[test]
    fn interleaved_len_is_frames_times_width() {
        let six = ChannelLayout::from(6u16);
        assert_eq!(six.interleaved_len(Samples::new(10)), 60);
        assert_eq!(six.from_interleaved_len(60), Samples::new(10));
        assert_eq!(ChannelLayout::STEREO.interleaved_len(Samples::new(3)), 6);

        // A trailing partial frame is not a frame.
        assert_eq!(six.from_interleaved_len(65), Samples::new(10));
        assert_eq!(
            ChannelLayout::STEREO.from_interleaved_len(7),
            Samples::new(3)
        );
        // Round trip from the frame side is exact.
        for n in [0usize, 1, 7, 512] {
            let f = Samples::new(n);
            assert_eq!(six.from_interleaved_len(six.interleaved_len(f)), f);
        }
    }

    /// A zero-width buffer holds no frames, and says so rather than dividing
    /// by zero.
    #[test]
    fn an_empty_layout_holds_no_frames() {
        assert_eq!(ChannelLayout::EMPTY.from_interleaved_len(64), Samples::ZERO);
        assert_eq!(ChannelLayout::EMPTY.interleaved_len(Samples::new(64)), 0);
    }

    /// Every count has exactly one representation, so equal counts are always
    /// equal values — including for the widths that have names, and whichever
    /// of the two constructors built them.
    #[test]
    fn construction_is_canonical() {
        assert_eq!(ChannelLayout::from(1u16), ChannelLayout::MONO);
        assert_eq!(ChannelLayout::from(2u16), ChannelLayout::STEREO);
        assert_eq!(ChannelLayout::from(4u16), ChannelLayout::QUAD);
        assert_eq!(ChannelLayout::from(0u16), ChannelLayout::EMPTY);
        assert_eq!(ChannelLayout::from(6u16).count(), 6);

        // The `const` primitive and the `From` impls must not diverge — the
        // latter delegate to the former, and call sites mix both.
        for n in 0..=12u16 {
            assert_eq!(ChannelLayout::from_count(n), ChannelLayout::from(n));
        }
    }

    /// The lossless widths convert infallibly and the wide ones through
    /// `TryFrom`, to the same value.
    #[test]
    fn every_integer_width_converts() {
        assert_eq!(ChannelLayout::from(2u8), ChannelLayout::STEREO);
        assert_eq!(ChannelLayout::from(2u16), ChannelLayout::STEREO);
        assert_eq!(ChannelLayout::try_from(2u32), Ok(ChannelLayout::STEREO));
        assert_eq!(ChannelLayout::try_from(2usize), Ok(ChannelLayout::STEREO));
        assert_eq!(
            ChannelLayout::try_from(usize::from(u16::MAX)).map(ChannelLayout::count),
            Ok(u16::MAX)
        );
        for n in 0..=u8::MAX {
            assert_eq!(ChannelLayout::from(n).count(), u16::from(n));
        }
        let inferred: ChannelLayout = 6u16.into();
        assert_eq!(inferred.count(), 6);
    }

    /// A count past `u16::MAX` is an error carrying the refused count, never a
    /// narrower layout.
    #[test]
    fn a_count_beyond_u16_is_refused_not_truncated() {
        for raw in [65_536usize, 65_537, 65_538, 70_000] {
            assert_eq!(
                ChannelLayout::try_from(raw),
                Err(LayoutError::TooWide(raw as u64))
            );
        }
        assert_eq!(
            ChannelLayout::try_from(usize::from(u16::MAX)),
            Ok(ChannelLayout::from(u16::MAX))
        );
        assert_eq!(
            ChannelLayout::try_from(70_000u32),
            Err(LayoutError::TooWide(70_000))
        );
        assert_eq!(
            ChannelLayout::try_from(u32::MAX),
            Err(LayoutError::TooWide(u64::from(u32::MAX)))
        );
        assert!(
            LayoutError::TooWide(65_536)
                .to_string()
                .contains("65536 channels")
        );
    }

    /// `clamped` saturates at `u16::MAX` instead of wrapping.
    #[test]
    fn clamped_saturates() {
        assert_eq!(ChannelLayout::clamped(2), ChannelLayout::STEREO);
        assert_eq!(ChannelLayout::clamped(65_535).count(), u16::MAX);
        assert_eq!(ChannelLayout::clamped(65_536).count(), u16::MAX);
        assert_eq!(ChannelLayout::clamped(65_538).count(), u16::MAX);
        assert_eq!(ChannelLayout::clamped(usize::MAX).count(), u16::MAX);
    }

    /// The named constants report the widths their names claim.
    #[test]
    fn the_named_widths_have_the_counts_they_say() {
        assert_eq!(ChannelLayout::EMPTY.count(), 0);
        assert_eq!(ChannelLayout::MONO.count(), 1);
        assert_eq!(ChannelLayout::STEREO.count(), 2);
        assert_eq!(ChannelLayout::QUAD.count(), 4);
    }

    #[test]
    fn is_mono_is_true_for_exactly_one_channel() {
        assert!(ChannelLayout::MONO.is_mono());
        assert!(!ChannelLayout::STEREO.is_mono());
        assert!(!ChannelLayout::QUAD.is_mono());
        assert!(!ChannelLayout::from_count(6).is_mono());
    }

    /// An empty layout has no second channel to read, and no first one either:
    /// both predicates must reject it.
    #[test]
    fn an_empty_layout_is_neither_mono_nor_multi() {
        let empty = ChannelLayout::EMPTY;
        assert_eq!(empty.count(), 0);
        assert!(!empty.is_multi(), "the empty layout has no second channel");
        assert!(!empty.is_mono(), "the empty layout has no channels at all");
    }

    /// For every non-empty width the two predicates partition exactly.
    #[test]
    fn the_predicates_partition_every_non_empty_width() {
        for n in 1..=12u16 {
            let layout = ChannelLayout::from_count(n);
            assert_ne!(
                layout.is_mono(),
                layout.is_multi(),
                "width {n} must be exactly one of mono/multi"
            );
        }
    }

    /// `Debug` names the common widths.
    ///
    /// Load-bearing, not cosmetic: width-mismatch errors (`PumpLoop::new`)
    /// `Debug`-format both layouts, and a derived newtype `Debug` would print
    /// `ChannelLayout(2)`.
    #[test]
    fn debug_names_the_common_widths() {
        assert_eq!(format!("{:?}", ChannelLayout::EMPTY), "Empty");
        assert_eq!(format!("{:?}", ChannelLayout::MONO), "Mono");
        assert_eq!(format!("{:?}", ChannelLayout::STEREO), "Stereo");
        assert_eq!(format!("{:?}", ChannelLayout::QUAD), "Quad");
        assert_eq!(format!("{:?}", ChannelLayout::from_count(6)), "6 channels");
    }

    /// Ordering is by width.
    #[test]
    fn ordering_is_by_channel_count() {
        assert!(ChannelLayout::MONO < ChannelLayout::STEREO);
        assert!(ChannelLayout::STEREO < ChannelLayout::QUAD);
        assert!(ChannelLayout::QUAD < ChannelLayout::from_count(6));
        assert!(ChannelLayout::EMPTY < ChannelLayout::MONO);

        let mut widths = [
            ChannelLayout::from_count(6),
            ChannelLayout::MONO,
            ChannelLayout::QUAD,
            ChannelLayout::STEREO,
        ];
        widths.sort();
        assert_eq!(
            widths.map(ChannelLayout::count),
            [1, 2, 4, 6],
            "sorting layouts must order them by width"
        );
    }

    /// A channel index is below `u16::MAX`, past the widest layout's last channel: `new`,
    /// `TryFrom` and `of` agree.
    #[test]
    fn a_channel_index_is_below_the_widest_layouts_count() {
        tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
            new: ChannelIndex::new,
            get: ChannelIndex::get,
            of: Some(ChannelIndex::of),
            valid: &[0, 1, MAX_CHANNELS, usize::from(u16::MAX) - 1],
            invalid: &[usize::from(u16::MAX), usize::MAX],
        });
    }

    /// A layout names the channels below its count and refuses the rest, including a channel
    /// the static range admits; a layout wider than a bus names all its channels.
    #[test]
    fn a_layout_indexes_only_the_channels_it_has() {
        let stereo = ChannelLayout::STEREO;
        assert_eq!(stereo.index(1).map(ChannelIndex::get), Some(1));
        assert_eq!(stereo.index(2), None);
        assert_eq!(stereo.index(usize::MAX), None);
        assert_eq!(ChannelLayout::EMPTY.index(0), None);
        let wide = ChannelLayout::from_count(24);
        assert_eq!(wide.index(23).map(ChannelIndex::get), Some(23));
        assert_eq!(wide.index(24), None);
    }
}
