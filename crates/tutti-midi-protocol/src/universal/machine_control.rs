//! MIDI Machine Control (RP-013): Universal Real Time System Exclusive
//! sub-ID#1 `06`, the commands a controller sends, and `07`, the responses a
//! device sends back.
//!
//! [`read()`](fn@crate::read) turns a message into
//! [`Universal::MachineControl`](crate::Universal::MachineControl) (a
//! [`MmcCommands`] list) or
//! [`UniversalRead::MachineControlResponse`](crate::UniversalRead::MachineControlResponse)
//! (a [`MmcFields`] list). Each list holds the validated bytes of one message and
//! yields typed [`MmcCommand`]s or [`MmcInfoField`]s; it allocates nothing and is
//! `Copy`.
//!
//! # Addressing
//!
//! A command's device ID is its destination (RP-013 p.5): `7F` is the
//! all-call, any other ID a device or a **group**, which is an ID no device has
//! as its own and that GROUP assigns devices to. Both are a
//! [`Recipient`]. A response's device ID is its source and
//! is always one device's own ([`SysexDeviceId`]); a
//! response from `7F` does not read.
//!
//! # What is read
//!
//! Every command of RP-013 pp.15 and 20 to 43 and every response and
//! information field of pp.16 and 44 to 74 is a variant of [`MmcCommand`] or
//! [`MmcInfoField`]. A code the specification leaves undefined is kept as
//! [`MmcReservedCommand`] or [`MmcOtherField`], skipped by the length its code gives
//! (RP-013 p.6), including the extension sets (`00 nn`, `00 00 nn`). A message
//! that is not a whole number of well-formed commands or fields, or is longer
//! than [`MMC_MAX_DATA`] bytes, is [`UniversalRead::Unread`](crate::UniversalRead::Unread).
//!
//! # Writing
//!
//! [`MmcCommands::payload`] and [`MmcFields::payload`] build the payload of a message,
//! the bytes between `F0` and `F7`, which tutti-ump's `Sysex7Packets` packs.
//!
//! ```
//! use tutti_midi_protocol::{MmcCommand, MmcCommands, Recipient};
//!
//! let mut commands = MmcCommands::new();
//! commands.push(&MmcCommand::DeferredPlay).unwrap();
//! assert_eq!(commands.payload(Recipient::AllCall), [0x7F, 0x7F, 0x06, 0x03]);
//! ```

mod command;
mod field;
mod read;
mod segmentation;
mod values;
mod write;

use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use tutti_ump::first_non_seven_bit;

use crate::universal::{Recipient, SysexDeviceId};

pub use command::{
    MmcArithmetic, MmcCommand, MmcEvent, MmcGroup, MmcLocate, MmcMaskedWrite, MmcMove,
    MmcProcedure, MmcReservedCommand, MmcSystemMaster, MmcUpdate,
};
pub use field::*;
pub use segmentation::{
    MMC_MAX_SEGMENTED, MMC_MAX_SEGMENTS, MmcCommandReassembler, MmcReassembler,
    MmcResponseReassembler, MmcSegmentError, MmcSegments, MmcSegmentsRefused, MmcString,
};
pub use values::{
    MMC_MAX_TRACK_BITMAP_BYTES, MMC_SEGMENT_BYTES, MmcAudioTrack, MmcCodeStatus, MmcDirection,
    MmcExtension, MmcFragment, MmcMantissa, MmcName, MmcPosition, MmcSegmentBytes, MmcSelection,
    MmcShift, MmcShortTimeCode, MmcSign, MmcSlot, MmcSpeed, MmcStep, MmcStepCount, MmcSubframes,
    MmcTimeCode, MmcTimeLabel, MmcTimeTail, MmcTrack, MmcTrackBitmap, MmcUserBitFlags, MmcUserBits,
    NotMmcAudioTrack, NotMmcMantissa, NotMmcPosition, NotMmcShift, NotMmcSlot, NotMmcStepCount,
    NotMmcSubframes,
};

pub(crate) use read::{commands, fields, truncated_segment};

/// The most bytes a message holds after its sub-ID (RP-013 p.2).
pub const MMC_MAX_DATA: usize = 48;

/// Bytes a Machine Control message cannot hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum MmcBytesRefused {
    /// More than [`MMC_MAX_DATA`] bytes.
    #[error("a Machine Control message holds at most {MMC_MAX_DATA} bytes")]
    TooLong,
    /// A byte above 127, which no data byte is.
    #[error("a Machine Control message holds data bytes, each below 128")]
    NotSevenBit,
}

/// Up to [`MMC_MAX_DATA`] data bytes, each below 128.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcBytes {
    buf: [u8; MMC_MAX_DATA],
    len: u8,
}

impl MmcBytes {
    pub(crate) const EMPTY: Self = Self {
        buf: [0; MMC_MAX_DATA],
        len: 0,
    };

    pub(crate) fn from_slice(bytes: &[u8]) -> Option<Self> {
        let mut out = Self::EMPTY;
        out.extend(bytes).ok()?;
        Some(out)
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf[..usize::from(self.len)]
    }

    pub(crate) const fn len(&self) -> u8 {
        self.len
    }

    pub(crate) fn push(&mut self, byte: u8) -> Result<(), MmcBytesRefused> {
        self.extend(&[byte])
    }

    pub(crate) fn extend(&mut self, bytes: &[u8]) -> Result<(), MmcBytesRefused> {
        let at = usize::from(self.len);
        let end = at + bytes.len();
        let len = u8::try_from(end)
            .ok()
            .filter(|&len| usize::from(len) <= MMC_MAX_DATA)
            .ok_or(MmcBytesRefused::TooLong)?;
        if first_non_seven_bit(bytes).is_some() {
            return Err(MmcBytesRefused::NotSevenBit);
        }
        self.buf[at..end].copy_from_slice(bytes);
        self.len = len;
        Ok(())
    }
}

mod sealed {
    use super::{MmcBytes, MmcBytesRefused, read};

    /// What a [`MmcSequence`](super::MmcSequence) can hold: read from and written to
    /// the wire.
    pub trait Sealed: Sized {
        fn parse(input: &mut &[u8]) -> read::Res<Self>;
        fn write(&self, out: &mut MmcBytes) -> Result<(), MmcBytesRefused>;
    }
}

/// An item of a [`MmcSequence`]: a [`MmcCommand`], an [`MmcInfoField`], a [`MmcName`] or a
/// [`Recipient`]. Sealed.
pub trait MmcItem: sealed::Sealed {}

macro_rules! item {
    ($ty:ty, $parse:path, $write:path) => {
        impl sealed::Sealed for $ty {
            fn parse(input: &mut &[u8]) -> read::Res<Self> {
                $parse(input)
            }
            fn write(&self, out: &mut MmcBytes) -> Result<(), MmcBytesRefused> {
                $write(*self, out)
            }
        }
        impl MmcItem for $ty {}
    };
}

impl sealed::Sealed for MmcCommand {
    fn parse(input: &mut &[u8]) -> read::Res<Self> {
        read::command(input)
    }
    fn write(&self, out: &mut MmcBytes) -> Result<(), MmcBytesRefused> {
        write::command(self, out)
    }
}
impl MmcItem for MmcCommand {}

impl sealed::Sealed for MmcInfoField {
    fn parse(input: &mut &[u8]) -> read::Res<Self> {
        read::info_field(input)
    }
    fn write(&self, out: &mut MmcBytes) -> Result<(), MmcBytesRefused> {
        write::info_field(self, out)
    }
}
impl MmcItem for MmcInfoField {}

item!(MmcName, read::name, write::name);
item!(Recipient, read::recipient, write::recipient);

/// A bounded list of `T`, held as the bytes it is on the wire: at most
/// [`MMC_MAX_DATA`] of them. What it holds is validated when it is built or read,
/// so [`iter`](Self::iter) cannot fail.
pub struct MmcSequence<T> {
    bytes: MmcBytes,
    item: PhantomData<fn() -> T>,
}

/// The commands of one Machine Control Command message.
pub type MmcCommands = MmcSequence<MmcCommand>;
/// The information fields of one Machine Control Response message, or the
/// fields a WRITE loads.
pub type MmcFields = MmcSequence<MmcInfoField>;
/// A list of field names, as READ, UPDATE and RESPONSE ERROR carry.
pub type MmcNames = MmcSequence<MmcName>;
/// A list of device IDs, as GROUP carries.
pub type MmcDevices = MmcSequence<Recipient>;

impl<T> Clone for MmcSequence<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for MmcSequence<T> {}
impl<T> PartialEq for MmcSequence<T> {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}
impl<T> Eq for MmcSequence<T> {}
impl<T> Hash for MmcSequence<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bytes.hash(state);
    }
}

impl<T: MmcItem + fmt::Debug> fmt::Debug for MmcSequence<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: MmcItem> Default for MmcSequence<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> MmcSequence<T> {
    pub(crate) const fn from_bytes(bytes: MmcBytes) -> Self {
        Self {
            bytes,
            item: PhantomData,
        }
    }
}

impl<T: MmcItem> MmcSequence<T> {
    /// The empty list.
    #[must_use]
    pub const fn new() -> Self {
        Self::from_bytes(MmcBytes::EMPTY)
    }

    /// Adds `item` at the end.
    ///
    /// # Errors
    ///
    /// [`MmcBytesRefused`] when it does not fit in [`MMC_MAX_DATA`] bytes or holds a
    /// byte above 127; the list is unchanged.
    pub fn push(&mut self, item: &T) -> Result<(), MmcBytesRefused> {
        let mut grown = self.bytes;
        item.write(&mut grown)?;
        self.bytes = grown;
        Ok(())
    }

    /// The items, in order.
    #[must_use]
    pub fn iter(&self) -> MmcItems<'_, T> {
        MmcItems {
            rest: self.bytes.as_slice(),
            item: PhantomData,
        }
    }

    /// The list's wire bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.len() == 0
    }
}

impl<'a, T: MmcItem> IntoIterator for &'a MmcSequence<T> {
    type Item = T;
    type IntoIter = MmcItems<'a, T>;

    fn into_iter(self) -> MmcItems<'a, T> {
        self.iter()
    }
}

/// The items of a [`MmcSequence`].
pub struct MmcItems<'a, T> {
    rest: &'a [u8],
    item: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for MmcItems<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MmcItems")
            .field("rest", &self.rest.len())
            .finish()
    }
}

impl<'a, T> MmcItems<'a, T> {
    /// The items of `rest`, which the caller has validated.
    pub(crate) const fn over(rest: &'a [u8]) -> Self {
        Self {
            rest,
            item: PhantomData,
        }
    }
}

impl<T: MmcItem> Iterator for MmcItems<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.rest.is_empty() {
            return None;
        }
        // The bytes were validated as a whole list when the list was made.
        match T::parse(&mut self.rest) {
            Ok(item) => Some(item),
            Err(_) => {
                self.rest = &[];
                None
            }
        }
    }
}

impl MmcCommands {
    /// STOP alone.
    #[must_use]
    pub fn stop() -> Self {
        Self::of(&[MmcCommand::Stop])
    }

    /// PLAY alone.
    #[must_use]
    pub fn play() -> Self {
        Self::of(&[MmcCommand::Play])
    }

    /// LOCATE (target) to `target`, then DEFERRED PLAY when `then_play`: the
    /// device plays once it has got there. At most 9 bytes, far inside
    /// [`MMC_MAX_DATA`], so it cannot fail.
    #[must_use]
    pub fn locate(target: MmcPosition, then_play: bool) -> Self {
        let locate = MmcCommand::Locate(MmcLocate::Target(target));
        if then_play {
            Self::of(&[locate, MmcCommand::DeferredPlay])
        } else {
            Self::of(&[locate])
        }
    }

    /// The list of `commands`, which the callers bound to a few bytes each.
    fn of(commands: &[MmcCommand]) -> Self {
        let mut list = Self::new();
        for command in commands {
            let pushed = list.push(command);
            debug_assert!(pushed.is_ok(), "the callers' commands fit 48 bytes");
        }
        list
    }

    /// The payload of the Command message to `to`: a device, a group or all
    /// (`7F device 06 commands`), the bytes between `F0` and `F7`. It goes out
    /// as SysEx7 through [`Sysex7Packets::from_payload`](tutti_ump::Sysex7Packets::from_payload).
    #[must_use]
    pub fn payload(&self, to: Recipient) -> MmcPayload {
        MmcPayload::framed(to.device_byte(), 0x06, &self.bytes)
    }
}

impl MmcFields {
    /// The payload of the Response message from `source` (`7F source 07
    /// fields`), the bytes between `F0` and `F7`.
    #[must_use]
    pub fn payload(&self, source: SysexDeviceId) -> MmcPayload {
        MmcPayload::framed(source.get(), 0x07, &self.bytes)
    }
}

/// The payload of one Machine Control message: `7F`, a device ID, the
/// sub-ID and up to [`MMC_MAX_DATA`] data bytes, held inline so that building
/// and sending one allocates nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcPayload {
    buf: [u8; 3 + MMC_MAX_DATA],
    len: u8,
}

impl MmcPayload {
    /// `7F`, the device ID, the sub-ID and the data.
    fn framed(device: u8, sub_id: u8, data: &MmcBytes) -> Self {
        let mut buf = [0; 3 + MMC_MAX_DATA];
        buf[..3].copy_from_slice(&[0x7F, device, sub_id]);
        buf[3..3 + usize::from(data.len())].copy_from_slice(data.as_slice());
        Self {
            buf,
            // `MmcBytes` holds at most `MMC_MAX_DATA` bytes.
            len: 3 + data.len(),
        }
    }

    /// The bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..usize::from(self.len)]
    }
}

impl core::ops::Deref for MmcPayload {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for MmcPayload {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<const N: usize> PartialEq<[u8; N]> for MmcPayload {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.as_bytes() == other
    }
}

#[cfg(test)]
mod tests;
