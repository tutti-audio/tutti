//! Small conversion helpers for VST3 C buffers and raw-byte types.

use vst3::Steinberg::TUID;

/// Converts a 16-byte `Guid` (`[u8; 16]`) to a `TUID` (`[i8; 16]`).
#[must_use]
pub(crate) fn guid_as_tuid(guid: &vst3::com_scrape_types::Guid) -> TUID {
    std::array::from_fn(|i| guid[i] as i8)
}

/// The integer type the `vst3` bindings give every SDK enum constant:
/// `DefaultEnumType`, which is `c_int` on Windows and `c_uint` everywhere else.
///
/// The bindings do not re-export `DefaultEnumType` (their `support` module is
/// private), so it is named here through `MediaTypes`, one of the public
/// aliases they define as exactly that type. Naming it through an alias rather
/// than mirroring the `cfg` means a change in the bindings surfaces as a type
/// error at [`sdk_enum_i32`]'s call sites, not as a silently stale copy.
pub(crate) type SdkEnum = vst3::Steinberg::Vst::MediaTypes;

/// Widens an SDK enum constant to the `int32` the ABI fields carrying it use
/// (`BusInfo::mediaType`, `ProcessSetup::processMode`, `IBStream::seek`'s
/// `mode`, …).
///
/// The one place this crate spells that conversion, so the platform split
/// lives here rather than at every call site: on Windows `SdkEnum` already *is*
/// `i32` and the cast is a no-op; elsewhere it is a real `u32 -> i32`. Every
/// value passed is a small SDK ordinal, so it never wraps.
#[allow(
    clippy::unnecessary_cast,
    reason = "a no-op on Windows, where `DefaultEnumType` is `c_int`; load-bearing on every other target"
)]
#[must_use]
pub const fn sdk_enum_i32(v: SdkEnum) -> i32 {
    v as i32
}

/// [`sdk_enum_i32`] for the `uint32` fields and flag words that carry an SDK
/// enum constant (`PhysicalUITypeIDs`, `ProcessContext` flags, …). A no-op on
/// Unix, where `SdkEnum` is `u32`; a sign change on Windows, where it is `i32`.
#[allow(
    clippy::unnecessary_cast,
    reason = "a no-op on Unix, where `DefaultEnumType` is `c_uint`; load-bearing on Windows"
)]
#[must_use]
pub const fn sdk_enum_u32(v: SdkEnum) -> u32 {
    v as u32
}

/// An SDK struct for which the all-zero bit pattern is a valid value: plain
/// integers, floats, arrays of them, raw pointers and unions of those.
///
/// Sealed: only the structs this crate lists below implement it. They are the
/// out-parameter and event records of the SDK, which a plugin fills in or the
/// host fills field by field.
pub(crate) trait Zeroable: private::Sealed + Sized {}

mod private {
    pub(crate) trait Sealed {}
}

macro_rules! zeroable {
    ($($ty:ty),+ $(,)?) => {$(
        impl private::Sealed for $ty {}
        impl Zeroable for $ty {}
    )+};
}

zeroable!(
    vst3::Steinberg::PFactoryInfo,
    vst3::Steinberg::PClassInfo,
    vst3::Steinberg::PClassInfoW,
    vst3::Steinberg::Vst::ParameterInfo,
    vst3::Steinberg::Vst::NoteExpressionTypeInfo,
    vst3::Steinberg::Vst::KeyswitchInfo,
    vst3::Steinberg::Vst::ProgramListInfo,
    vst3::Steinberg::Vst::UnitInfo,
    vst3::Steinberg::Vst::BusInfo,
    vst3::Steinberg::Vst::RepresentationInfo,
    vst3::Steinberg::Vst::Event,
    vst3::Steinberg::Vst::ProcessContext,
    vst3::Steinberg::Vst::AudioBusBuffers,
);

/// The all-zero value of an SDK struct.
///
/// What a C host writes as `ParameterInfo info = {};` before handing it to a
/// plugin to fill in.
#[must_use]
pub(crate) fn zeroed<T: Zeroable>() -> T {
    // SAFETY: `Zeroable` is implemented only for the SDK structs listed above,
    // each made of integers, floats, arrays of them, raw pointers and unions of
    // those, for which all-zero bytes is a valid value (null pointers, `0`,
    // `0.0`).
    unsafe { std::mem::zeroed() }
}
