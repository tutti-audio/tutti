//! The host's `audioMaster` dispatch and the two string helpers the host and the plugin side share.
//!
//! The plugin-side entry points (`dispatch`, `process_replacing`, ...) are
//! `tutti-vst2-author`'s: this crate is the host half.

#![doc(hidden)]

use std::cell::Cell;
use std::os::raw::c_void;
use std::ptr::NonNull;

use tutti_plugin_types::{c_str_prefix, read_c_str, write_c_str};

use crate::{
    api::{self, AEffect, Supported, TimeInfo, consts::*},
    host::Host,
};

/// `kVstLangEnglish`, the host's one language: the values are 1-based, so `0` names none.
const LANGUAGE_ENGLISH: isize = 1;

/// Copy a string into a destination buffer.
///
/// Returns `0` (not handled) for a null `dst`.
///
/// `max` counts the NUL, as the SDK's `kVstMax*Len` constants do. What is written is
/// [`c_str_prefix`]`(src, max)` and a NUL: always terminated, cut at a char boundary, and
/// nothing is written past that NUL, so a destination that is shorter than `max` but holds
/// the string is not overrun by a clearing pass.
///
/// # Safety
///
/// A non-null `dst` is writable for `max` bytes, and nothing else accesses them during the
/// call.
pub unsafe fn copy_string(dst: *mut c_void, src: &str, max: usize) -> isize {
    if dst.is_null() || max == 0 {
        return 0;
    }
    let text = c_str_prefix(src, max);
    // SAFETY: `dst` is non-null and the opcode's buffer, which the VST2 ABI has hold at
    // least `max` writable bytes (the callers pass the SDK's fixed size for the opcode;
    // a host that offers less is outside the ABI, and this fn does not and cannot check
    // it). The slice is `text.len() + 1 <= max` bytes of it, so only the text and its NUL
    // are formed and written, and a plugin's buffer does not alias the `&mut`.
    let out = unsafe { std::slice::from_raw_parts_mut(dst.cast::<u8>(), text.len() + 1) };
    write_c_str(out, text);

    1 // Success
}

/// Dispatch one `audioMaster` opcode to the host.
///
/// Takes `&dyn Host`, not `&mut`: every `Host` method takes `&self`, and this
/// runs on the plugin's audio thread as often as the UI thread — see
/// [`crate::host::PluginLoader::load`] for why no lock guards it.
///
/// # Safety
///
/// `ptr` is what the plugin passed `audioMasterCallback` for `opcode`: the opcode's
/// structure or string, valid for the call, or null.
pub(crate) unsafe fn host_dispatch(
    host: &dyn Host,
    _effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    opt: f32,
) -> isize {
    use crate::host::OpCode;

    let opcode_number = opcode;
    let opcode = OpCode::try_from(opcode);
    match opcode {
        Ok(OpCode::Version) => return 2400,
        Ok(OpCode::Automate) => host.automate(index, opt),
        Ok(OpCode::BeginEdit) => host.begin_edit(index),
        Ok(OpCode::EndEdit) => host.end_edit(index),

        Ok(OpCode::Idle) => host.idle(),

        // ...
        // `audioMasterCanDo`: 1 = yes, -1 = no, 0 = don't know. A null string
        // is a malformed query and answers "don't know".
        Ok(OpCode::CanDo) => {
            if ptr.is_null() {
                host.refused_callback(opcode_number);
                return 0;
            }
            // SAFETY: `ptr` is non-null and the plugin's NUL-terminated query string, valid
            // for the call (this fn's contract).
            let query = unsafe { read_string(ptr) };
            return match host.can_do(&query) {
                Supported::Yes => 1,
                Supported::Maybe => 0,
                Supported::No => -1,
                Supported::Custom(n) => n,
            };
        }

        Ok(OpCode::GetVendorVersion) => return host.get_info().0,
        Ok(OpCode::GetVendorString) => {
            // SAFETY: the plugin's buffer for the string, of the SDK's size for the opcode.
            return unsafe { copy_string(ptr, &host.get_info().1, MAX_VENDOR_STR_LEN) };
        }
        Ok(OpCode::GetProductString) => {
            // SAFETY: as above.
            return unsafe { copy_string(ptr, &host.get_info().2, MAX_PRODUCT_STR_LEN) };
        }
        Ok(OpCode::ProcessEvents) => {
            // A null or misaligned `VstEvents` is a malformed call: not handled.
            let events = NonNull::new(ptr.cast::<api::Events>()).filter(|e| e.is_aligned());
            let Some(events) = events else {
                host.refused_callback(opcode_number);
                return 0;
            };
            // SAFETY: VST2 ABI: for `audioMasterProcessEvents`, `ptr` is the plugin's
            // `VstEvents`, valid for the call; non-null and aligned (checked above). The
            // pointer keeps the provenance of the plugin's whole list, and what it holds
            // beyond the header (the count's worth of event pointers) is the plugin's word,
            // which `PluginEvents::iter` bounds as it can.
            host.process_plugin_events(unsafe { api::PluginEvents::new(events) });
        }

        Ok(OpCode::GetTime) => {
            return match host.get_time_info(value as i32) {
                None => 0,
                Some(result) => {
                    thread_local! {
                        static TIME_INFO: Cell<TimeInfo> =
                            Cell::new(TimeInfo::default());
                    }
                    TIME_INFO.with(|time_info| {
                        (*time_info).set(result);
                        time_info.as_ptr() as isize
                    })
                }
            };
        }
        Ok(OpCode::GetBlockSize) => return host.get_block_size(),

        // `audioMasterGetSampleRate` returns the rate via the intptr result
        // (same channel as `GetBlockSize`), truncated from f32 to isize.
        Ok(OpCode::GetSampleRate) => return host.get_sample_rate() as isize,
        Ok(OpCode::SizeWindow) => return host.size_window(index, value) as isize,
        Ok(OpCode::IOChanged) => return host.io_changed() as isize,
        Ok(OpCode::GetInputLatency) => return host.get_input_latency(),
        Ok(OpCode::GetOutputLatency) => return host.get_output_latency(),
        Ok(OpCode::GetCurrentProcessLevel) => {
            // The ABI carries the level as an integer: the typed `ProcessLevel` stops here.
            return host.get_process_level() as isize;
        }
        Ok(OpCode::GetAutomationState) => {
            // The ABI carries the state as an integer: the typed `AutomationState` stops here.
            return host.get_automation_state() as isize;
        }

        // `audioMasterUpdateDisplay`. A plugin fires this after changing preset
        // or program from its own editor: the parameter set the host is showing
        // is now stale and must be re-read. `Host::update_display` was declared
        // with nothing routing to it, so the plugin got the fall-through's `0`
        // and the host's parameter list silently drifted from the plugin's.
        Ok(OpCode::UpdateDisplay) => {
            host.update_display();
            // 1 = handled. A plugin that reads this as "host will not refresh"
            // may fall back to forcing its own repaint.
            return 1;
        }

        // `audioMasterCurrentId`. A shell plugin (a bundle exposing several
        // effects behind one binary) calls this during `VSTPluginMain` to learn
        // which sub-plugin the host wants. The fall-through's `0` means "no
        // particular id", so every shell loaded its default effect — and
        // `Host::get_plugin_id` was overridden to a real value that nothing
        // ever asked for.
        Ok(OpCode::CurrentId) => return host.get_plugin_id() as isize,

        // `audioMasterGetLanguage`. `kVstLangEnglish` is 1 and the values are
        // 1-based, so a `0` is not a language: a plugin indexing a string table
        // by it reads slot 0. English is the honest answer for a host with no
        // localization.
        Ok(OpCode::GetLanguage) => return LANGUAGE_ENGLISH,

        // `audioMasterOpenFileSelector` / `audioMasterCloseFileSelector`. The
        // host has no file dialog; `0` declines, and `canDo` answers No.
        Ok(OpCode::OpenFileSelector | OpCode::CloseFileSelector) => return 0,

        // An opcode the host does not answer: counted by the host, never logged (this runs
        // on the plugin's audio thread).
        _ => host.refused_callback(opcode_number),
    }
    0
}

/// The most bytes `read_string` reads: a plugin's `canDo` query is a short capability name,
/// and the pointer carries no length, so the read is bounded here.
const MAX_QUERY_LEN: usize = 256;

/// Read a string from the `ptr` buffer, up to its first NUL or `MAX_QUERY_LEN` (256) bytes. A
/// null `ptr` reads as the empty string.
///
/// # Safety
///
/// A non-null `ptr` is a NUL-terminated C string, readable up to its NUL for the call.
pub unsafe fn read_string(ptr: *mut c_void) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let ptr = ptr.cast::<u8>().cast_const();
    let mut bytes = Vec::new();
    for at in 0..MAX_QUERY_LEN {
        // SAFETY: `ptr` is non-null (checked above), and the VST2 ABI has every opcode that
        // passes a string send a NUL-terminated C string that the sender keeps valid for the
        // call. Bytes are read one at a time and the read stops at the first NUL, so it
        // never goes past the terminator; a sender that breaks the ABI by sending no NUL
        // within a shorter buffer is the only way to read past it, and no read here can
        // check that.
        let byte = unsafe { ptr.add(at).read() };
        if byte == 0 {
            break;
        }
        bytes.push(byte);
    }
    read_c_str(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// [`super::copy_string`] over the test's own buffers, each at least `max` bytes (or
    /// untouched, for a `max` of 0 and a null `dst`).
    fn copy_string(dst: *mut c_void, src: &str, max: usize) -> isize {
        // SAFETY: the callers pass a null `dst`, a `max` of 0, or a buffer of `max` bytes.
        unsafe { super::copy_string(dst, src, max) }
    }

    /// [`super::host_dispatch`] with the test's own pointers: null, misaligned (never
    /// followed) or absent.
    fn host_dispatch(
        host: &dyn Host,
        effect: *mut AEffect,
        opcode: i32,
        index: i32,
        value: isize,
        ptr: *mut c_void,
        opt: f32,
    ) -> isize {
        // SAFETY: the callers pass null or a misaligned pointer, which the dispatch refuses
        // before reading.
        unsafe { super::host_dispatch(host, effect, opcode, index, value, ptr, opt) }
    }

    /// **`copy_string` writes the string, a NUL, and nothing past it.**
    #[test]
    fn copy_string_stops_at_its_nul_and_always_has_one() {
        let mut dst = [0xAAu8; 16];
        assert_eq!(copy_string(dst.as_mut_ptr().cast(), "hi", 16), 1);
        assert_eq!(&dst[..3], b"hi\0");
        assert!(dst[3..].iter().all(|b| *b == 0xAA), "wrote past the NUL");

        // A string at least `max` long is cut to `max - 1` and terminated.
        let mut dst = [0xAAu8; 16];
        assert_eq!(copy_string(dst.as_mut_ptr().cast(), "abcdefghij", 8), 1);
        assert_eq!(&dst[..8], b"abcdefg\0");
        assert_eq!(dst[8], 0xAA, "wrote past max");

        // Not cut inside a char.
        let mut dst = [0xAAu8; 8];
        copy_string(dst.as_mut_ptr().cast(), "aé", 3);
        assert_eq!(&dst[..2], b"a\0");

        // An interior NUL ends it.
        let mut dst = [0xAAu8; 8];
        copy_string(dst.as_mut_ptr().cast(), "a\0b", 8);
        assert_eq!(&dst[..2], b"a\0");
        assert_eq!(dst[2], 0xAA);
    }

    /// A null destination is not handled (`0`), not written to.
    #[test]
    fn copy_string_to_null_is_not_handled() {
        assert_eq!(copy_string(std::ptr::null_mut(), "x", 8), 0);
        assert_eq!(
            copy_string(std::ptr::NonNull::<u8>::dangling().as_ptr().cast(), "x", 0),
            0
        );
    }

    struct Counting(AtomicUsize);

    impl Host for Counting {
        fn process_plugin_events(&self, _events: api::PluginEvents<'_>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// **A plugin's malformed host calls are answered `0`, not dereferenced.** A
    /// null `VstEvents`, a misaligned one, and a null string buffer.
    #[test]
    fn malformed_host_calls_are_not_followed() {
        let host = Counting(AtomicUsize::new(0));
        let process_events = crate::host::OpCode::ProcessEvents as i32;
        let null: *mut c_void = std::ptr::null_mut();
        let no_effect: *mut AEffect = std::ptr::null_mut();

        assert_eq!(
            host_dispatch(&host, no_effect, process_events, 0, 0, null, 0.0),
            0
        );
        let misaligned = (std::mem::align_of::<api::Events>() as isize + 1) as *mut c_void;
        assert_eq!(
            host_dispatch(&host, no_effect, process_events, 0, 0, misaligned, 0.0),
            0
        );
        assert_eq!(
            host.0.load(Ordering::SeqCst),
            0,
            "a bad pointer was followed"
        );

        for opcode in [
            crate::host::OpCode::GetVendorString as i32,
            crate::host::OpCode::GetProductString as i32,
        ] {
            assert_eq!(host_dispatch(&host, no_effect, opcode, 0, 0, null, 0.0), 0);
        }
    }
}
