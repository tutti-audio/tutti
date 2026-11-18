//! All-zero VST3 SDK records, for a test to fill in as a C host writes
//! `Event e = {};`.

use vst3::Steinberg::Vst::{Event, ProcessContext};

mod sealed {
    pub trait Sealed {}
}

/// An SDK struct whose all-zero bit pattern is a valid value: plain integers,
/// floats, arrays of them, raw pointers and unions of those.
///
/// Sealed: only the records listed here implement it.
pub trait SdkRecord: sealed::Sealed + Sized {}

macro_rules! sdk_record {
    ($($ty:ty),+ $(,)?) => {$(
        impl sealed::Sealed for $ty {}
        impl SdkRecord for $ty {}
    )+};
}

sdk_record!(Event, ProcessContext);

/// The all-zero value of an SDK record.
#[must_use]
#[allow(
    unsafe_code,
    reason = "all-zero bytes is valid for every `SdkRecord`; see the trait"
)]
pub fn sdk_zeroed<T: SdkRecord>() -> T {
    // SAFETY: `SdkRecord` is implemented only for the records above, each made
    // of integers, floats, arrays of them, raw pointers and unions of those, for
    // which all-zero bytes is a valid value.
    unsafe { std::mem::zeroed() }
}
