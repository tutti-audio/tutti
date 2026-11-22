//! [`MidiChannelSet`] and [`MidiGroupSet`]: a subset of the sixteen channels or
//! groups, one bit each. [`FunctionBlockSet`]: a subset of the thirty-two
//! Function Block numbers.

macro_rules! midi_set {
    ($(#[$meta:meta])* $set:ident of $item:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub struct $set(u16);

        impl $set {
            /// No member.
            pub const EMPTY: Self = Self(0);
            /// All sixteen.
            pub const ALL: Self = Self(u16::MAX);

            /// Adds `item`.
            #[inline]
            pub const fn insert(&mut self, item: crate::$item) {
                self.0 |= 1 << item.get();
            }

            /// Removes `item`.
            #[inline]
            pub const fn remove(&mut self, item: crate::$item) {
                self.0 &= !(1 << item.get());
            }

            /// Whether `item` is a member.
            #[inline]
            #[must_use]
            pub const fn contains(self, item: crate::$item) -> bool {
                self.0 >> item.get() & 1 != 0
            }

            /// Whether there is no member.
            #[inline]
            #[must_use]
            pub const fn is_empty(self) -> bool {
                self.0 == 0
            }

            /// The members, lowest first.
            pub fn iter(self) -> impl Iterator<Item = crate::$item> + Clone {
                crate::$item::all().filter(move |&item| self.contains(item))
            }
        }

    };
}

midi_set! {
    /// A set of [`MidiChannel`](crate::MidiChannel)s.
    MidiChannelSet of MidiChannel
}

midi_set! {
    /// A set of [`MidiGroup`](crate::MidiGroup)s.
    MidiGroupSet of MidiGroup
}

/// A set of [`FunctionBlockId`](crate::FunctionBlockId)s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FunctionBlockSet(u32);

impl FunctionBlockSet {
    /// No member.
    pub const EMPTY: Self = Self(0);

    /// Adds `block`.
    #[inline]
    pub const fn insert(&mut self, block: crate::FunctionBlockId) {
        self.0 |= 1 << block.get();
    }

    /// Whether every block numbered below `count` is a member: the blocks an
    /// Endpoint declares are numbered from 0.
    #[must_use]
    pub const fn covers(self, count: crate::FunctionBlockCount) -> bool {
        let n = count.get();
        let below = if n >= 32 { u32::MAX } else { (1u32 << n) - 1 };
        self.0 & below == below
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FunctionBlockCount, FunctionBlockId, MidiChannel, MidiGroup};

    #[test]
    fn a_function_block_set_covers_the_blocks_below_a_count() {
        let mut set = FunctionBlockSet::EMPTY;
        assert!(set.covers(FunctionBlockCount::of(0)));
        assert!(!set.covers(FunctionBlockCount::of(1)));
        set.insert(FunctionBlockId::of(1));
        assert!(!set.covers(FunctionBlockCount::of(2)));
        set.insert(FunctionBlockId::of(0));
        assert!(set.covers(FunctionBlockCount::of(2)));
        assert!(!set.covers(FunctionBlockCount::of(3)));
        for id in 0..32 {
            set.insert(FunctionBlockId::of(id));
        }
        assert!(set.covers(FunctionBlockCount::MAX));
    }

    #[test]
    fn a_set_holds_each_member_once_and_iterates_them_in_order() {
        let mut set = MidiChannelSet::EMPTY;
        assert!(set.is_empty());
        set.insert(MidiChannel::of(0));
        set.insert(MidiChannel::of(15));
        set.insert(MidiChannel::of(7));
        assert!(set.iter().eq([0, 7, 15].map(MidiChannel::of)));
        set.remove(MidiChannel::of(7));
        assert!(!set.contains(MidiChannel::of(7)));
        assert!(set.contains(MidiChannel::of(15)));
        assert_eq!(MidiChannelSet::ALL.iter().count(), 16);
        let mut groups = MidiGroupSet::EMPTY;
        groups.insert(MidiGroup::of(3));
        groups.insert(MidiGroup::of(3));
        assert_eq!(groups.iter().count(), 1);
    }
}
