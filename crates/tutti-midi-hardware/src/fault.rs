//! [`InputFault`]: why an open input stopped delivering.

use core::num::NonZeroU32;
use tutti_rt::Cause;

/// The largest OS error number an [`Errno`] holds: 24 bits.
const ERRNO_MAX: i32 = 0x00FF_FFFF;

tutti_units::ranged! {
    /// An OS error number as an input fault carries it: `1..=0xFF_FFFF`, the
    /// 24 bits a fault's payload has. "No error number known" is the absence of
    /// one (`Option<Errno>`), never `0`.
    #[derive(Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "i32", into = "i32"))]
    Errno(i32), ErrnoOutOfRange, "within 1..=0xFF_FFFF", |raw| raw >= 1 && raw <= ERRNO_MAX
}

impl Errno {
    /// The number an OS call returned, **saturated** to the largest an `Errno`
    /// holds (a number that large is no errno of any OS this crate runs on, and
    /// keeping it reads as "out of range" rather than as another number);
    /// `None` for a number that is not positive.
    #[must_use]
    pub fn clamped(raw: i32) -> Option<Self> {
        Self::new(raw.min(ERRNO_MAX))
    }
}

/// Why an open input stopped delivering, read through
/// [`MidiSession::input_fault`](crate::MidiSession::input_fault). It is what an
/// input's pump or callback raised on its death latch: the first cause stays.
///
/// `#[non_exhaustive]`: a backend can die a way no variant names yet, and an
/// app that matches reads the rest as "the input is gone".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InputFault {
    /// The input's callback or pump thread panicked. Nothing it held is trusted
    /// after, so it does nothing more.
    Panicked,
    /// The OS call the pump waits for events in failed, with this error number
    /// when the OS gave one.
    WaitFailed(Option<Errno>),
    /// Reading the next event failed on every try in a row, the last with this
    /// error number when the OS gave one.
    ReadFailed(Option<Errno>),
    /// A descriptor of the port the input reads reported an error or hang-up
    /// (ALSA's `Wait::Broken`).
    PortBroken,
    /// The driver closed the device (winmm's `MIM_CLOSE`).
    DriverClosed,
    /// The input's worker could not give a buffer back to the driver (winmm),
    /// so the port goes quiet as its buffers fill.
    BufferLost,
}

const KIND_SHIFT: u32 = 24;
const PAYLOAD: u32 = 0x00FF_FFFF;

/// The code of kind `kind` with no payload; a kind is never `0`.
const fn kind(kind: u32) -> NonZeroU32 {
    match NonZeroU32::new(kind << KIND_SHIFT) {
        Some(code) => code,
        None => panic!("a kind is never zero"),
    }
}

const PANICKED: NonZeroU32 = kind(1);
const WAIT_FAILED: NonZeroU32 = kind(2);
const READ_FAILED: NonZeroU32 = kind(3);
const PORT_BROKEN: NonZeroU32 = kind(4);
const DRIVER_CLOSED: NonZeroU32 = kind(5);
const BUFFER_LOST: NonZeroU32 = kind(6);

/// The payload an optional error number packs to: `0` for none.
fn payload(errno: Option<Errno>) -> u32 {
    errno.map_or(0, |e| u32::try_from(e.get()).unwrap_or(0))
}

/// The error number a payload stands for: none for `0`.
fn errno_of(payload: u32) -> Option<Errno> {
    i32::try_from(payload).ok().and_then(Errno::new)
}

/// Packs a fault into a code: the kind in the top byte (never `0`, so a code is
/// never zero) and the payload in the low 24 bits, an error number or `0`. A
/// code whose kind is unknown, or whose payload is not `0` for a kind that has
/// none, is no fault: `from_code` answers `None`.
impl Cause for InputFault {
    fn code(self) -> NonZeroU32 {
        match self {
            Self::Panicked => PANICKED,
            Self::WaitFailed(e) => WAIT_FAILED | payload(e),
            Self::ReadFailed(e) => READ_FAILED | payload(e),
            Self::PortBroken => PORT_BROKEN,
            Self::DriverClosed => DRIVER_CLOSED,
            Self::BufferLost => BUFFER_LOST,
        }
    }

    fn from_code(code: NonZeroU32) -> Option<Self> {
        let raw = code.get();
        let (kind, payload) = (raw & !PAYLOAD, raw & PAYLOAD);
        let unit = |fault| (payload == 0).then_some(fault);
        match kind {
            k if k == PANICKED.get() => unit(Self::Panicked),
            k if k == WAIT_FAILED.get() => Some(Self::WaitFailed(errno_of(payload))),
            k if k == READ_FAILED.get() => Some(Self::ReadFailed(errno_of(payload))),
            k if k == PORT_BROKEN.get() => unit(Self::PortBroken),
            k if k == DRIVER_CLOSED.get() => unit(Self::DriverClosed),
            k if k == BUFFER_LOST.get() => unit(Self::BufferLost),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
