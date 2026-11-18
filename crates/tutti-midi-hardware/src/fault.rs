//! [`InputFault`]: why an open input stopped delivering.

use core::num::NonZeroU32;
use tutti_rt::Cause;

/// An OS error number as an input fault carries it: 24 bits.
///
/// A number past [`Errno::MAX`] **saturates** to it (a value that large is no
/// errno of any OS this crate runs on, and keeping it reads as "out of range"
/// rather than as a different, wrong number); a number that is not positive is
/// `0`, "no number known".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Errno(u32);

impl Errno {
    /// The largest number an [`Errno`] holds: `2^24 - 1`.
    pub const MAX: u32 = 0x00FF_FFFF;

    /// No number known.
    pub const UNKNOWN: Self = Self(0);

    /// The OS error number `raw`, saturated to [`MAX`](Self::MAX); `0` for a
    /// number that is not positive.
    #[must_use]
    pub fn saturating(raw: i32) -> Self {
        Self(u32::try_from(raw).unwrap_or(0).min(Self::MAX))
    }

    /// The number, at most [`MAX`](Self::MAX).
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

/// Why an open input stopped delivering, read through
/// [`MidiSession::input_fault`](crate::MidiSession::input_fault). It is what an
/// input's pump or callback raised on its death latch: the first cause stays.
///
/// It packs into a [`Cause`] code: a kind in the top byte (never `0`, so a code
/// is never zero) and the payload in the low 24 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InputFault {
    /// The input's callback or pump thread panicked. Nothing it held is trusted
    /// after, so it does nothing more.
    Panicked,
    /// The OS call the pump waits for events in failed with this error.
    WaitFailed(Errno),
    /// Reading the next event failed on every try in a row, the last with this
    /// error ([`Errno::UNKNOWN`] when it had none).
    ReadFailed(Errno),
    /// The port the input reads is broken or gone: its descriptor reported an
    /// error or hang-up, or the device departed.
    PortBroken,
    /// The driver closed the device (winmm's `MIM_CLOSE`).
    DriverClosed,
}

const KIND_SHIFT: u32 = 24;
const PAYLOAD: u32 = Errno::MAX;

impl InputFault {
    const PANICKED: u32 = 1;
    const WAIT_FAILED: u32 = 2;
    const READ_FAILED: u32 = 3;
    const PORT_BROKEN: u32 = 4;
    const DRIVER_CLOSED: u32 = 5;

    /// The wait failed with the OS error `raw` ([`Errno::saturating`]).
    #[must_use]
    pub fn wait_failed(raw: i32) -> Self {
        Self::WaitFailed(Errno::saturating(raw))
    }

    /// The read failed with the OS error `raw`, if it had one.
    #[must_use]
    pub fn read_failed(raw: Option<i32>) -> Self {
        Self::ReadFailed(raw.map_or(Errno::UNKNOWN, Errno::saturating))
    }
}

impl Cause for InputFault {
    fn code(self) -> NonZeroU32 {
        let (kind, payload) = match self {
            Self::Panicked => (Self::PANICKED, 0),
            Self::WaitFailed(e) => (Self::WAIT_FAILED, e.get()),
            Self::ReadFailed(e) => (Self::READ_FAILED, e.get()),
            Self::PortBroken => (Self::PORT_BROKEN, 0),
            Self::DriverClosed => (Self::DRIVER_CLOSED, 0),
        };
        NonZeroU32::new((kind << KIND_SHIFT) | payload).expect("a kind is never zero")
    }

    fn from_code(code: NonZeroU32) -> Option<Self> {
        let code = code.get();
        let payload = code & PAYLOAD;
        let errno = Errno(payload);
        match (code >> KIND_SHIFT, payload) {
            (Self::PANICKED, 0) => Some(Self::Panicked),
            (Self::WAIT_FAILED, _) => Some(Self::WaitFailed(errno)),
            (Self::READ_FAILED, _) => Some(Self::ReadFailed(errno)),
            (Self::PORT_BROKEN, 0) => Some(Self::PortBroken),
            (Self::DRIVER_CLOSED, 0) => Some(Self::DriverClosed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
