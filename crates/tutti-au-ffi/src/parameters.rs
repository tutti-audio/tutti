//! The parameter-shaped property queries whose request and answer share one
//! struct, and the info record whose name is a CoreFoundation string.

use coreaudio_sys as sys;

use crate::cf::{CfString, take_string};
use crate::error::Result;
use crate::property::{
    Element, ParameterClumpName, ParameterIdName, ParameterInfo, ParameterStringFromValue,
    ParameterValueFromString, Scope,
};
use crate::unit::Unit;

/// `AudioUnitParameterNameInfo`, the request `ParameterClumpName` and
/// `kAudioUnitProperty_ParameterIDName` share: `{ UInt32 inID; SInt32 inDesiredLength; CFStringRef
/// outName; }`. Hand-declared because `coreaudio-sys` does not export it; the
/// layout is pinned by `name_request_matches_the_c_abi`. The desired length is
/// read by `kAudioUnitProperty_ParameterIDName` alone.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct NameRequest {
    in_id: u32,
    in_desired_length: i32,
    out_name: sys::CFStringRef,
}

impl crate::plain::Sealed for NameRequest {}
// SAFETY: a `u32`, an `i32` and a raw pointer in a `#[repr(C)]` struct: every
// bit pattern, zero included, is valid.
unsafe impl crate::plain::Plain for NameRequest {}

impl crate::plain::Sealed for sys::AudioUnitParameterStringFromValue {}
// SAFETY: a `u32` and two raw pointers in a `#[repr(C)]` struct.
unsafe impl crate::plain::Plain for sys::AudioUnitParameterStringFromValue {}

impl crate::plain::Sealed for sys::AudioUnitParameterValueFromString {}
// SAFETY: a `u32`, a raw pointer and an `f32` in a `#[repr(C)]` struct.
unsafe impl crate::plain::Plain for sys::AudioUnitParameterValueFromString {}

impl Unit {
    /// `kAudioUnitProperty_ParameterInfo` for parameter `id` at `scope`, with
    /// the display name resolved: the CF name when the record carries one (and
    /// it is released here), otherwise the fixed C-string field.
    ///
    /// # Errors
    /// The `OSStatus` of the read.
    pub fn parameter_info(
        &self,
        scope: Scope,
        id: u32,
    ) -> Result<(sys::AudioUnitParameterInfo, String)> {
        let raw = self.get::<ParameterInfo>(scope, Element(id))?;
        let name = if raw.flags & sys::kAudioUnitParameterFlag_HasCFNameString != 0
            && !raw.cfNameString.is_null()
        {
            // SAFETY: with `HasCFNameString` set the record's `cfNameString` is
            // a CFString the caller owns one retain of (the header's rule).
            unsafe { take_string(raw.cfNameString) }.unwrap_or_default()
        } else {
            // `name` is `[c_char; 52]`, NUL-terminated.
            tutti_plugin_types::read_c_chars(&raw.name)
        };
        Ok((raw, name))
    }

    /// The unit's own text for `value` of parameter `id`, or `None` when it
    /// does not format one.
    pub fn parameter_string_from_value(
        &self,
        scope: Scope,
        element: Element,
        id: u32,
        value: f32,
    ) -> Option<String> {
        // `inValue` is a *pointer* to the value; borrowing the parameter keeps
        // it alive across the call. A null `inValue` would ask for the unit's
        // own current value; this always names one explicitly.
        let request = sys::AudioUnitParameterStringFromValue {
            inParamID: id,
            inValue: &value,
            outString: std::ptr::null(),
        };
        // The property carries the request in and the answer back out in the
        // same struct.
        let answer = self
            .get_in_out::<ParameterStringFromValue>(scope, element, request)
            .ok()?;
        // SAFETY: documented Copy rule: "the outString ... must be released by
        // the caller", so a non-null answer is one retain we own.
        unsafe { take_string(answer.outString) }
    }

    /// The value the unit parses from `text` for parameter `id`, or `None` when
    /// it declines or answers a non-finite figure.
    pub fn parameter_value_from_string(
        &self,
        scope: Scope,
        element: Element,
        id: u32,
        text: &str,
    ) -> Option<f32> {
        // The CFString is borrowed by the unit for the duration of the call
        // only, so a host-owned temporary is correct.
        let string = CfString::new(text);
        let request = sys::AudioUnitParameterValueFromString {
            inParamID: id,
            inString: string.as_raw(),
            // NaN rather than 0.0 as the sentinel: a unit that returned `noErr`
            // without writing the field would otherwise read as a genuine
            // parse of "0".
            outValue: f32::NAN,
        };
        let answer = self
            .get_in_out::<ParameterValueFromString>(scope, element, request)
            .ok()?;
        answer.outValue.is_finite().then_some(answer.outValue)
    }

    /// The unit's name for parameter group `clump`, or `None`.
    pub fn parameter_clump_name(
        &self,
        scope: Scope,
        element: Element,
        clump: u32,
    ) -> Option<String> {
        let request = NameRequest {
            in_id: clump,
            in_desired_length: 0,
            out_name: std::ptr::null(),
        };
        let answer = self
            .get_in_out::<ParameterClumpName>(scope, element, request)
            .ok()?;
        // SAFETY: Copy rule, as for every CF object retrieved from an AU
        // property.
        unsafe { take_string(answer.out_name) }
    }

    /// The unit's name for parameter `id` cut to at most `length` characters,
    /// or `None` when it refuses or answers empty.
    pub fn parameter_short_name(
        &self,
        scope: Scope,
        element: Element,
        id: u32,
        length: i32,
    ) -> Option<String> {
        let request = NameRequest {
            in_id: id,
            in_desired_length: length,
            out_name: std::ptr::null(),
        };
        let answer = self
            .get_in_out::<ParameterIdName>(scope, element, request)
            .ok()?;
        // SAFETY: Copy rule, as for every CF object retrieved from an AU
        // property.
        unsafe { take_string(answer.out_name) }.filter(|name| !name.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// The name request is hand-declared, so its layout is pinned here:
    /// `{ UInt32; SInt32; CFStringRef }` is 16 bytes with the pointer at offset 8.
    #[test]
    fn name_request_matches_the_c_abi() {
        assert_eq!(size_of::<NameRequest>(), 16);
        assert_eq!(std::mem::offset_of!(NameRequest, in_id), 0);
        assert_eq!(std::mem::offset_of!(NameRequest, in_desired_length), 4);
        assert_eq!(std::mem::offset_of!(NameRequest, out_name), 8);
    }

    #[test]
    fn the_name_field_is_a_whole_c_string_buffer() {
        // The fallback decode reads every byte of the fixed field.
        let info = sys::AudioUnitParameterInfo::default();
        assert_eq!(info.name.len(), 52);
    }
}
