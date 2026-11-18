//! [`MidiChannel`]: which of a group's 16 channels a message addresses.
//!
//! An address, not a quantity. A channel names a voice slot (under MPE a note
//! is addressed by `NoteId::from_channel_note(channel, note)`), so the channel
//! of a note is part of its identity. It is a distinct type from
//! [`MidiGroup`](crate::MidiGroup): both are 4 bits, and a transposed
//! `(group, channel)` pair does not compile.

tutti_units::ranged! {
    /// One of the 16 channels within a MIDI group.
    ///
    /// Always in `0..=MAX`: [`new`](Self::new) refuses a larger number, and
    /// [`masked`](Self::masked) is the one constructor that masks. Serialized
    /// as the bare number, read back through `TryFrom<u8>`, so a document
    /// cannot carry a value the type refuses.
    #[repr(transparent)]
    #[derive(Default, Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    MidiChannel(u8), MidiChannelOutOfRange, bits 4
}

impl MidiChannel {
    /// The first channel — and the one a single-timbral instrument uses.
    pub const FIRST: MidiChannel = MidiChannel(0);
    /// The last channel. (Channel "16" in the 1-based numbering hardware shows.)
    pub const LAST: MidiChannel = MidiChannel(15);
    /// How many channels a MIDI group carries.
    pub const COUNT: u8 = 16;

    /// The channel or group as an array index, `0..16`: the lane of a table
    /// with one slot for each of the channels.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// All sixteen, in order.
    ///
    /// ```
    /// use tutti_ump::MidiChannel;
    ///
    /// assert_eq!(MidiChannel::all().count(), 16);
    /// assert!(MidiChannel::all().map(MidiChannel::index).eq(0..16));
    /// ```
    #[inline]
    pub fn all() -> impl ExactSizeIterator<Item = MidiChannel> + DoubleEndedIterator + Clone {
        (0..Self::COUNT).map(MidiChannel)
    }

    /// The channel a person numbers `number` (1 to 16: hardware and DAW UIs show
    /// the first as 1), or `None` outside that. The 1-based door; the wire's
    /// number is [`new`](Self::new)'s. [`Display`](core::fmt::Display) writes
    /// the same number back.
    ///
    /// ```
    /// use tutti_ump::MidiChannel;
    ///
    /// assert_eq!(MidiChannel::numbered(10), Some(MidiChannel::of(9)));
    /// assert_eq!(MidiChannel::numbered(0), None);
    /// assert_eq!(MidiChannel::numbered(17), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn numbered(number: u8) -> Option<MidiChannel> {
        match number.checked_sub(1) {
            Some(raw) => Self::new(raw),
            None => None,
        }
    }
}

/// The 1-based number a person reads ([`numbered`](MidiChannel::numbered)'s): `MidiChannel::of(9)` is
/// `10`. [`Debug`](core::fmt::Debug) keeps the wire number.
impl core::fmt::Display for MidiChannel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&(u16::from(self.0) + 1), f)
    }
}

// Deliberately no infallible `From<u8> for MidiChannel`: the conversion would mask or
// fail, and a lossy conversion inside a trait that promises not to lose data
// is exactly what `ChannelLayout` refused a reverse `From` for.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_from_agrees_with_new() {
        for raw in 0..=255u8 {
            assert_eq!(MidiChannel::try_from(raw).ok(), MidiChannel::new(raw));
        }
        assert_eq!(MidiChannel::try_from(16), Err(MidiChannelOutOfRange(16)));
    }

    #[test]
    fn a_channel_out_of_range_is_refused() {
        for raw in 0..=255u8 {
            match MidiChannel::new(raw) {
                Some(channel) => {
                    assert!(raw < 16);
                    assert_eq!(channel.get(), raw);
                    assert_eq!(MidiChannel::of(raw), channel);
                }
                None => assert!(raw >= 16),
            }
        }
    }

    #[test]
    fn masking_is_only_through_masked() {
        assert_eq!(MidiChannel::masked(16).get(), 0);
        assert_eq!(MidiChannel::masked(255).get(), 15);
        assert_eq!(MidiChannel::masked(9), MidiChannel::of(9));
    }

    #[test]
    #[should_panic(expected = "not within 4 bits")]
    fn of_panics_out_of_range() {
        let _ = MidiChannel::of(16);
    }

    #[test]
    fn the_display_number_is_one_based() {
        assert_eq!(MidiChannel::FIRST.to_string(), "1");
        assert_eq!(MidiChannel::LAST.to_string(), "16");
    }

    #[test]
    fn numbered_is_one_based_and_refuses_outside() {
        for n in 0..=255u8 {
            match MidiChannel::numbered(n) {
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
        assert_eq!(MidiChannel::of(9).to_string(), "10");
        assert_eq!(format!("{:?}", MidiChannel::of(9)), "MidiChannel(9)");
    }

    #[test]
    fn all_is_every_value_in_order_and_index_is_its_position() {
        let all: Vec<_> = MidiChannel::all().collect();
        assert_eq!(all.len(), usize::from(MidiChannel::COUNT));
        for (i, v) in all.iter().enumerate() {
            assert_eq!(v.index(), i);
            assert_eq!(usize::from(v.get()), i);
        }
    }
}
