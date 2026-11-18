//! What cpal reports, in the engine's vocabulary.
//!
//! The engine's block seam knows no backend: a stream's faults are classified
//! ([`BackendErrorKind`]) and its latency is a bare `Option<Duration>`. This
//! module is where cpal's stream errors and callback timestamps are read into
//! them, once, for the output driver and the capture stream alike.
//! It also holds the offset-binary sample formats (`u8`, `u16`, `u32`), converted
//! by the rule in `Sample` on the signed format of their width.
//!
//! cpal stamps each callback with two instants
//! ([`OutputStreamTimestamp`](cpal::OutputStreamTimestamp): the callback and
//! the predicted playback; [`InputStreamTimestamp`](cpal::InputStreamTimestamp):
//! the callback and the capture). Their difference is the buffering the device
//! adds in that direction. A backend that cannot say gives a timestamp with no
//! difference to read (the playback before the callback, say), and that is
//! `None`, "not reported", never zero.

use std::time::{Duration, Instant};

use tutti_buffers::{Float, Sample};
use tutti_engine::{BackendErrorKind, StreamFaults};

/// An offset-binary unsigned device format: the signed format of its width
/// with the sign bit flipped, so silence is mid-scale.
///
/// The conversion between a graph's float and the device is `Sample`'s rule on
/// the signed format, then the flip: one rule for every device format.
pub(crate) trait OffsetBinary: cpal::SizedSample {
    /// The signed format of the same width.
    type Native: Sample;
    /// The unsigned value of `signed`, its sign bit flipped.
    fn from_signed(signed: Self::Native) -> Self;
    /// The signed value of `self`, its sign bit flipped back.
    #[cfg(any(feature = "capture", test))]
    fn to_signed(self) -> Self::Native;
}

macro_rules! offset_binary {
    ($($unsigned:ty => $signed:ty),+) => {$(
        impl OffsetBinary for $unsigned {
            type Native = $signed;
            #[inline]
            fn from_signed(signed: $signed) -> Self {
                (signed as $unsigned) ^ (1 << (<$unsigned>::BITS - 1))
            }
            #[cfg(any(feature = "capture", test))]
            #[inline]
            fn to_signed(self) -> $signed {
                (self ^ (1 << (<$unsigned>::BITS - 1))) as $signed
            }
        }
    )+};
}

offset_binary!(u8 => i8, u16 => i16, u32 => i32);

/// The offset-binary device sample of the graph's `x`.
#[inline]
pub(crate) fn to_offset<F: Float, U: OffsetBinary>(x: F) -> U {
    U::from_signed(x.convert_to::<U::Native>())
}

/// What a native ([`Sample`]) device sample reads as in the graph's float.
#[cfg(feature = "capture")]
#[inline]
pub(crate) fn from_native<S: Sample, F: Float>(sample: S) -> F {
    sample.convert_to::<F>()
}

/// What an offset-binary device sample reads as in the graph's float.
#[cfg(feature = "capture")]
#[inline]
pub(crate) fn from_offset<U: OffsetBinary, F: Float>(sample: U) -> F {
    sample.to_signed().convert_to::<F>()
}

/// How a stream's faults read cpal's `err`: `DeviceNotAvailable` is the device
/// gone; `DeviceChanged` and `RealtimeDenied` say the stream keeps playing;
/// anything else is a backend error that may be transient.
pub(crate) fn backend_kind(err: &cpal::Error) -> BackendErrorKind {
    match err.kind() {
        cpal::ErrorKind::DeviceNotAvailable => BackendErrorKind::DeviceGone,
        cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied => {
            BackendErrorKind::Survivable
        }
        _ => BackendErrorKind::Other,
    }
}

/// Records cpal's `err` into `faults`: what a stream's error callback calls.
pub(crate) fn record(faults: &StreamFaults, err: &cpal::Error) {
    faults.record(backend_kind(err), err, Instant::now());
}

/// The delay an output callback's timestamp reports: the playback instant
/// after the callback's. `None` if the backend gives a playback before the
/// callback.
pub(crate) fn output_delay(stamp: &cpal::OutputStreamTimestamp) -> Option<Duration> {
    stamp.playback.checked_duration_since(stamp.callback)
}

/// The delay an input callback's timestamp reports: the callback after the
/// capture instant. `None` if the backend gives a capture after the callback.
#[cfg(feature = "capture")]
pub(crate) fn input_delay(stamp: &cpal::InputStreamTimestamp) -> Option<Duration> {
    stamp.callback.checked_duration_since(stamp.capture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_engine::StreamFaultKind;

    /// cpal's error kinds map to the stream's three readings: a lost device is
    /// a disconnect at once, a route change or a refused real-time promotion
    /// records nothing, and any other error is a backend fault that is not yet
    /// a disconnect, with cpal's own message.
    #[test]
    fn cpal_error_kinds_read_as_the_streams_three_faults() {
        let faults = StreamFaults::new();
        record(&faults, &cpal::Error::new(cpal::ErrorKind::DeviceChanged));
        record(&faults, &cpal::Error::new(cpal::ErrorKind::RealtimeDenied));
        assert!(!faults.is_faulted(), "a survivable change is no fault");

        record(
            &faults,
            &cpal::Error::with_message(cpal::ErrorKind::Xrun, "xrun"),
        );
        let fault = faults.last().expect("recorded");
        assert_eq!(fault.kind, StreamFaultKind::Backend);
        assert!(fault.message.contains("xrun"), "{:?}", fault.message);
        assert!(!faults.is_disconnected());

        record(
            &faults,
            &cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable),
        );
        assert!(faults.is_disconnected());
        assert_eq!(
            faults.last().map(|f| f.kind),
            Some(StreamFaultKind::Disconnected)
        );
    }
}
