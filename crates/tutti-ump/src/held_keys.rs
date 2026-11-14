//! The tables an edge keeps of what a source has started: a bit per [`Key`] and
//! a position per [`Lane`].

use crate::{Address, Key, MidiChannel, MidiGroup, Protocol};

/// A channel of one protocol: the position of a (protocol, Group, Channel) in a
/// table of 512 entries, MIDI 1.0 lanes first. The index is the one a
/// [`Key`] packs above its note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lane(u16);

impl Lane {
    /// How many lanes there are: two protocols, sixteen Groups, sixteen Channels.
    pub const COUNT: usize = 512;

    /// The lane of `address` in `protocol`, or `None` for a protocol that has
    /// no channel voice message ([`Protocol::Reserved`]).
    #[must_use]
    pub const fn new(protocol: Protocol, address: Address) -> Option<Self> {
        match protocol {
            Protocol::Midi1 => Some(Self(address.index() as u16)),
            Protocol::Midi2 => Some(Self(256 + address.index() as u16)),
            Protocol::Reserved(_) => None,
        }
    }

    /// The lane of the 9 bits a [`Key`] packs above its note.
    pub(crate) const fn from_packed(bits: u16) -> Self {
        Self(bits & 0x1ff)
    }

    /// The lane at table position `index`, or `None` from [`COUNT`](Self::COUNT) up.
    #[must_use]
    pub const fn from_index(index: usize) -> Option<Self> {
        if index < Self::COUNT {
            Some(Self(index as u16))
        } else {
            None
        }
    }

    /// The table position, `0..`[`COUNT`](Self::COUNT).
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// The protocol: [`Protocol::Midi1`] or [`Protocol::Midi2`].
    #[must_use]
    pub const fn protocol(self) -> Protocol {
        if self.0 >= 256 {
            Protocol::Midi2
        } else {
            Protocol::Midi1
        }
    }

    /// The Group and Channel.
    #[must_use]
    pub const fn address(self) -> Address {
        let lane = (self.0 & 0xff) as u8;
        Address {
            group: MidiGroup::masked(lane >> 4),
            channel: MidiChannel::masked(lane),
        }
    }
}

const WORDS: usize = 1 << 10;

/// A set of [`Key`]s: one bit for each of the 65536 packed keys, 8 KiB, kept in
/// key order. It holds no allocation: a caller that wants it off the stack boxes
/// it.
///
/// ```
/// use tutti_ump::{Address, Key, KeyBitset, MidiChannel, NoteNumber, Protocol};
///
/// let mut held = KeyBitset::new();
/// let key = Key::new(Protocol::Midi2, Address::first(MidiChannel::of(0)), NoteNumber::of(60)).unwrap();
/// assert!(held.insert(key));
/// assert!(!held.insert(key));
/// assert_eq!(held.iter().collect::<Vec<_>>(), [key]);
/// assert!(held.remove(key));
/// assert!(held.is_empty());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBitset {
    words: [u64; WORDS],
    len: u32,
}

impl Default for KeyBitset {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyBitset {
    /// The empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            words: [0; WORDS],
            len: 0,
        }
    }

    /// Adds `key`; whether it was not there.
    pub fn insert(&mut self, key: Key) -> bool {
        let (word, bit) = Self::place(key);
        let added = self.words[word] & bit == 0;
        self.words[word] |= bit;
        self.len += u32::from(added);
        added
    }

    /// Removes `key`; whether it was there.
    pub fn remove(&mut self, key: Key) -> bool {
        let (word, bit) = Self::place(key);
        let removed = self.words[word] & bit != 0;
        self.words[word] &= !bit;
        self.len -= u32::from(removed);
        removed
    }

    /// Whether `key` is in the set.
    #[must_use]
    pub fn contains(&self, key: Key) -> bool {
        let (word, bit) = Self::place(key);
        self.words[word] & bit != 0
    }

    /// How many keys are in the set.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether no key is in the set.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Empties the set.
    pub fn clear(&mut self) {
        self.words.fill(0);
        self.len = 0;
    }

    /// Adds every key of `other`.
    pub fn union_with(&mut self, other: &Self) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a |= b;
        }
        self.len = self.words.iter().map(|w| w.count_ones()).sum();
    }

    /// The keys, in key order.
    pub fn iter(&self) -> impl Iterator<Item = Key> + '_ {
        self.words.iter().enumerate().flat_map(|(w, &word)| {
            let mut bits = word;
            core::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(Key::from_index((w * 64 + b) as u16))
            })
        })
    }

    /// The first key at packed position `from` or above ([`Key::index`]), or
    /// `None`: a walk that may remove what it has passed.
    #[must_use]
    pub fn next_from(&self, from: u32) -> Option<Key> {
        let mut w = (from >> 6) as usize;
        if w >= WORDS {
            return None;
        }
        let mut bits = self.words[w] & (!0u64 << (from & 63));
        loop {
            if bits != 0 {
                return Some(Key::from_index(
                    (w * 64 + bits.trailing_zeros() as usize) as u16,
                ));
            }
            w += 1;
            bits = *self.words.get(w)?;
        }
    }

    const fn place(key: Key) -> (usize, u64) {
        let i = key.index();
        ((i >> 6) as usize, 1u64 << (i & 63))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoteNumber;

    fn key(protocol: Protocol, index: usize, note: u8) -> Key {
        let address = Address::from_index(index).unwrap();
        Key::new(protocol, address, NoteNumber::of(note)).unwrap()
    }

    #[test]
    fn a_key_is_in_the_set_once_and_the_set_counts_keys() {
        let mut set = KeyBitset::new();
        let k = key(Protocol::Midi1, 17, 60);
        assert!(set.insert(k));
        assert!(!set.insert(k));
        assert!(set.contains(k));
        assert_eq!(set.len(), 1);
        assert!(set.remove(k));
        assert!(!set.remove(k));
        assert!(set.is_empty());
    }

    #[test]
    fn the_keys_come_back_in_key_order_across_every_word() {
        let mut set = KeyBitset::new();
        let keys = [
            key(Protocol::Midi2, 255, 127),
            key(Protocol::Midi1, 0, 0),
            key(Protocol::Midi1, 200, 64),
            key(Protocol::Midi2, 0, 1),
        ];
        for k in keys {
            set.insert(k);
        }
        let mut want = keys;
        want.sort();
        assert_eq!(set.iter().collect::<Vec<_>>(), want);
    }

    #[test]
    fn next_from_walks_the_keys_while_they_are_removed() {
        let mut set = KeyBitset::new();
        let keys = [
            key(Protocol::Midi1, 0, 5),
            key(Protocol::Midi1, 9, 127),
            key(Protocol::Midi2, 255, 127),
        ];
        for k in keys {
            set.insert(k);
        }
        let mut walked = Vec::new();
        let mut next = 0;
        while let Some(k) = set.next_from(next) {
            walked.push(k);
            set.remove(k);
            next = u32::from(k.index()) + 1;
        }
        assert_eq!(walked, keys);
        assert!(set.is_empty());
        assert_eq!(set.next_from(65_536), None);
    }

    #[test]
    fn a_union_counts_the_keys_both_hold_once() {
        let (mut a, mut b) = (KeyBitset::new(), KeyBitset::new());
        let shared = key(Protocol::Midi1, 3, 10);
        a.insert(shared);
        b.insert(shared);
        b.insert(key(Protocol::Midi2, 3, 10));
        a.union_with(&b);
        assert_eq!(a.len(), 2);
        a.clear();
        assert!(a.is_empty());
    }

    #[test]
    fn a_lane_is_the_protocol_then_the_address_and_names_both_back() {
        let address = Address::from_index(0x2B).unwrap();
        let m1 = Lane::new(Protocol::Midi1, address).unwrap();
        let m2 = Lane::new(Protocol::Midi2, address).unwrap();
        assert_eq!((m1.index(), m2.index()), (0x2B, 256 + 0x2B));
        assert_eq!(
            (m1.protocol(), m2.protocol()),
            (Protocol::Midi1, Protocol::Midi2)
        );
        assert_eq!((m1.address(), m2.address()), (address, address));
        assert_eq!(Lane::new(Protocol::Reserved(3), address), None);
        assert_eq!(Lane::from_index(511).map(Lane::index), Some(511));
        assert_eq!(Lane::from_index(512), None);
        assert_eq!(key(Protocol::Midi2, 0x2B, 5).lane(), m2);
    }
}
