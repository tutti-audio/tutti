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

/// The vendor string `audioMasterGetVendorString` answers.
const VENDOR: &str = "Tutti";

/// The product string `audioMasterGetProductString` answers.
const PRODUCT: &str = "Tutti VST2 Host";

/// The vendor version `audioMasterGetVendorVersion` answers: this crate's own version as
/// `major * 10000 + minor * 100 + patch`: minor and patch take two decimal digits each, and
/// tutti's versions stay under 100 in both.
const VENDOR_VERSION: isize = version_part(env!("CARGO_PKG_VERSION_MAJOR")) * 10_000
    + version_part(env!("CARGO_PKG_VERSION_MINOR")) * 100
    + version_part(env!("CARGO_PKG_VERSION_PATCH"));

/// One decimal component of the package version.
const fn version_part(digits: &str) -> isize {
    let bytes = digits.as_bytes();
    let mut value = 0isize;
    let mut i = 0;
    while i < bytes.len() {
        assert!(bytes[i].is_ascii_digit(), "a version component is decimal");
        value = value * 10 + (bytes[i] - b'0') as isize;
        i += 1;
    }
    value
}

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
    use crate::host::AudioMasterOpCode;

    let opcode_number = opcode;
    let opcode = AudioMasterOpCode::try_from(opcode);
    match opcode {
        Ok(AudioMasterOpCode::Version) => return 2400,
        Ok(AudioMasterOpCode::Automate) => host.automate(index, opt),
        Ok(AudioMasterOpCode::BeginEdit) => host.begin_edit(index),
        Ok(AudioMasterOpCode::EndEdit) => host.end_edit(index),

        Ok(AudioMasterOpCode::Idle) => host.idle(),

        // ...
        // `audioMasterCanDo`: 1 = yes, -1 = no, 0 = don't know. A null string
        // is a malformed query and answers "don't know".
        Ok(AudioMasterOpCode::CanDo) => {
            if ptr.is_null() {
                host.refused_callback(opcode_number);
                return 0;
            }
            let mut buf = [0u8; MAX_QUERY_LEN];
            // SAFETY: `ptr` is non-null and the plugin's NUL-terminated query string, valid
            // for the call (this fn's contract).
            let bytes = unsafe { read_bytes(ptr, &mut buf) };
            // A query that is no UTF-8 names no capability the host knows: "don't know".
            let query = std::str::from_utf8(bytes).unwrap_or("");
            return match host.can_do(query) {
                Supported::Yes => 1,
                Supported::Maybe => 0,
                Supported::No => -1,
                Supported::Custom(n) => n,
            };
        }

        Ok(AudioMasterOpCode::GetVendorVersion) => return VENDOR_VERSION,
        Ok(AudioMasterOpCode::GetVendorString) => {
            // SAFETY: the plugin's buffer for the string, of the SDK's size for the opcode.
            return unsafe { copy_string(ptr, VENDOR, MAX_VENDOR_STR_LEN) };
        }
        Ok(AudioMasterOpCode::GetProductString) => {
            // SAFETY: as above.
            return unsafe { copy_string(ptr, PRODUCT, MAX_PRODUCT_STR_LEN) };
        }
        Ok(AudioMasterOpCode::ProcessEvents) => {
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

        Ok(AudioMasterOpCode::GetTime) => {
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
        Ok(AudioMasterOpCode::GetBlockSize) => return host.get_block_size(),

        // `audioMasterGetSampleRate` returns the rate via the intptr result
        // (same channel as `GetBlockSize`), truncated from f32 to isize.
        Ok(AudioMasterOpCode::GetSampleRate) => return host.get_sample_rate() as isize,
        Ok(AudioMasterOpCode::SizeWindow) => return host.size_window(index, value) as isize,
        Ok(AudioMasterOpCode::IOChanged) => return host.io_changed() as isize,
        Ok(AudioMasterOpCode::GetInputLatency) => return host.get_input_latency(),
        Ok(AudioMasterOpCode::GetOutputLatency) => return host.get_output_latency(),
        Ok(AudioMasterOpCode::GetCurrentProcessLevel) => {
            // The ABI carries the level as an integer: the typed `ProcessLevel` stops here.
            return host.get_process_level() as isize;
        }
        Ok(AudioMasterOpCode::GetAutomationState) => {
            // The ABI carries the state as an integer: the typed `AutomationState` stops here.
            return host.get_automation_state() as isize;
        }

        // `audioMasterUpdateDisplay`. A plugin fires this after changing preset
        // or program from its own editor: the parameter set the host is showing
        // is stale and must be re-read: `Host::update_display` tells the host so.
        Ok(AudioMasterOpCode::UpdateDisplay) => {
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
        Ok(AudioMasterOpCode::CurrentId) => return host.get_plugin_id() as isize,

        // `audioMasterGetLanguage`. `kVstLangEnglish` is 1 and the values are
        // 1-based, so a `0` is not a language: a plugin indexing a string table
        // by it reads slot 0. English is the honest answer for a host with no
        // localization.
        Ok(AudioMasterOpCode::GetLanguage) => return LANGUAGE_ENGLISH,

        // `audioMasterOpenFileSelector` / `audioMasterCloseFileSelector`. The
        // host has no file dialog; `0` declines, and `canDo` answers No.
        Ok(AudioMasterOpCode::OpenFileSelector | AudioMasterOpCode::CloseFileSelector) => return 0,

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
    let mut buf = [0u8; MAX_QUERY_LEN];
    // SAFETY: this fn's contract is `read_bytes`'s.
    read_c_str(unsafe { read_bytes(ptr, &mut buf) })
}

/// The bytes of the string at `ptr`, up to its first NUL or `MAX_QUERY_LEN`, in `buf`. Empty
/// for a null `ptr`. Allocates nothing, so the audio thread may call it.
///
/// # Safety
///
/// As [`read_string`].
unsafe fn read_bytes(ptr: *mut c_void, buf: &mut [u8; MAX_QUERY_LEN]) -> &[u8] {
    if ptr.is_null() {
        return &[];
    }
    let ptr = ptr.cast::<u8>().cast_const();
    let mut len = 0;
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
        buf[len] = byte;
        len += 1;
    }
    &buf[..len]
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
        let process_events = crate::host::AudioMasterOpCode::ProcessEvents as i32;
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
            crate::host::AudioMasterOpCode::GetVendorString as i32,
            crate::host::AudioMasterOpCode::GetProductString as i32,
        ] {
            assert_eq!(host_dispatch(&host, no_effect, opcode, 0, 0, null, 0.0), 0);
        }
    }

    /// A host that counts the callbacks it refuses.
    struct Refusing(AtomicUsize);

    impl Host for Refusing {
        fn refused_callback(&self, _opcode: i32) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// **Each callback the host does not answer is reported to the host**: an opcode it does
    /// not implement, a null `VstEvents` and a null `canDo` string.
    #[test]
    fn each_unanswered_callback_is_counted() {
        let host = Refusing(AtomicUsize::new(0));
        let null: *mut c_void = std::ptr::null_mut();
        let no_effect: *mut AEffect = std::ptr::null_mut();
        let opcode = |op: crate::host::AudioMasterOpCode| op as i32;

        assert_eq!(host_dispatch(&host, no_effect, 9999, 0, 0, null, 0.0), 0);
        assert_eq!(host.0.load(Ordering::SeqCst), 1, "an unknown opcode");
        let process_events = opcode(crate::host::AudioMasterOpCode::ProcessEvents);
        host_dispatch(&host, no_effect, process_events, 0, 0, null, 0.0);
        assert_eq!(host.0.load(Ordering::SeqCst), 2, "a null event list");
        let can_do = opcode(crate::host::AudioMasterOpCode::CanDo);
        host_dispatch(&host, no_effect, can_do, 0, 0, null, 0.0);
        assert_eq!(host.0.load(Ordering::SeqCst), 3, "a null canDo string");

        // An answered callback is not counted.
        let version = opcode(crate::host::AudioMasterOpCode::Version);
        assert_eq!(
            host_dispatch(&host, no_effect, version, 0, 0, null, 0.0),
            2400
        );
        assert_eq!(host.0.load(Ordering::SeqCst), 3);
    }

    struct Answering;

    impl Host for Answering {
        fn can_do(&self, can_do: &str) -> Supported {
            if can_do == "sendVstEvents" {
                Supported::Yes
            } else {
                Supported::Maybe
            }
        }
        fn get_time_info(&self, _mask: i32) -> Option<TimeInfo> {
            Some(TimeInfo::default())
        }
        fn process_plugin_events(&self, events: api::PluginEvents<'_>) {
            assert_eq!(events.iter().count(), 0);
        }
    }

    /// **The callbacks a plugin makes from its audio thread allocate nothing**: the `canDo`
    /// query is read into the stack, the vendor and product strings are constants, and the event list and time info are views.
    #[test]
    fn the_audio_thread_callbacks_allocate_nothing() {
        use crate::host::AudioMasterOpCode;
        let no_effect: *mut AEffect = std::ptr::null_mut();
        let null: *mut c_void = std::ptr::null_mut();
        let mut query = *b"sendVstEvents\0";
        let mut strings = [0u8; 64];
        let mut events = api::Events {
            num_events: 0,
            _reserved: 0,
            events: [std::ptr::null_mut(); 2],
        };
        let events_ptr: *mut c_void = (&raw mut events).cast();
        let query_ptr: *mut c_void = query.as_mut_ptr().cast();
        let strings_ptr: *mut c_void = strings.as_mut_ptr().cast();
        let calls: [(AudioMasterOpCode, *mut c_void); 20] = [
            (AudioMasterOpCode::Automate, null),
            (AudioMasterOpCode::Version, null),
            (AudioMasterOpCode::CurrentId, null),
            (AudioMasterOpCode::Idle, null),
            (AudioMasterOpCode::GetTime, null),
            (AudioMasterOpCode::ProcessEvents, events_ptr),
            (AudioMasterOpCode::IOChanged, null),
            (AudioMasterOpCode::SizeWindow, null),
            (AudioMasterOpCode::GetSampleRate, null),
            (AudioMasterOpCode::GetBlockSize, null),
            (AudioMasterOpCode::GetInputLatency, null),
            (AudioMasterOpCode::GetOutputLatency, null),
            (AudioMasterOpCode::GetCurrentProcessLevel, null),
            (AudioMasterOpCode::GetAutomationState, null),
            (AudioMasterOpCode::UpdateDisplay, null),
            (AudioMasterOpCode::GetLanguage, null),
            (AudioMasterOpCode::CanDo, query_ptr),
            (AudioMasterOpCode::GetVendorString, strings_ptr),
            (AudioMasterOpCode::GetProductString, strings_ptr),
            (AudioMasterOpCode::GetVendorVersion, null),
        ];
        let answered = std::cell::Cell::new(0);
        assert_no_alloc::assert_no_alloc(|| {
            for (op, ptr) in calls {
                // SAFETY: `ptr` is null or the test's own buffer of the size the opcode needs.
                let _ = unsafe {
                    super::host_dispatch(&Answering, no_effect, op as i32, 0, 0, ptr, 0.0)
                };
                answered.set(answered.get() + 1);
            }
            // An opcode the host does not implement is refused without allocating too.
            // SAFETY: a null pointer is never followed.
            let _ = unsafe { super::host_dispatch(&Answering, no_effect, 9999, 0, 0, null, 0.0) };
        });
        assert_eq!(answered.get(), calls.len());
        assert_eq!(
            host_dispatch(
                &Answering,
                no_effect,
                AudioMasterOpCode::CanDo as i32,
                0,
                0,
                query_ptr,
                0.0
            ),
            1,
            "the query read off the stack is answered"
        );
        assert_eq!(&strings[..15], b"Tutti VST2 Host", "the last string copied");
    }

    /// **The host identifies itself as Tutti at this crate's version**: the vendor version
    /// is `major * 10000 + minor * 100 + patch` of the package version.
    #[test]
    fn the_host_answers_its_own_name_and_package_version() {
        use crate::host::AudioMasterOpCode;
        let no_effect: *mut AEffect = std::ptr::null_mut();
        let mut vendor = [0u8; 64];
        let vendor_ptr: *mut c_void = vendor.as_mut_ptr().cast();
        assert_eq!(
            host_dispatch(
                &Answering,
                no_effect,
                AudioMasterOpCode::GetVendorString as i32,
                0,
                0,
                vendor_ptr,
                0.0
            ),
            1
        );
        assert_eq!(&vendor[..6], b"Tutti\0");

        let mut parts = env!("CARGO_PKG_VERSION")
            .split('.')
            .map(|part| part.parse::<isize>().unwrap());
        let (major, minor, patch) = (
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap(),
        );
        assert_eq!(
            host_dispatch(
                &Answering,
                no_effect,
                AudioMasterOpCode::GetVendorVersion as i32,
                0,
                0,
                std::ptr::null_mut(),
                0.0
            ),
            major * 10_000 + minor * 100 + patch
        );
    }
}
