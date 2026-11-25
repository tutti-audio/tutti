//! What a source has started and not ended: a bit per [`NoteKey`], the controllers
//! per [`GroupChannelIndex`], and [`Held`], the two together.

use alloc::boxed::Box;

use crate::{Address, HeldControllers, MidiGroup, NoteKey, NoteKeyEvent, Protocol, Ump};

tutti_units::ranged! {
    /// A channel of one protocol: the position of a (protocol, Group, Channel) in a
    /// table of 512 entries, MIDI 1.0 lanes first.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    GroupChannelIndex(u16), GroupChannelIndexOutOfRange, "within 0..512", |raw| raw < 512
}

impl GroupChannelIndex {
    /// How many lanes there are: two protocols, sixteen Groups, sixteen Channels.
    pub const COUNT: usize = 512;

    /// The lane of `address` in `protocol`, or `None` for a protocol that has
    /// no channel voice message ([`Protocol::Reserved`]).
    #[must_use]
    pub const fn at(protocol: Protocol, address: Address) -> Option<Self> {
        match protocol {
            Protocol::Midi1 => Some(Self(address.index() as u16)),
            Protocol::Midi2 => Some(Self(256 + address.index() as u16)),
            Protocol::Reserved(_) => None,
        }
    }

    /// Every lane, in table order.
    ///
    /// ```
    /// use tutti_ump::GroupChannelIndex;
    ///
    /// assert_eq!(GroupChannelIndex::all().count(), GroupChannelIndex::COUNT);
    /// assert_eq!(GroupChannelIndex::all().next().map(GroupChannelIndex::index), Some(0));
    /// ```
    pub fn all() -> impl Iterator<Item = GroupChannelIndex> {
        (0..Self::COUNT as u16).map(GroupChannelIndex)
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
        Address::from_lane((self.0 & 0xff) as u8)
    }
}

const WORDS: usize = 1 << 9;

/// A set of [`NoteKey`]s: one bit for each of the 32768 packed keys, 4 KiB, kept in
/// key order. It holds no allocation: a caller that wants it off the stack boxes
/// it.
///
/// ```
/// use tutti_ump::{Address, NoteKey, KeyBitset, MidiChannel, NoteNumber, Protocol};
///
/// let mut held = KeyBitset::new();
/// let key = NoteKey::new(Address::first(MidiChannel::of(0)), NoteNumber::of(60));
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
    pub fn insert(&mut self, key: NoteKey) -> bool {
        let (word, bit) = Self::place(key);
        let added = self.words[word] & bit == 0;
        self.words[word] |= bit;
        self.len += u32::from(added);
        added
    }

    /// Removes `key`; whether it was there.
    pub fn remove(&mut self, key: NoteKey) -> bool {
        let (word, bit) = Self::place(key);
        let removed = self.words[word] & bit != 0;
        self.words[word] &= !bit;
        self.len -= u32::from(removed);
        removed
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
    pub fn iter(&self) -> impl Iterator<Item = NoteKey> + '_ {
        self.words.iter().enumerate().flat_map(|(w, &word)| {
            let mut bits = word;
            core::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(NoteKey::from_index((w * 64 + b) as u16))
            })
        })
    }

    /// The first key at packed position `from` or above ([`NoteKey::index`]), or
    /// `None`: a walk that may remove what it has passed.
    #[must_use]
    pub fn next_from(&self, from: u32) -> Option<NoteKey> {
        let mut w = (from >> 6) as usize;
        if w >= WORDS {
            return None;
        }
        let mut bits = self.words[w] & (!0u64 << (from & 63));
        loop {
            if bits != 0 {
                return Some(NoteKey::from_index(
                    (w * 64 + bits.trailing_zeros() as usize) as u16,
                ));
            }
            w += 1;
            bits = *self.words.get(w)?;
        }
    }

    const fn place(key: NoteKey) -> (usize, u64) {
        let i = key.index();
        ((i >> 6) as usize, 1u64 << (i & 63))
    }
}

/// What a source has started and not ended: the notes down and the controllers
/// (the hold pedals and the pitch bend) off their normal position. Whoever sits
/// on a source's edge folds every message the source sent into it
/// ([`observe`](Self::observe)) and, when the source ends, lets go of what it
/// held as the messages that end it ([`release`](Self::release)).
///
/// **What a release sends, and why not All Notes Off.** Per held key a MIDI
/// 2.0 Note Off at velocity 0, whatever protocol started the note; then, for each hold pedal
/// (CC 64, 66, 67, 69) that is down a pedal-up and for each pitch bend off
/// centre a centred bend, in the protocol it was sent in. Nothing channelwide
/// (CC 120, 121, 123): a channel is shared, because several sources merge into
/// one listener's fan-in, so a channelwide message from one source would end
/// another's notes and reset another's wheel. MIDI 1.0 Detailed Specification
/// 4.2.1 also says that recognising All Notes Off is not required of a
/// receiver and that notes should first be turned off by individual Note-Off
/// messages (p. 24), so the individual offs are what every receiver
/// understands. The notes come first and the controllers after, because a pedal
/// that is down holds a note through a Note Off or All Notes Off until it is
/// released (Appendix A-5), so the pedal-up lets go of the notes just ended;
/// p. 31 (Stop) and RP-015 (Reset All Controllers) name the pedals and the
/// pitch wheel as what returns to its normal position.
///
/// A release may be asked for one Group: a Group is one cable of a device
/// (M2-104 section 2.1.2, Appendix B.1), and what Active Sensing or a System
/// Reset says of a cable is about that cable's Group.
///
/// The tables are boxed, 4 KiB of keys and 1 KiB of controllers, allocated in
/// [`new`](Self::new). Nothing else allocates.
///
/// ```
/// use tutti_ump::{Held, Ump};
///
/// let mut held = Held::new();
/// held.observe(Ump([0x2090_3c64, 0, 0, 0])); // Note On, group 0, channel 0, key 60
/// let mut offs = Vec::new();
/// assert!(held.release(None, |ump| { offs.push(ump); true }));
/// assert_eq!(offs, [Ump([0x4080_3c00, 0, 0, 0])]); // a MIDI 2.0 Note Off
/// assert!(held.is_empty());
/// ```
#[derive(Clone, Default)]
pub struct Held {
    keys: Box<KeyBitset>,
    controllers: Box<HeldControllers>,
}

impl core::fmt::Debug for Held {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Held")
            .field("notes", &self.keys.len())
            .field("controllers", &self.controllers.len())
            .finish()
    }
}

impl Held {
    /// Nothing held. Control thread: allocates.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The notes down.
    #[must_use]
    pub fn keys(&self) -> &KeyBitset {
        &self.keys
    }

    /// The notes down, for a caller that walks and ends them itself.
    pub fn keys_mut(&mut self) -> &mut KeyBitset {
        &mut self.keys
    }

    /// The controllers off their normal position.
    #[must_use]
    pub fn controllers(&self) -> &HeldControllers {
        &self.controllers
    }

    /// The controllers off their normal position, for a caller that releases them
    /// itself.
    pub fn controllers_mut(&mut self) -> &mut HeldControllers {
        &mut self.controllers
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.controllers.is_empty()
    }

    /// Forgets everything held, sending nothing.
    pub fn clear(&mut self) {
        self.keys.clear();
        self.controllers.clear();
    }

    /// Adds everything `other` holds.
    pub fn union_with(&mut self, other: &Self) {
        self.keys.union_with(&other.keys);
        self.controllers.union_with(&other.controllers);
    }

    /// Folds in a message the source sent. A note on starts its key (and a
    /// note off ends it, a MIDI 1.0 Note On at velocity 0 included); a pedal
    /// or bend off its normal position is held, and back at it is let go.
    /// Anything else changes nothing.
    #[inline]
    pub fn observe(&mut self, ump: impl Into<Ump>) {
        let ump = ump.into();
        match ump.note_key_event() {
            Some(NoteKeyEvent::On(key)) => {
                self.keys.insert(key);
            }
            Some(NoteKeyEvent::Off(key)) => {
                self.keys.remove(key);
            }
            Some(NoteKeyEvent::Sustain { .. }) | None => {}
        }
        self.controllers.observe(ump);
    }

    /// Hands `accept` the message that ends each thing held (all of `group`'s
    /// Group, or every Group's for `None`): each note's Note Off in key order,
    /// then each controller's return, and forgets what `accept` takes
    /// (`true`). Stops at the first it refuses (`false`), which stays held with
    /// everything after it, so a receiver with no room now is asked again.
    /// Returns whether everything in scope was taken. Allocates nothing.
    pub fn release(
        &mut self,
        group: Option<MidiGroup>,
        mut accept: impl FnMut(Ump) -> bool,
    ) -> bool {
        let mut next = 0;
        while let Some(key) = self.keys.next_from(next) {
            next = u32::from(key.index()) + 1;
            if group.is_some_and(|g| key.address().group != g) {
                continue;
            }
            if !accept(Ump::note_off(key)) {
                return false;
            }
            self.keys.remove(key);
        }
        self.controllers.release_while(group, accept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BendValue, CCNumber, ControlValue, Message, MidiChannel, NoteNumber, NoteVelocity, U7, U14,
        Voice1, Voice2,
    };

    fn key(index: usize, note: u8) -> NoteKey {
        let address = Address::from_index(index).unwrap();
        NoteKey::new(address, NoteNumber::of(note))
    }

    #[test]
    fn a_key_is_in_the_set_once_and_the_set_counts_keys() {
        let mut set = KeyBitset::new();
        let k = key(17, 60);
        assert!(set.insert(k));
        assert!(!set.insert(k));
        assert_eq!(set.len(), 1);
        assert!(set.remove(k));
        assert!(!set.remove(k));
        assert!(set.is_empty());
    }

    #[test]
    fn the_keys_come_back_in_key_order_across_every_word() {
        let mut set = KeyBitset::new();
        let keys = [key(255, 127), key(0, 0), key(200, 64), key(0, 1)];
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
        let keys = [key(0, 5), key(9, 127), key(255, 127)];
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
        assert_eq!(set.next_from(32_768), None);
    }

    #[test]
    fn a_union_counts_the_keys_both_hold_once() {
        let (mut a, mut b) = (KeyBitset::new(), KeyBitset::new());
        let shared = key(3, 10);
        a.insert(shared);
        b.insert(shared);
        b.insert(key(3, 11));
        a.union_with(&b);
        assert_eq!(a.len(), 2);
        a.clear();
        assert!(a.is_empty());
    }

    #[test]
    fn a_lane_is_the_protocol_then_the_address_and_names_both_back() {
        let address = Address::from_index(0x2B).unwrap();
        let m1 = GroupChannelIndex::at(Protocol::Midi1, address).unwrap();
        let m2 = GroupChannelIndex::at(Protocol::Midi2, address).unwrap();
        assert_eq!((m1.index(), m2.index()), (0x2B, 256 + 0x2B));
        assert_eq!(
            (m1.protocol(), m2.protocol()),
            (Protocol::Midi1, Protocol::Midi2)
        );
        assert_eq!((m1.address(), m2.address()), (address, address));
        assert_eq!(
            GroupChannelIndex::at(Protocol::Reserved(crate::Reserved::wire(3)), address),
            None
        );
    }

    const G0: MidiGroup = MidiGroup::FIRST;
    const G1: MidiGroup = MidiGroup::of(1);

    fn on(group: MidiGroup, channel: u8, note: u8) -> Ump {
        let address = Address::new(group, MidiChannel::of(channel));
        Message::midi2(
            address,
            Voice2::note_on(NoteNumber::of(note), NoteVelocity::MAX),
        )
        .into()
    }

    fn off(group: MidiGroup, channel: u8, note: u8) -> Ump {
        let address = Address::new(group, MidiChannel::of(channel));
        Ump::note_off(NoteKey::new(address, NoteNumber::of(note)))
    }

    fn drain(held: &mut Held, group: Option<MidiGroup>) -> Vec<Ump> {
        let mut out = Vec::new();
        assert!(held.release(group, |u| {
            out.push(u);
            true
        }));
        out
    }

    /// Whether nothing is held: a release that is refused its first message
    /// keeps everything, so it only probes.
    fn is_empty(held: &mut Held) -> bool {
        let mut any = false;
        held.release(None, |_| {
            any = true;
            false
        });
        !any
    }

    /// **A release ends exactly what was started**: two keys down and one up,
    /// the release is one Note Off for the key still down; a second release has
    /// nothing to send.
    #[test]
    fn a_release_ends_the_keys_still_down_and_only_those() {
        let mut held = Held::new();
        held.observe(on(G0, 0, 60));
        held.observe(on(G0, 0, 64));
        held.observe(Message::midi2(
            Address::new(G0, MidiChannel::of(0)),
            Voice2::note_off(NoteNumber::of(60), NoteVelocity::MIN),
        ));
        assert_eq!(drain(&mut held, None), [off(G0, 0, 64)]);
        assert!(is_empty(&mut held));
        assert!(drain(&mut held, None).is_empty());
    }

    /// **A MIDI 1.0 Note On at velocity 0 is an off** (MIDI 1.0 Detailed
    /// Specification, Note On): the key it names is not held; the same bytes in
    /// MIDI 2.0 are a note on (M2-104 section 7.4.2).
    #[test]
    fn a_midi1_note_on_at_velocity_zero_ends_the_key() {
        let mut held = Held::new();
        let m1_on = |v: u8| {
            Ump::from(Message::midi1(
                MidiChannel::of(2),
                Voice1::note_on(NoteNumber::of(60), U7::of(v)),
            ))
        };
        held.observe(m1_on(100));
        assert!(!is_empty(&mut held));
        held.observe(m1_on(0));
        assert!(is_empty(&mut held));
    }

    /// **A release for one Group leaves the other's alone** (a Group is a cable,
    /// M2-104 section 2.1.2): notes on Groups 1 and 2, release Group 2, and
    /// Group 1's note is still held.
    /// Spec: m2-104/2.1.2#2
    #[test]
    fn a_release_for_one_group_leaves_the_others_held() {
        let mut held = Held::new();
        held.observe(on(G0, 0, 60));
        held.observe(on(G1, 0, 61));
        held.observe(Message::midi1(
            Address::new(G1, MidiChannel::of(0)),
            Voice1::control_change(CCNumber::SUSTAIN, U7::of(127)),
        ));
        let sent = drain(&mut held, Some(G1));
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[0], off(G1, 0, 61));
        assert!(!is_empty(&mut held));
        assert_eq!(drain(&mut held, Some(G0)), [off(G0, 0, 60)]);
    }

    /// **A receiver with no room is asked again for what it refused**: refusing
    /// the second of three keys leaves it and the third held, and the next
    /// release sends exactly those.
    #[test]
    fn a_refused_release_stays_held_for_the_next() {
        let mut held = Held::new();
        for n in [60, 61, 62] {
            held.observe(on(G0, 0, n));
        }
        let mut taken = 0;
        let done = held.release(None, |_| {
            taken += 1;
            taken < 2
        });
        assert!(!done);
        assert_eq!(taken, 2);
        assert_eq!(drain(&mut held, None), [off(G0, 0, 61), off(G0, 0, 62)]);
    }

    /// **Notes first, then every hold pedal and the bend** (MIDI 1.0 Detailed
    /// Specification Appendix A-5: a pedal that is down defeats the offs), each
    /// one a message in the protocol it was sent in: Group 3 channel 6 holds a
    /// key, the four pedals and an off-centre bend in MIDI 1.0 and in MIDI 2.0.
    /// The release is the MIDI 2.0 Note Off, then for each protocol CC 64, 66, 67
    /// and 69 at 0 (MIDI 1.0 Detailed Specification, Control Change messages:
    /// Damper Pedal (Sustain), Sostenuto, Soft Pedal, Hold 2) and the bend at its
    /// centre (0x2000 in MIDI 1.0, 0x80000000 in MIDI 2.0), as these words.
    #[test]
    fn a_release_frees_every_pedal_and_centres_the_bend_message_by_message() {
        let address = Address::new(MidiGroup::of(3), MidiChannel::of(6));
        let mut held = Held::new();
        for cc in [64, 66, 67, 69] {
            let cc = CCNumber::of(cc);
            held.observe(Message::midi1(
                address,
                Voice1::control_change(cc, U7::of(127)),
            ));
            held.observe(Message::midi2(
                address,
                Voice2::control_change(cc, ControlValue::MAX),
            ));
        }
        held.observe(Message::midi1(address, Voice1::PitchBend(U14::of(0))));
        held.observe(Message::midi2(
            address,
            Voice2::PitchBend(BendValue::new(0)),
        ));
        held.observe(Message::midi1(
            address,
            Voice1::note_on(NoteNumber::of(60), U7::of(100)),
        ));
        let m1 = |word: u32| Ump([0x2000_0000 | 0x0306_0000 | word, 0, 0, 0]);
        let m2 = |word: u32, data: u32| Ump([0x4000_0000 | 0x0306_0000 | word, data, 0, 0]);
        assert_eq!(
            drain(&mut held, None),
            [
                m2(0x0080_3c00, 0),
                m1(0x00b0_4000),
                m1(0x00b0_4200),
                m1(0x00b0_4300),
                m1(0x00b0_4500),
                m1(0x00e0_0040),
                m2(0x00b0_4000, 0),
                m2(0x00b0_4200, 0),
                m2(0x00b0_4300, 0),
                m2(0x00b0_4500, 0),
                m2(0x00e0_0000, 0x8000_0000),
            ]
        );
        assert!(is_empty(&mut held));
    }

    /// **A pedal is held from the middle of its range** (MIDI 1.0 Detailed
    /// Specification, Control Change messages: 0 to 63 off, 64 to 127 on; MIDI
    /// 2.0 the upper half of 32 bits) and a bend from any value but its centre.
    #[test]
    fn a_controller_is_held_from_its_midpoint_and_let_go_back_at_normal() {
        let address = Address::first(MidiChannel::of(0));
        let cc = |v: u8| {
            Message::midi1(
                address,
                Voice1::control_change(CCNumber::SUSTAIN, U7::of(v)),
            )
        };
        let cc2 = |v: u32| {
            Message::midi2(
                address,
                Voice2::control_change(CCNumber::SUSTAIN, ControlValue::new(v)),
            )
        };
        let mut held = HeldControllers::new();
        for (message, down) in [
            (cc(63), false),
            (cc(64), true),
            (cc(0), false),
            (cc2(0x7fff_ffff), false),
            (cc2(0x8000_0000), true),
            (cc2(0), false),
        ] {
            held.observe(message.into());
            assert_eq!(!held.is_empty(), down, "{message:?}");
        }
        let bend = Message::midi1(address, Voice1::PitchBend(U14::CENTER));
        held.observe(bend.into());
        assert!(held.is_empty());
        held.observe(Message::midi1(address, Voice1::PitchBend(U14::of(0x2001))).into());
        assert_eq!(held.closes().count(), 1);
        held.observe(bend.into());
        assert!(held.is_empty());
    }

    /// **A refused release is owed until it is paid, and a new value forgets it**:
    /// three controllers held, the second refused, so it and the third are owed
    /// and still held; a pay that takes one and refuses the next leaves the last
    /// owed; a release the source then supersedes is no longer owed.
    #[test]
    fn a_refused_release_is_owed_until_paid_and_a_new_value_forgets_it() {
        let address = Address::first(MidiChannel::of(0));
        let mut held = HeldControllers::new();
        for cc in [64, 66, 67] {
            held.observe(
                Message::midi1(
                    address,
                    Voice1::control_change(CCNumber::of(cc), U7::of(127)),
                )
                .into(),
            );
        }
        let mut offered = Vec::new();
        held.release_all(|_, owed| {
            offered.push(owed);
            offered.len() < 2
        });
        assert_eq!(offered, [false, false, false]);
        assert!(held.owing());
        assert_eq!(held.closes().count(), 2);
        let mut taken = 0;
        held.pay(|_| {
            taken += 1;
            taken < 2
        });
        assert_eq!(held.closes().count(), 1);
        assert!(held.owing());
        held.observe(
            Message::midi1(address, Voice1::control_change(CCNumber::of(67), U7::of(0))).into(),
        );
        held.pay(|_| unreachable!("nothing is owed now"));
        assert!(held.is_empty());
        assert!(!held.owing());
    }

    /// **Debug prints counts**, not the tables.
    #[test]
    fn debug_prints_the_counts_of_what_is_held() {
        let mut held = Held::new();
        held.observe(on(G0, 0, 60));
        held.observe(Message::midi1(
            Address::first(MidiChannel::of(0)),
            Voice1::control_change(CCNumber::SUSTAIN, U7::of(127)),
        ));
        assert_eq!(format!("{held:?}"), "Held { notes: 1, controllers: 1 }");
    }

    /// **A union holds what either held, once each**, and `pop` takes the
    /// lowest first.
    #[test]
    fn a_union_holds_each_controller_once_and_pop_takes_the_first() {
        let address = Address::first(MidiChannel::of(0));
        let down = |cc: u8| {
            Ump::from(Message::midi1(
                address,
                Voice1::control_change(CCNumber::of(cc), U7::of(127)),
            ))
        };
        let (mut a, mut b) = (HeldControllers::new(), HeldControllers::new());
        a.observe(down(66));
        b.observe(down(66));
        b.observe(down(64));
        a.union_with(&b);
        assert_eq!(a.closes().count(), 2);
        let first = a.pop().unwrap();
        assert_eq!(first, Ump([0x2000_0000 | 0x00b0_4000, 0, 0, 0]));
        assert_eq!(a.closes().count(), 1);
        a.clear();
        assert_eq!(a.pop(), None);
    }
}
