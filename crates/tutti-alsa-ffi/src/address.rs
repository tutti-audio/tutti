//! What the sequencer calls things: an address, a port's capabilities and its kind.

use std::ops::BitOr;

use crate::sys;

/// A client's or a port's number, as one byte.
type Byte = u8;

/// The sequencer's client table holds 192 clients (the kernel's SNDRV_SEQ_MAX_CLIENTS): the
/// numbers `0..=191`. Above them are the broadcast addresses.
const CLIENTS: Byte = 192;
/// A client holds 254 ports (the kernel's SNDRV_SEQ_MAX_PORTS): the numbers `0..=253`.
/// 254 and 255 name "the subscribers" and "everyone".
const PORTS: Byte = 254;

/// A port of a client: the pair the sequencer addresses events and
/// subscriptions by. It names a place that can exist, not one that does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address {
    client: Byte,
    port: Byte,
}

impl Address {
    /// The system client's announce port (`SND_SEQ_CLIENT_SYSTEM`,
    /// `SND_SEQ_PORT_SYSTEM_ANNOUNCE`), which sends an event whenever a client or
    /// port starts, exits or changes.
    pub const ANNOUNCE: Self = Self { client: 0, port: 1 };

    /// The port `port` of client `client`; `None` when either is past the
    /// sequencer's tables (clients `0..=191`, ports `0..=253`), which is the
    /// broadcast range and no one's port.
    #[must_use]
    pub const fn new(client: u8, port: u8) -> Option<Self> {
        if client < CLIENTS && port < PORTS {
            Some(Self { client, port })
        } else {
            None
        }
    }

    /// [`new`](Self::new) for a literal; panics on a refused pair.
    ///
    /// # Panics
    ///
    /// When `new` refuses the pair.
    #[must_use]
    pub const fn of(client: u8, port: u8) -> Self {
        match Self::new(client, port) {
            Some(address) => address,
            None => panic!("not a sequencer address"),
        }
    }

    /// From alsa-lib's integers; `None` when either is no address.
    pub(crate) fn from_ints(client: i32, port: i32) -> Option<Self> {
        Self::new(u8::try_from(client).ok()?, u8::try_from(port).ok()?)
    }

    /// The client's number.
    #[must_use]
    pub const fn client(self) -> u8 {
        self.client
    }

    /// The port's number within its client.
    #[must_use]
    pub const fn port(self) -> u8 {
        self.port
    }
}

/// The address as one number a caller can store: the client in the upper half,
/// the port in the lower.
impl From<Address> for u64 {
    fn from(address: Address) -> Self {
        (u64::from(address.client) << 32) | u64::from(address.port)
    }
}

/// The number [`From<Address> for u64`](Address) makes, back to its address.
/// A number that is not one of those (a bit above either byte, or a pair past the
/// tables) is refused rather than truncated to whichever port its low bits name.
impl TryFrom<u64> for Address {
    type Error = u64;

    fn try_from(raw: u64) -> Result<Self, u64> {
        let (client, port) = (raw >> 32, raw & 0xFFFF_FFFF);
        match (u8::try_from(client), u8::try_from(port)) {
            (Ok(client), Ok(port)) => Self::new(client, port).ok_or(raw),
            _ => Err(raw),
        }
    }
}

/// What a port allows (`SND_SEQ_PORT_CAP_*`), a set of flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PortCaps(u32);

impl PortCaps {
    /// The port may be read from (`SND_SEQ_PORT_CAP_READ`).
    pub const READ: Self = Self(sys::SND_SEQ_PORT_CAP_READ);
    /// The port may be written to (`SND_SEQ_PORT_CAP_WRITE`).
    pub const WRITE: Self = Self(sys::SND_SEQ_PORT_CAP_WRITE);
    /// Other clients may subscribe to read from the port (`SND_SEQ_PORT_CAP_SUBS_READ`).
    pub const SUBS_READ: Self = Self(sys::SND_SEQ_PORT_CAP_SUBS_READ);
    /// Other clients may subscribe to write to the port (`SND_SEQ_PORT_CAP_SUBS_WRITE`).
    pub const SUBS_WRITE: Self = Self(sys::SND_SEQ_PORT_CAP_SUBS_WRITE);

    /// No capability.
    pub const NONE: Self = Self(0);

    /// The set a port reports, bits this crate has no name for kept.
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// The bits, as alsa-lib takes them.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether every capability of `other` is in this set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether another client can subscribe to read what the port sends: it is
    /// readable and subscribable. A port that is readable but not
    /// subscribe-readable cannot be connected to.
    #[must_use]
    pub const fn can_read_from(self) -> bool {
        self.contains(Self(Self::READ.0 | Self::SUBS_READ.0))
    }

    /// Whether another client can subscribe to write to the port: it is
    /// writable and subscribable.
    #[must_use]
    pub const fn can_write_to(self) -> bool {
        self.contains(Self(Self::WRITE.0 | Self::SUBS_WRITE.0))
    }
}

impl BitOr for PortCaps {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// What a port is (`SND_SEQ_PORT_TYPE_*`), a set of flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PortKind(u32);

impl PortKind {
    /// A generic MIDI port (`SND_SEQ_PORT_TYPE_MIDI_GENERIC`).
    pub const MIDI_GENERIC: Self = Self(sys::SND_SEQ_PORT_TYPE_MIDI_GENERIC);
    /// A port owned by an application (`SND_SEQ_PORT_TYPE_APPLICATION`).
    pub const APPLICATION: Self = Self(sys::SND_SEQ_PORT_TYPE_APPLICATION);

    /// The bits, as alsa-lib takes them.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl BitOr for PortKind {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client below 192 and a port below 254 make an address; the first past
    /// either is the broadcast range and is refused.
    #[test]
    fn an_address_past_the_tables_is_refused() {
        assert_eq!(Address::new(191, 253).map(Address::client), Some(191));
        assert_eq!(Address::new(0, 0).map(Address::port), Some(0));
        assert_eq!(Address::new(192, 0), None);
        assert_eq!(Address::new(0, 254), None);
        assert_eq!(Address::new(255, 255), None);
    }

    /// The integers alsa-lib gives are an address when both are in range, and
    /// nothing otherwise, a negative included.
    #[test]
    fn alsa_integers_are_an_address_only_in_range() {
        assert_eq!(Address::from_ints(130, 9), Some(Address::of(130, 9)));
        assert_eq!(Address::from_ints(-1, 0), None);
        assert_eq!(Address::from_ints(0, 256), None);
        assert_eq!(Address::from_ints(0, 254), None);
    }

    /// Packing puts the client above the port, and every address unpacks to what
    /// went in, at the ends of both ranges.
    #[test]
    fn a_packed_address_unpacks_to_itself() {
        assert_eq!(u64::from(Address::of(5, 3)), (5 << 32) | 3);
        for client in [0, 1, 128, 191] {
            for port in [0, 1, 253] {
                let address = Address::of(client, port);
                assert_eq!(Address::try_from(u64::from(address)), Ok(address));
            }
        }
    }

    /// A number no address packs to is refused whole, not truncated to a port:
    /// a bit above a byte in either half, or a pair past the tables.
    #[test]
    fn a_number_no_address_made_is_refused() {
        for raw in [
            1 << 40,
            (1 << 32) | 0x100,
            0x100 << 32,
            192 << 32,
            254,
            u64::MAX,
        ] {
            assert_eq!(Address::try_from(raw), Err(raw), "{raw:#x}");
        }
    }

    /// The predicates are the flag values the kernel defines: READ 1, WRITE 2,
    /// SUBS_READ 32, SUBS_WRITE 64.
    #[test]
    fn caps_are_the_kernels_flag_values() {
        assert_eq!(PortCaps::READ.bits(), 0x01);
        assert_eq!(PortCaps::WRITE.bits(), 0x02);
        assert_eq!(PortCaps::SUBS_READ.bits(), 0x20);
        assert_eq!(PortCaps::SUBS_WRITE.bits(), 0x40);
        assert_eq!(PortKind::MIDI_GENERIC.bits(), 0x2);
        assert_eq!(PortKind::APPLICATION.bits(), 1 << 20);
    }

    /// Reading from a port takes both the read and the subscribe-read bit, and
    /// writing to it both of its pair; neither alone, and neither direction's pair
    /// grants the other.
    #[test]
    fn a_direction_needs_its_raw_and_its_subscribe_bit() {
        let read = PortCaps::READ | PortCaps::SUBS_READ;
        let write = PortCaps::WRITE | PortCaps::SUBS_WRITE;
        assert!(read.can_read_from() && !read.can_write_to());
        assert!(write.can_write_to() && !write.can_read_from());
        assert!(!PortCaps::READ.can_read_from());
        assert!(!PortCaps::SUBS_READ.can_read_from());
        assert!(!PortCaps::WRITE.can_write_to());
        assert!(!PortCaps::SUBS_WRITE.can_write_to());
        assert!((read | write).can_read_from() && (read | write).can_write_to());
        assert!(!PortCaps::NONE.can_read_from());
        // A bit with no name stays and does not change the answer.
        assert!(PortCaps::from_bits(read.bits() | 0x8000).can_read_from());
    }
}
