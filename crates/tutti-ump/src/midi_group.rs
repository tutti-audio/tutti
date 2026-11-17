//! [`MidiGroup`]: which of a UMP Endpoint's 16 groups a packet travels on.
//!
//! An address, not a quantity. The Group field is present in every UMP packet
//! except Utility (type `0x0`) and UMP Stream (type `0xF`) (M2-104 §2.1.2); it
//! is transport addressing, where a [`MidiChannel`](crate::MidiChannel) is
//! voice addressing within a group. Both are 4 bits, and they are distinct
//! types so that a transposed pair does not compile:
//!
//! ```compile_fail
//! # use tutti_ump::{MidiChannel, MidiGroup};
//! fn takes(_group: MidiGroup, _channel: MidiChannel) {}
//! takes(MidiChannel::of(3), MidiGroup::of(1));
//! ```

use crate::OutOfRange;

/// One of the 16 groups (virtual cables) in a UMP Endpoint.
///
/// Always in `0..=15`: [`new`](Self::new) refuses a larger number, and
/// [`masked`](Self::masked) is the one constructor that masks.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
// Serialized as the bare number, and read back through `TryFrom<u8>`, so a
// document cannot carry a value the type refuses.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub struct MidiGroup(u8);

impl MidiGroup {
    /// The first group — and the only one a single-cable endpoint uses.
    ///
    /// This is the overwhelmingly common value: an engine that is not
    /// multiplexing several virtual cables onto one endpoint puts everything
    /// here, which is why [`Default`] resolves to it.
    pub const FIRST: MidiGroup = MidiGroup(0);
    /// The last group.
    pub const LAST: MidiGroup = MidiGroup(15);
    /// How many groups a UMP Endpoint carries.
    pub const COUNT: u8 = 16;

    /// The group, or `None` when `raw` is above 15.
    ///
    /// ```
    /// use tutti_ump::MidiGroup;
    ///
    /// assert_eq!(MidiGroup::new(15), Some(MidiGroup::LAST));
    /// assert_eq!(MidiGroup::new(16), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new(raw: u8) -> Option<MidiGroup> {
        if raw < Self::COUNT {
            Some(MidiGroup(raw))
        } else {
            None
        }
    }

    /// The group for a literal or a constant. For a value that arrives from
    /// outside, use [`new`](Self::new).
    ///
    /// # Panics
    ///
    /// When `raw` is above 15; in a `const` that is a compile error.
    #[inline]
    #[must_use]
    #[track_caller]
    pub const fn of(raw: u8) -> MidiGroup {
        match Self::new(raw) {
            Some(group) => group,
            None => panic!("a group is 0 to 15"),
        }
    }

    /// The low four bits of `raw`: the one place that masks. Use it where the
    /// bits are known to be a Group field, as the decoder does; a value from
    /// outside the wire is checked with [`new`](Self::new).
    #[inline]
    #[must_use]
    pub const fn masked(raw: u8) -> MidiGroup {
        MidiGroup(raw & 0x0F)
    }

    /// The channel or group as an array index, `0..16`: the lane of a table
    /// with one slot for each of the groups.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// All sixteen, in order.
    ///
    /// ```
    /// use tutti_ump::MidiGroup;
    ///
    /// assert_eq!(MidiGroup::all().count(), 16);
    /// assert!(MidiGroup::all().map(MidiGroup::index).eq(0..16));
    /// ```
    #[inline]
    pub fn all() -> impl ExactSizeIterator<Item = MidiGroup> + DoubleEndedIterator + Clone {
        (0..Self::COUNT).map(MidiGroup)
    }

    /// The group a person numbers `number` (1 to 16: hardware and DAW UIs show
    /// the first as 1), or `None` outside that. The 1-based door; the wire's
    /// number is [`new`](Self::new)'s. [`Display`](core::fmt::Display) writes
    /// the same number back.
    ///
    /// ```
    /// use tutti_ump::MidiGroup;
    ///
    /// assert_eq!(MidiGroup::numbered(10), Some(MidiGroup::of(9)));
    /// assert_eq!(MidiGroup::numbered(0), None);
    /// assert_eq!(MidiGroup::numbered(17), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn numbered(number: u8) -> Option<MidiGroup> {
        match number.checked_sub(1) {
            Some(raw) => Self::new(raw),
            None => None,
        }
    }

    /// Returns the raw 0-based group number, for a UMP constructor.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// The 1-based number a person reads ([`numbered`](MidiGroup::numbered)'s): `MidiGroup::of(9)` is
/// `10`. [`Debug`](core::fmt::Debug) keeps the wire number.
impl core::fmt::Display for MidiGroup {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&(u16::from(self.0) + 1), f)
    }
}

impl TryFrom<u8> for MidiGroup {
    type Error = OutOfRange;

    #[inline]
    fn try_from(raw: u8) -> Result<Self, OutOfRange> {
        Self::new(raw).ok_or(OutOfRange {
            what: "group",
            value: u32::from(raw),
            max: u32::from(Self::LAST.0),
        })
    }
}

impl From<MidiGroup> for u8 {
    #[inline]
    fn from(g: MidiGroup) -> u8 {
        g.0
    }
}

// Deliberately no infallible `From<u8> for MidiGroup`, and no conversion to or from
// `MidiChannel` in either direction. The `u8` case is `MidiChannel`'s
// reasoning: the conversion would mask or fail, and a lossy conversion inside
// a trait that promises not to lose data is what `ChannelLayout` refused a
// reverse `From` for. The `MidiChannel` case is stronger: a total, lossless `From` between
// them would compile, and would silently reinstate exactly the transposition
// this type exists to reject.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_from_agrees_with_new() {
        for raw in 0..=255u8 {
            assert_eq!(MidiGroup::try_from(raw).ok(), MidiGroup::new(raw));
        }
        assert_eq!(
            MidiGroup::try_from(16),
            Err(OutOfRange {
                what: "group",
                value: 16,
                max: 15
            })
        );
    }

    #[test]
    fn a_group_out_of_range_is_refused() {
        for raw in 0..=255u8 {
            match MidiGroup::new(raw) {
                Some(group) => {
                    assert!(raw < 16);
                    assert_eq!(group.get(), raw);
                    assert_eq!(MidiGroup::of(raw), group);
                }
                None => assert!(raw >= 16),
            }
        }
    }

    #[test]
    fn masking_is_only_through_masked() {
        assert_eq!(MidiGroup::masked(16).get(), 0);
        assert_eq!(MidiGroup::masked(255).get(), 15);
        assert_eq!(MidiGroup::masked(7), MidiGroup::of(7));
    }

    #[test]
    #[should_panic(expected = "a group is 0 to 15")]
    fn of_panics_out_of_range() {
        let _ = MidiGroup::of(16);
    }

    #[test]
    fn a_group_and_a_channel_are_indistinguishable_by_value() {
        use crate::MidiChannel;
        // This is the premise that makes the separate type necessary, not a
        // property of it: the two fields have identical width, identical
        // masking, and identical range, so *nothing in the value* can tell a
        // transposed argument from a correct one. Only the type system can.
        // The compile-time half of this is the `compile_fail` doctest in the
        // module docs above, which a `#[test]` cannot express.
        assert_eq!(MidiGroup::of(7).get(), MidiChannel::of(7).get());
        assert_eq!(MidiGroup::new(16), None);
        assert_eq!(MidiChannel::new(16), None);
        assert_eq!(MidiGroup::COUNT, MidiChannel::COUNT);
    }

    #[test]
    fn numbered_is_one_based_and_refuses_outside() {
        for n in 0..=255u8 {
            match MidiGroup::numbered(n) {
                Some(v) => {
                    assert!((1..=16).contains(&n));
                    assert_eq!(v.get(), n - 1);
                    assert_eq!(v.to_string(), n.to_string());
                }
                None => assert!(n == 0 || n > 16),
            }
        }
    }

    #[test]
    fn display_is_numbered_and_debug_is_the_wire() {
        assert_eq!(MidiGroup::of(9).to_string(), "10");
        assert_eq!(format!("{:?}", MidiGroup::of(9)), "MidiGroup(9)");
    }

    #[test]
    fn all_is_every_value_in_order_and_index_is_its_position() {
        let all: Vec<_> = MidiGroup::all().collect();
        assert_eq!(all.len(), usize::from(MidiGroup::COUNT));
        for (i, v) in all.iter().enumerate() {
            assert_eq!(v.index(), i);
            assert_eq!(usize::from(v.get()), i);
        }
    }
}
