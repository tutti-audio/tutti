//! What the sequencer calls things: an address, a port's capabilities and its kind.

use std::ops::BitOr;

use crate::sys;

/// The sequencer's client table holds 192 clients (the kernel's SNDRV_SEQ_MAX_CLIENTS):
/// the numbers `0..=191`. Above them are the broadcast addresses.
const CLIENTS: u8 = 192;
/// A client holds 254 ports (the kernel's SNDRV_SEQ_MAX_PORTS): the numbers `0..=253`.
/// 254 and 255 name "the subscribers" and "everyone".
const PORTS: u8 = 254;

// `ClientId`, `PortId` and `Address` are written by hand, not with `tutti_types::ranged!`:
// this crate is layer 0 and cannot name `tutti-types`, and `Address` is a pair.

/// A client's number in the sequencer: `0..=191`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientId(u8);

impl ClientId {
    /// The client `raw`; `None` past the sequencer's client table.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        if raw < CLIENTS { Some(Self(raw)) } else { None }
    }

    /// [`new`](Self::new) for a literal.
    ///
    /// # Panics
    ///
    /// When `new` refuses `raw`.
    #[must_use]
    pub const fn of(raw: u8) -> Self {
        match Self::new(raw) {
            Some(id) => id,
            None => panic!("not a sequencer client"),
        }
    }

    /// From alsa-lib's integer; `None` when it is no client.
    pub(crate) fn from_int(raw: i32) -> Option<Self> {
        Self::new(u8::try_from(raw).ok()?)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl From<ClientId> for u8 {
    fn from(id: ClientId) -> Self {
        id.0
    }
}

impl TryFrom<u8> for ClientId {
    type Error = u8;

    fn try_from(raw: u8) -> Result<Self, u8> {
        Self::new(raw).ok_or(raw)
    }
}

/// A port's number within its client: `0..=253`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortId(u8);

impl PortId {
    /// The port `raw`; `None` for the broadcast numbers 254 and 255.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        if raw < PORTS { Some(Self(raw)) } else { None }
    }

    /// [`new`](Self::new) for a literal.
    ///
    /// # Panics
    ///
    /// When `new` refuses `raw`.
    #[must_use]
    pub const fn of(raw: u8) -> Self {
        match Self::new(raw) {
            Some(id) => id,
            None => panic!("not a sequencer port"),
        }
    }

    /// From alsa-lib's integer; `None` when it is no port.
    pub(crate) fn from_int(raw: i32) -> Option<Self> {
        Self::new(u8::try_from(raw).ok()?)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl From<PortId> for u8 {
    fn from(id: PortId) -> Self {
        id.0
    }
}

impl TryFrom<u8> for PortId {
    type Error = u8;

    fn try_from(raw: u8) -> Result<Self, u8> {
        Self::new(raw).ok_or(raw)
    }
}

/// A port of a client: the pair the sequencer addresses events and
/// subscriptions by. It names a place that can exist, not one that does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address {
    client: ClientId,
    port: PortId,
}

impl Address {
    /// The system client's announce port (`SND_SEQ_CLIENT_SYSTEM`,
    /// `SND_SEQ_PORT_SYSTEM_ANNOUNCE`), which sends an event whenever a client or
    /// port starts, exits or changes.
    pub const ANNOUNCE: Self = Self::of(0, 1);

    /// The port `port` of client `client`.
    #[must_use]
    pub const fn new(client: ClientId, port: PortId) -> Self {
        Self { client, port }
    }

    /// [`new`](Self::new) for literal numbers.
    ///
    /// # Panics
    ///
    /// When either number is past the sequencer's tables.
    #[must_use]
    pub const fn of(client: u8, port: u8) -> Self {
        Self::new(ClientId::of(client), PortId::of(port))
    }

    /// From alsa-lib's integers; `None` when either is no address.
    pub(crate) fn from_ints(client: i32, port: i32) -> Option<Self> {
        Some(Self::new(
            ClientId::from_int(client)?,
            PortId::from_int(port)?,
        ))
    }

    /// The client.
    #[must_use]
    pub const fn client(self) -> ClientId {
        self.client
    }

    /// The port within its client.
    #[must_use]
    pub const fn port(self) -> PortId {
        self.port
    }
}

/// The address as one number a caller can store: the client in the upper half,
/// the port in the lower.
impl From<Address> for u64 {
    fn from(address: Address) -> Self {
        (u64::from(address.client.get()) << 32) | u64::from(address.port.get())
    }
}

/// The number [`From<Address> for u64`](Address) makes, back to its address.
/// A number that is not one of those (a bit above either byte, or a pair past the
/// tables) is refused rather than truncated to whichever port its low bits name.
impl TryFrom<u64> for Address {
    type Error = u64;

    fn try_from(raw: u64) -> Result<Self, u64> {
        let (client, port) = (raw >> 32, raw & 0xFFFF_FFFF);
        let (Ok(client), Ok(port)) = (u8::try_from(client), u8::try_from(port)) else {
            return Err(raw);
        };
        match (ClientId::new(client), PortId::new(port)) {
            (Some(client), Some(port)) => Ok(Self::new(client, port)),
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

    /// The set a port reports, bits this crate has no name for kept.
    pub(crate) const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// The bits, as alsa-lib takes them.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether every capability of `other` is in this set.
    const fn contains(self, other: Self) -> bool {
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

    /// A client below 192 is a client; 192 and up is the broadcast range and is
    /// refused, as `TryFrom` and `From` agree.
    #[test]
    fn a_client_past_the_table_is_refused() {
        assert_eq!(ClientId::new(191).map(ClientId::get), Some(191));
        assert_eq!(ClientId::new(0).map(ClientId::get), Some(0));
        assert_eq!(ClientId::new(192), None);
        assert_eq!(ClientId::new(255), None);
        assert_eq!(ClientId::try_from(192), Err(192));
        assert_eq!(u8::from(ClientId::of(130)), 130);
        assert_eq!(ClientId::from_int(-1), None);
        assert_eq!(ClientId::from_int(256), None);
    }

    /// A port below 254 is a port; 254 ("the subscribers") and 255 ("everyone")
    /// are refused.
    #[test]
    fn a_port_past_the_table_is_refused() {
        assert_eq!(PortId::new(253).map(PortId::get), Some(253));
        assert_eq!(PortId::new(254), None);
        assert_eq!(PortId::new(255), None);
        assert_eq!(PortId::try_from(254), Err(254));
        assert_eq!(u8::from(PortId::of(9)), 9);
        assert_eq!(PortId::from_int(-1), None);
    }

    /// The integers alsa-lib gives are an address when both are in range, and
    /// nothing otherwise, a negative included.
    #[test]
    fn alsa_integers_are_an_address_only_in_range() {
        assert_eq!(Address::from_ints(130, 9), Some(Address::of(130, 9)));
        assert_eq!(Address::from_ints(-1, 0), None);
        assert_eq!(Address::from_ints(0, 256), None);
        assert_eq!(Address::from_ints(192, 0), None);
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
    /// a bit above a byte in either half, a pair past the tables, or the low
    /// half of a CoreMIDI-style 32-bit unique id.
    #[test]
    fn a_number_no_address_made_is_refused() {
        for raw in [
            1 << 40,
            (1 << 32) | 0x100,
            0x100 << 32,
            192 << 32,
            254,
            0xDEAD_BEEF,
            u64::MAX,
        ] {
            assert_eq!(Address::try_from(raw), Err(raw), "{raw:#x}");
        }
    }

    /// The flags are the kernel's values: READ 1, WRITE 2, SUBS_READ 32,
    /// SUBS_WRITE 64; MIDI_GENERIC 2 and APPLICATION 1<<20.
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
        assert!(!PortCaps::from_bits(0).can_read_from());
        // A bit with no name stays and does not change the answer.
        assert!(PortCaps::from_bits(read.bits() | 0x8000).can_read_from());
    }
}
