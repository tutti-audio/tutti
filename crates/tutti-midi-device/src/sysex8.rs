//! The Stream Ids a sender of System Exclusive 8 messages uses (M2-104 §7.8,
//! "Stream id"): which are free to open, and how many the receiver supports.
//!
//! The receive side needs no policy of its own: a `Sysex8Assembler` (in
//! `tutti-ump`) keeps a message open for each Group and Stream Id it is sent,
//! as many as its lanes, and a receiver sizes them to the
//! [`max_sysex8_streams`](crate::FunctionBlock::max_sysex8_streams) it declares.

use tutti_ump::{MidiGroup, Sysex8Message};

use crate::endpoint::FunctionBlock;

/// How many simultaneous SysEx8 streams a receiver supports, as the sender
/// knows it (§7.8, §7.1.8 "Max Number of SysEx 8 Streams").
///
/// A sender that has not learned the number from a Function Block Info
/// Notification, because either side lacks Function Block Discovery, has
/// [`undiscovered`](Self::undiscovered): one stream. Only a Function Block's
/// declaration ([`of_block`](Self::of_block), [`declared`](Self::declared))
/// gives more, so a merger that has not discovered cannot send merged streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sysex8Support {
    streams: u8,
}

impl Sysex8Support {
    /// What a sender may assume with no Function Block Info Notification: one
    /// stream at a time.
    #[must_use]
    pub const fn undiscovered() -> Self {
        Self { streams: 1 }
    }

    /// The count a Function Block Info Notification declares: `0` the
    /// receiver supports no SysEx8 message, `1` no simultaneous ones, `2` to
    /// `255` that many Stream Ids.
    #[must_use]
    pub const fn declared(streams: u8) -> Self {
        Self { streams }
    }

    /// What `block` declares it supports as a receiver.
    #[must_use]
    pub const fn of_block(block: &FunctionBlock) -> Self {
        Self::declared(block.max_sysex8_streams())
    }

    /// The number of simultaneous streams; `0` is none.
    #[must_use]
    pub const fn streams(self) -> u8 {
        self.streams
    }
}

/// Why [`Sysex8Streams::open`] has no Stream Id to give.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum Sysex8Refused {
    /// The receiver supports no SysEx8 message (a count of 0).
    #[error("the receiver supports no System Exclusive 8 message")]
    Unsupported,
    /// Every Stream Id the receiver supports has a message open: the message
    /// waits for one to be released.
    #[error("every System Exclusive 8 Stream Id the receiver supports is in use")]
    Busy,
}

/// A Stream Id held by one open message, from [`Sysex8Streams::open`]. It is
/// not `Copy`: releasing it ([`Sysex8Streams::release`]) is the one way the
/// Id is free to open again, so two open messages never share one.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a slot holds a Stream Id until it is released"]
pub struct Sysex8Slot(u8);

impl Sysex8Slot {
    /// The Stream Id: below the receiver's count, and `0` for a receiver
    /// with one stream.
    #[must_use]
    pub const fn stream_id(&self) -> u8 {
        self.0
    }

    /// The message of `payload` on `group` and this slot's stream.
    #[must_use]
    pub const fn message<'a>(&self, group: MidiGroup, payload: &'a [u8]) -> Sysex8Message<'a> {
        Sysex8Message {
            group,
            stream_id: self.0,
            payload,
        }
    }
}

/// The Stream Ids one sender opens toward one receiver (§7.8): the lowest
/// free Id below the receiver's count, held until released.
///
/// ```
/// use tutti_midi_device::{Sysex8Refused, Sysex8Streams, Sysex8Support};
///
/// let mut streams = Sysex8Streams::new(Sysex8Support::declared(2));
/// let (a, b) = (streams.open().unwrap(), streams.open().unwrap());
/// assert_eq!((a.stream_id(), b.stream_id()), (0, 1));
/// assert_eq!(streams.open(), Err(Sysex8Refused::Busy));
/// streams.release(a);
/// assert_eq!(streams.open().unwrap().stream_id(), 0);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sysex8Streams {
    support: Sysex8Support,
    /// Bit `n` of the 256: Stream Id `n` is open.
    open: [u64; 4],
}

impl Sysex8Streams {
    /// A sender with no message open toward a receiver of `support`.
    #[must_use]
    pub const fn new(support: Sysex8Support) -> Self {
        Self {
            support,
            open: [0; 4],
        }
    }

    /// The receiver's support this sender follows.
    #[must_use]
    pub const fn support(&self) -> Sysex8Support {
        self.support
    }

    /// Follows the receiver's support as a new Function Block Info Notification
    /// declares it: Ids open above a lower count stay held until released.
    pub const fn rediscover(&mut self, support: Sysex8Support) {
        self.support = support;
    }

    /// Opens a message: the lowest Stream Id that is free and below the
    /// receiver's count.
    ///
    /// # Errors
    ///
    /// [`Sysex8Refused::Unsupported`] when the count is 0,
    /// [`Sysex8Refused::Busy`] when every Id below it is open.
    pub fn open(&mut self) -> Result<Sysex8Slot, Sysex8Refused> {
        if self.support.streams == 0 {
            return Err(Sysex8Refused::Unsupported);
        }
        let id = (0..self.support.streams)
            .find(|&id| !self.is_open(id))
            .ok_or(Sysex8Refused::Busy)?;
        self.open[usize::from(id / 64)] |= 1 << (id % 64);
        Ok(Sysex8Slot(id))
    }

    /// Frees the Id of a message that ended or was aborted.
    pub fn release(&mut self, slot: Sysex8Slot) {
        self.open[usize::from(slot.0 / 64)] &= !(1 << (slot.0 % 64));
    }

    fn is_open(&self, id: u8) -> bool {
        self.open[usize::from(id / 64)] & (1 << (id % 64)) != 0
    }
}

#[cfg(test)]
mod tests;
