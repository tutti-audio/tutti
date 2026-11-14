//! [`AmbisonicFormat`]: a spherical-harmonic channel set, as opposed to speaker feeds.

use crate::{ChannelLayout, MAX_CHANNELS};

crate::ranged! {
    /// The order of an ambisonic signal, `0..=3`: zeroth order is one channel
    /// and third order is sixteen, the most a bus holds ([`MAX_CHANNELS`]).
    #[derive(Eq, Hash)]
    #[cfg_attr(
        feature = "serde",
        derive(serde::Serialize, serde::Deserialize),
        serde(try_from = "u8", into = "u8")
    )]
    AmbisonicOrder(u8), AmbisonicOrderOutOfRange, "an ambisonic order within 0..=3",
    |raw| raw <= 3
}

// The cap on the order is the cap on a bus: third order is exactly `MAX_CHANNELS`.
const _: () = assert!(AmbisonicOrder::MAX.channels().count() as usize == MAX_CHANNELS);

impl AmbisonicOrder {
    /// The highest order, third.
    pub const MAX: Self = Self::of(3);

    /// The channels of a full-sphere signal of this order, `(order + 1)²`.
    #[must_use]
    pub const fn channels(self) -> ChannelLayout {
        let n = self.0 as u16 + 1;
        ChannelLayout::from_count(n * n)
    }
}

/// The order in which an ambisonic signal's channels are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Ordering {
    /// Ambisonic channel number, `n = l² + l + m`: the order of AmbiX and of
    /// every order up to the cap.
    Acn,
    /// Furse-Malham, `W X Y Z ...`: a legacy order that exists to third order
    /// only, which every [`AmbisonicOrder`] is.
    FuMa,
}

/// How the channels of an ambisonic signal are scaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Normalization {
    /// Schmidt semi-normalised: AmbiX's scaling.
    Sn3d,
    /// Fully normalised.
    N3d,
    /// Each channel peaks at one.
    MaxN,
    /// Furse-Malham's scaling: [`MaxN`](Self::MaxN) with `W` at -3 dB.
    FuMa,
    /// [`Sn3d`](Self::Sn3d) over the horizontal plane only.
    Sn2d,
    /// [`N3d`](Self::N3d) over the horizontal plane only.
    N2d,
}

/// An ambisonic channel set: its order, channel order and scaling.
///
/// The width is `(order + 1)²`, derived, never stored.
///
/// ```
/// use tutti_types::{AmbisonicFormat, AmbisonicOrder, ChannelLayout};
///
/// let first = AmbisonicFormat::ambix(AmbisonicOrder::of(1));
/// assert_eq!(first.width(), ChannelLayout::QUAD);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AmbisonicFormat {
    order: AmbisonicOrder,
    ordering: Ordering,
    normalization: Normalization,
}

impl AmbisonicFormat {
    /// A format of `order` stored in `ordering` and scaled by `normalization`.
    ///
    /// Every combination is a format: [`Ordering::FuMa`] is defined to third
    /// order, which [`AmbisonicOrder`] caps at, so no order refuses it.
    #[must_use]
    pub const fn new(
        order: AmbisonicOrder,
        ordering: Ordering,
        normalization: Normalization,
    ) -> Self {
        Self {
            order,
            ordering,
            normalization,
        }
    }

    /// AmbiX: ACN order, SN3D scaling.
    #[must_use]
    pub const fn ambix(order: AmbisonicOrder) -> Self {
        Self::new(order, Ordering::Acn, Normalization::Sn3d)
    }

    /// Furse-Malham B-format: FuMa order and scaling.
    #[must_use]
    pub const fn fuma(order: AmbisonicOrder) -> Self {
        Self::new(order, Ordering::FuMa, Normalization::FuMa)
    }

    /// The order.
    #[must_use]
    pub const fn order(&self) -> AmbisonicOrder {
        self.order
    }

    /// The channel order.
    #[must_use]
    pub const fn ordering(&self) -> Ordering {
        self.ordering
    }

    /// The scaling.
    #[must_use]
    pub const fn normalization(&self) -> Normalization {
        self.normalization
    }

    /// The channel count, `(order + 1)²`.
    #[must_use]
    pub const fn width(&self) -> ChannelLayout {
        self.order.channels()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_order_range_is_zero_to_third() {
        tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
            new: AmbisonicOrder::new,
            get: AmbisonicOrder::get,
            of: Some(AmbisonicOrder::of),
            valid: &[0, 1, 2, 3],
            invalid: &[4, 7, 255],
        });
    }

    /// An order `n` signal has `(n + 1)²` spherical harmonics: 1, 4, 9, 16.
    #[test]
    fn the_width_is_the_square_of_order_plus_one() {
        for (order, width) in [(0u8, 1u16), (1, 4), (2, 9), (3, 16)] {
            let f = AmbisonicFormat::ambix(AmbisonicOrder::of(order));
            assert_eq!(f.width().count(), width);
        }
    }

    #[test]
    fn the_two_named_formats_state_their_conventions() {
        let ambix = AmbisonicFormat::ambix(AmbisonicOrder::of(2));
        assert_eq!(ambix.ordering(), Ordering::Acn);
        assert_eq!(ambix.normalization(), Normalization::Sn3d);
        let fuma = AmbisonicFormat::fuma(AmbisonicOrder::of(1));
        assert_eq!(fuma.ordering(), Ordering::FuMa);
        assert_eq!(fuma.normalization(), Normalization::FuMa);
        assert_eq!(fuma.order(), AmbisonicOrder::of(1));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_format_serializes_with_its_order_as_a_bare_number_and_refuses_a_fourth() {
        let f = AmbisonicFormat::ambix(AmbisonicOrder::of(3));
        tutti_fixtures::check_round_trip(
            &f,
            r#"{"order":3,"ordering":"Acn","normalization":"Sn3d"}"#,
        );
        assert!(
            serde_json::from_str::<AmbisonicFormat>(
                r#"{"order":4,"ordering":"Acn","normalization":"Sn3d"}"#
            )
            .is_err()
        );
    }
}
