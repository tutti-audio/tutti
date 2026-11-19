//! A `MIDIEventList` built over a word buffer, for the calls that send one.
//!
//! `MIDIEventListInit` and `MIDIEventListAdd` initialise a list piece by piece, and
//! the list is only ever read by CoreMIDI through a pointer: the buffer is zeroed
//! first (by [`EventList::new`]) and no reference to a `MIDIEventList` is formed, so nothing reads
//! uninitialised memory as a typed value. `MIDIEventListAdd` returns null when a
//! packet does not fit the buffer; that is [`Error::EventListFull`], never ignored.

use super::error::Error;
use coremidi_sys::MIDITimeStamp;
use coremidi_sys::{ByteCount, MIDIEventList, MIDIEventListAdd, MIDIEventListInit, MIDIProtocolID};
use std::marker::PhantomData;

/// Words of a list's header: its protocol and packet count.
const HEADER_WORDS: usize = 2;
/// Words of a packet's header: its 64-bit time stamp and its word count.
const PACKET_HEADER_WORDS: usize = 3;
/// Words `MIDIEventListInit` writes: the header and the first packet's header.
const INIT_WORDS: usize = HEADER_WORDS + PACKET_HEADER_WORDS;
/// Words kept past what the packets need, so a packet's end alignment never decides
/// whether it fits.
const SLACK_WORDS: usize = 3;

/// The words of buffer a list of `packets` packets holding `words` words in all
/// needs.
pub(crate) const fn capacity(packets: usize, words: usize) -> usize {
    HEADER_WORDS + packets * PACKET_HEADER_WORDS + words + SLACK_WORDS
}

/// An event list built in a buffer, valid while the buffer is borrowed.
#[derive(Debug)]
pub(crate) struct EventList<'a> {
    list: *const MIDIEventList,
    buffer: PhantomData<&'a mut [u32]>,
}

impl<'a> EventList<'a> {
    /// Zeroes `buffer` and fills it with a list in `protocol` holding each of `packets` (its time
    /// stamp in host ticks, 0 for now, and its UMP words), in order.
    ///
    /// # Errors
    ///
    /// [`Error::EventListFull`] when a packet does not fit the buffer, which is
    /// [`capacity`] words for what it holds.
    pub(crate) fn new<'w>(
        buffer: &'a mut [u32],
        protocol: MIDIProtocolID,
        packets: impl IntoIterator<Item = (MIDITimeStamp, &'w [u32])>,
    ) -> Result<Self, Error> {
        if buffer.len() < INIT_WORDS {
            return Err(Error::EventListFull);
        }
        buffer.fill(0);
        let list = buffer.as_mut_ptr().cast::<MIDIEventList>();
        let size = size_of_val(buffer) as ByteCount;
        // SAFETY: `list` addresses `buffer`, which is four-byte aligned (a `MIDIEventList`
        // is packed to four) and holds at least what `Init` writes (checked above), and
        // is zeroed memory of plain integers.
        let mut packet = unsafe { MIDIEventListInit(list, protocol) };
        for (stamp, words) in packets {
            // SAFETY: `list` was initialised by `Init` over `size` bytes, the real size
            // of `buffer`, so `Add` writes inside it; `packet` is `Init`'s or the last
            // `Add`'s return, and `words` outlives the call.
            packet = unsafe {
                MIDIEventListAdd(
                    list,
                    size,
                    packet,
                    stamp,
                    words.len() as ByteCount,
                    words.as_ptr(),
                )
            };
            if packet.is_null() {
                return Err(Error::EventListFull);
            }
        }
        Ok(Self {
            list,
            buffer: PhantomData,
        })
    }

    /// The list, for a call that reads it before it returns.
    pub(crate) fn as_ptr(&self) -> *const MIDIEventList {
        self.list
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os::source::protocol_id;
    use tutti_ump::Protocol;

    fn midi2() -> MIDIProtocolID {
        protocol_id(Protocol::Midi2).expect("MIDI 2.0 has an id")
    }

    /// A packet that does not fit the buffer is `EventListFull`, whatever `Add`
    /// does with the words it cannot hold: the first packet of 4 words needs 3 more
    /// words than the header leaves in a 5-word buffer, and a list whose second
    /// packet overflows fails the same way.
    #[test]
    fn a_packet_past_the_buffer_is_refused() {
        let words = [0x5001_0000, 1, 2, 3];
        let mut small = [0u32; INIT_WORDS];
        assert_eq!(
            EventList::new(&mut small, midi2(), [(0, &words[..])]).map(|_| ()),
            Err(Error::EventListFull)
        );
        let mut one = vec![0u32; capacity(1, 4) - SLACK_WORDS];
        assert_eq!(
            EventList::new(&mut one, midi2(), [(0, &words[..]), (0, &words[..])]).map(|_| ()),
            Err(Error::EventListFull)
        );
        let mut tiny = [0u32; 1];
        assert_eq!(
            EventList::new(&mut tiny, midi2(), []).map(|_| ()),
            Err(Error::EventListFull)
        );
    }

    /// A buffer of `capacity` words holds every packet it was sized for.
    #[test]
    fn a_buffer_of_its_capacity_holds_its_packets() {
        let words = [0x5001_0000, 1, 2, 3];
        let packets = [(0, &words[..]), (7, &words[..]), (9, &words[..1])];
        let mut buffer = vec![0u32; capacity(3, 9)];
        assert!(EventList::new(&mut buffer, midi2(), packets).is_ok());
    }
}
